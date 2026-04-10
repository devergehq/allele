use super::grid_element::TerminalGridElement;
use super::pty_terminal::{PtyTerminal, ShellCommand, TermSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use gpui::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FONT_FAMILY: &str = "JetBrains Mono";
const FONT_SIZE: f32 = 13.0;
const MIN_FONT_SIZE: f32 = 8.0;
const MAX_FONT_SIZE: f32 = 32.0;
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;

/// Events emitted by the terminal view to the parent
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    NewSession,
    CloseSession,
    SwitchSession(usize), // 0-indexed
}

impl EventEmitter<TerminalEvent> for TerminalView {}

/// GPUI View wrapping a PTY-backed terminal
pub struct TerminalView {
    terminal: Option<PtyTerminal>,
    error: Option<String>,
    last_cols: u16,
    last_rows: u16,
    pub focus_handle: FocusHandle,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    scroll_dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Element bounds — written by grid_element during paint, read by mouse handlers.
    // Stored as atomic i32 pixels for lock-free access.
    element_origin_x: std::sync::Arc<std::sync::atomic::AtomicI32>,
    element_origin_y: std::sync::Arc<std::sync::atomic::AtomicI32>,
    scrollbar_dragging: bool,
    // Cursor blink
    cursor_visible: bool,
    last_keypress: Instant,
    last_blink_toggle: Instant,
    // Scrollbar fade
    scrollbar_opacity: f32,
    last_scroll_time: Instant,
    // Selection
    // Selection stored in alacritty Line coordinates so it scrolls with content.
    // (line_offset, col) where line_offset < 0 = history, >= 0 = current screen.
    selection_anchor: Option<(i32, usize)>,
    selection_extent: Option<(i32, usize)>,
    selecting: bool,
    // Search
    search_active: bool,
    search_query: String,
    search_matches: Vec<(i32, usize, usize)>, // (line_offset, col_start, col_end) — line_offset is alacritty Line value
    search_current_idx: usize,
    // URL detection
    hovered_url: Option<(usize, usize, usize, String)>, // (row, col_start, col_end, url)
    // Bell flash state
    bell_flash_start: Option<Instant>,
    // FPS tracking
    frame_count: u32,
    last_fps_time: Instant,
    pub current_fps: u32,
}

impl TerminalView {
    /// Create a terminal view running a specific command, or default shell if None
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        command: Option<ShellCommand>,
        working_dir: Option<PathBuf>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // Measure cell dimensions from the actual font via GPUI's text system.
        // This value is used both for PTY resize (column/row count) and for
        // the grid element's rendering, ensuring they agree.
        let font = Font {
            family: FONT_FAMILY.into(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
        };
        let (measured_w, measured_h) =
            TerminalGridElement::measure_cell(window, &font, px(FONT_SIZE));
        let cell_width = f32::from(measured_w);
        let cell_height = f32::from(measured_h);

        let terminal = match PtyTerminal::spawn(TermSize::default(), command, working_dir) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("Failed to create PTY: {e}");
                return Self {
                    terminal: None,
                    error: Some(format!("Failed to create PTY: {e}")),
                    last_cols: 80,
                    last_rows: 24,
                    focus_handle,
                    font_size: FONT_SIZE,
                    cell_width,
                    cell_height,
                    scroll_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    element_origin_x: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
                    element_origin_y: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
                    scrollbar_dragging: false,
                    cursor_visible: true,
                    last_keypress: Instant::now(),
                    last_blink_toggle: Instant::now(),
                    scrollbar_opacity: 0.0,
                    last_scroll_time: Instant::now() - Duration::from_secs(10),
                    selection_anchor: None,
                    selection_extent: None,
                    selecting: false,
                    search_active: false,
                    search_query: String::new(),
                    search_matches: Vec::new(),
                    search_current_idx: 0,
                    hovered_url: None,
                    bell_flash_start: None,
                    frame_count: 0,
                    last_fps_time: Instant::now(),
                    current_fps: 0,
                };
            }
        };

        // Auto-focus this terminal on creation
        focus_handle.focus(window, cx);

        // Poll for PTY events on a timer and re-render
        cx.spawn_in(window, async |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;

                let should_redraw = this
                    .update(cx, |this: &mut Self, _cx: &mut Context<Self>| {
                        let (pty_events, bell) = if let Some(ref mut terminal) = this.terminal {
                            let had = terminal.drain_events();
                            let bell = terminal.bell_pending;
                            terminal.bell_pending = false;
                            (had, bell)
                        } else {
                            (false, false)
                        };

                        if bell {
                            this.bell_flash_start = Some(Instant::now());
                        }

                        // Clear bell flash after 200ms
                        let bell_expired = if let Some(start) = this.bell_flash_start {
                            if start.elapsed() > Duration::from_millis(200) {
                                this.bell_flash_start = None;
                                true
                            } else {
                                false
                            }
                        } else { false };
                        let bell_active = this.bell_flash_start.is_some();
                        let _ = bell_active;
                        let _ = bell_expired;
                        // Also check if scroll happened
                        let scrolled = this.scroll_dirty.swap(false, std::sync::atomic::Ordering::Relaxed);

                        // Update scroll timestamp when scroll detected
                        if scrolled {
                            this.last_scroll_time = Instant::now();
                        }

                        // Cursor blink: toggle every 500ms, but only if idle (no keypress in 500ms)
                        let now = Instant::now();
                        let idle = now.duration_since(this.last_keypress) > Duration::from_millis(500);
                        let mut blink_changed = false;
                        if idle && now.duration_since(this.last_blink_toggle) >= Duration::from_millis(500) {
                            this.cursor_visible = !this.cursor_visible;
                            this.last_blink_toggle = now;
                            blink_changed = true;
                        } else if !idle && !this.cursor_visible {
                            this.cursor_visible = true;
                            blink_changed = true;
                        }

                        // Scrollbar fade: fade in on scroll, fade out after 1.5s
                        let scroll_age = now.duration_since(this.last_scroll_time).as_secs_f32();
                        let target_opacity = if this.scrollbar_dragging || scroll_age < 1.5 {
                            1.0
                        } else if scroll_age < 2.5 {
                            // Fade out over 1 second
                            1.0 - (scroll_age - 1.5)
                        } else {
                            0.0
                        };
                        let mut opacity_changed = false;
                        if (this.scrollbar_opacity - target_opacity).abs() > 0.01 {
                            this.scrollbar_opacity = target_opacity;
                            opacity_changed = true;
                        }

                        pty_events || scrolled || blink_changed || opacity_changed || bell || bell_expired
                    })
                    .unwrap_or(false);

                if should_redraw {
                    this.update(cx, |_this: &mut Self, cx: &mut Context<Self>| {
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();

        // Observe window bounds changes for resize.
        // Uses the actual terminal element origin (set by grid_element during
        // paint) when available, falling back to a sensible estimate on first run.
        cx.observe_window_bounds(window, |this: &mut Self, window, _cx| {
            let viewport = window.viewport_size();
            let origin_x = this.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
            let origin_y = this.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;

            // Use actual origin when painted at least once, else fall back to default
            let sidebar_estimate = if origin_x > 0.0 { origin_x } else { 240.0 };
            let available_width = f32::from(viewport.width) - sidebar_estimate;
            let available_height = f32::from(viewport.height) - origin_y;

            if available_width > 100.0 && available_height > 100.0 {
                let new_size = Self::compute_size(
                    available_width,
                    available_height,
                    this.cell_width,
                    this.cell_height,
                );
                if new_size.cols != this.last_cols || new_size.rows != this.last_rows {
                    this.last_cols = new_size.cols;
                    this.last_rows = new_size.rows;
                    if let Some(ref mut terminal) = this.terminal {
                        terminal.resize(new_size);
                    }
                }
            }
        })
        .detach();


        Self {
            terminal,
            error: None,
            last_cols: 80,
            last_rows: 24,
            focus_handle,
            font_size: FONT_SIZE,
            cell_width,
            cell_height,
            scroll_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            element_origin_x: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
            element_origin_y: std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0)),
            scrollbar_dragging: false,
            cursor_visible: true,
            last_keypress: Instant::now(),
            last_blink_toggle: Instant::now(),
            scrollbar_opacity: 0.0,
            last_scroll_time: Instant::now() - Duration::from_secs(10),
            selection_anchor: None,
            selection_extent: None,
            selecting: false,
            search_active: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_current_idx: 0,
            hovered_url: None,
            bell_flash_start: None,
            frame_count: 0,
            last_fps_time: Instant::now(),
            current_fps: 0,
        }
    }

    /// Focus this terminal view
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.focus(window, cx);
    }

    /// Recompute cell dimensions from current font_size.
    /// Called after font size changes (Cmd+= / Cmd+-).
    fn remeasure_cells(&mut self, window: &mut Window) {
        let font = Font {
            family: FONT_FAMILY.into(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
        };
        let (w, h) = TerminalGridElement::measure_cell(window, &font, px(self.font_size));
        self.cell_width = f32::from(w);
        self.cell_height = f32::from(h);
    }

    /// Check if the PTY process has exited
    pub fn has_exited(&self) -> bool {
        self.terminal.as_ref().map_or(true, |t| t.exited)
    }

    /// Get the current terminal title set by the shell via OSC sequences.
    pub fn title(&self) -> Option<String> {
        self.terminal.as_ref().and_then(|t| t.title.clone())
    }

    /// Convert window-relative pixel position to visible screen cell (row, col).
    /// row is the visible row index (0..screen_lines), col is the column.
    fn pixel_to_cell(&self, x: f32, y: f32) -> (usize, usize) {
        let origin_x = self.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
        let origin_y = self.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
        let local_x = (x - origin_x).max(0.0);
        let local_y = (y - origin_y).max(0.0);
        let col = (local_x / self.cell_width).floor() as usize;
        let row = (local_y / self.cell_height).floor() as usize;
        (row, col)
    }

    /// Convert window-relative pixel position to an alacritty line-coordinate cell.
    /// Returns (line_offset, col) where line_offset is negative for history,
    /// stable across scroll events. Use this for selection anchors.
    fn pixel_to_line_cell(&self, x: f32, y: f32) -> (i32, usize) {
        let (row, col) = self.pixel_to_cell(x, y);
        let display_offset = self
            .terminal
            .as_ref()
            .map(|t| t.term.lock().grid().display_offset() as i32)
            .unwrap_or(0);
        // line_offset = visible_row - display_offset
        // (Same formula the renderer uses: grid_line = line_idx - display_offset)
        let line_offset = row as i32 - display_offset;
        (line_offset, col)
    }

    /// Get selected text from the terminal grid using alacritty line coordinates.
    fn get_selected_text(&self) -> Option<String> {
        let (anchor, extent) = match (self.selection_anchor, self.selection_extent) {
            (Some(a), Some(e)) => (a, e),
            _ => return None,
        };
        let terminal = self.terminal.as_ref()?;
        let term = terminal.term.lock();
        let grid = term.grid();
        let num_cols = grid.columns();

        // Normalise: start <= end
        let (start_line, start_col, end_line, end_col) = if anchor.0 < extent.0
            || (anchor.0 == extent.0 && anchor.1 <= extent.1)
        {
            (anchor.0, anchor.1, extent.0, extent.1)
        } else {
            (extent.0, extent.1, anchor.0, anchor.1)
        };

        let mut text = String::new();
        for line in start_line..=end_line {
            let grid_line = Line(line);
            let c_start = if line == start_line { start_col } else { 0 };
            let c_end = if line == end_line { end_col + 1 } else { num_cols };

            for col in c_start..c_end.min(num_cols) {
                let cell = &grid[grid_line][Column(col)];
                if !cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                    let ch = if cell.c == '\0' { ' ' } else { cell.c };
                    text.push(ch);
                }
            }
            if line < end_line {
                let trimmed = text.trim_end_matches(' ');
                text.truncate(trimmed.len());
                text.push('\n');
            }
        }
        let trimmed = text.trim_end_matches(' ');
        Some(trimmed.to_string())
    }

    /// Find URL at the given grid cell, if any.
    /// First checks for OSC 8 hyperlinks (cell.hyperlink()), then falls back
    /// to char-indexed regex-style detection of http(s):// URLs.
    fn url_at(&self, cell: (usize, usize)) -> Option<(usize, usize, usize, String)> {
        let terminal = self.terminal.as_ref()?;
        let term = terminal.term.lock();
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let num_cols = grid.columns();
        let grid_line = Line(cell.0 as i32 - display_offset);

        // OSC 8 hyperlink check: if the hovered cell has a hyperlink attribute,
        // expand to the full contiguous range of cells with the same hyperlink URI.
        if cell.1 < num_cols {
            if let Some(hyperlink) = grid[grid_line][Column(cell.1)].hyperlink() {
                let target_uri = hyperlink.uri().to_string();
                let mut start_col = cell.1;
                while start_col > 0 {
                    let prev = &grid[grid_line][Column(start_col - 1)];
                    match prev.hyperlink() {
                        Some(h) if h.uri() == target_uri => start_col -= 1,
                        _ => break,
                    }
                }
                let mut end_col = cell.1;
                while end_col + 1 < num_cols {
                    let next = &grid[grid_line][Column(end_col + 1)];
                    match next.hyperlink() {
                        Some(h) if h.uri() == target_uri => end_col += 1,
                        _ => break,
                    }
                }
                return Some((cell.0, start_col, end_col, target_uri));
            }
        }

        // Build parallel arrays of (char, column) skipping wide-char spacers.
        let mut line_chars: Vec<char> = Vec::with_capacity(num_cols);
        let mut line_cols: Vec<usize> = Vec::with_capacity(num_cols);
        for col in 0..num_cols {
            let c = &grid[grid_line][Column(col)];
            if c.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let ch = if c.c == '\0' { ' ' } else { c.c };
            line_chars.push(ch);
            line_cols.push(col);
        }

        // Find URL start positions (as char indices) matching "http://" or "https://"
        let scheme_http: Vec<char> = "http://".chars().collect();
        let scheme_https: Vec<char> = "https://".chars().collect();

        let find_scheme = |text: &[char], scheme: &[char]| -> Vec<usize> {
            let mut results = Vec::new();
            if text.len() >= scheme.len() {
                for i in 0..=(text.len() - scheme.len()) {
                    if text[i..i + scheme.len()] == scheme[..] {
                        results.push(i);
                    }
                }
            }
            results
        };

        let mut starts = find_scheme(&line_chars, &scheme_http);
        starts.extend(find_scheme(&line_chars, &scheme_https));

        for start_idx in starts {
            // Find end of URL: scan forward until whitespace or terminator
            let mut end_idx = start_idx;
            while end_idx < line_chars.len() {
                let ch = line_chars[end_idx];
                if ch.is_whitespace() || matches!(ch, ')' | ']' | '>' | '"' | '\'') {
                    break;
                }
                end_idx += 1;
            }
            if end_idx == start_idx { continue; }

            // Trim trailing punctuation
            while end_idx > start_idx && matches!(line_chars[end_idx - 1], '.' | ',' | ';' | ':' | '!' | '?') {
                end_idx -= 1;
            }
            if end_idx == start_idx { continue; }

            let col_start = line_cols[start_idx];
            let col_end = line_cols[end_idx - 1];

            // Is the hovered cell within this URL's column range?
            if cell.1 >= col_start && cell.1 <= col_end {
                let url: String = line_chars[start_idx..end_idx].iter().collect();
                return Some((cell.0, col_start, col_end, url));
            }
        }

        None
    }

    /// Update search matches by scanning visible grid for query string.
    ///
    /// Handles multi-byte UTF-8 (bullets, emoji, CJK) correctly by tracking
    /// column positions in a parallel array and searching on char arrays,
    /// NOT byte indices. Skips wide character spacers to match renderer.
    fn update_search_matches(&mut self) {
        self.search_matches.clear();
        self.search_current_idx = 0;

        if self.search_query.is_empty() { return; }

        let Some(ref terminal) = self.terminal else { return };
        let term = terminal.term.lock();
        let grid = term.grid();
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        let history_size = grid.total_lines().saturating_sub(num_lines);

        let query_chars: Vec<char> = self.search_query.to_lowercase().chars().collect();
        if query_chars.is_empty() { return; }

        let min_line = -(history_size as i32);
        let max_line = num_lines as i32 - 1;

        for line_offset in min_line..=max_line {
            let grid_line = Line(line_offset);

            // Build parallel arrays: chars (lowercased) and their column positions.
            // Skip wide-char spacers so positions match what the renderer shows.
            let mut line_chars: Vec<char> = Vec::with_capacity(num_cols);
            let mut line_cols: Vec<usize> = Vec::with_capacity(num_cols);

            for col in 0..num_cols {
                let cell = &grid[grid_line][Column(col)];
                if cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                // Lowercase for case-insensitive match — use first char of lowercase
                // (most lowercase operations don't change char count)
                for lc in ch.to_lowercase() {
                    line_chars.push(lc);
                    line_cols.push(col);
                }
            }

            // Sliding window search over chars
            if line_chars.len() >= query_chars.len() {
                let mut i = 0;
                while i + query_chars.len() <= line_chars.len() {
                    if line_chars[i..i + query_chars.len()] == query_chars[..] {
                        let col_start = line_cols[i];
                        let col_end = line_cols[i + query_chars.len() - 1];
                        self.search_matches.push((line_offset, col_start, col_end));
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }

    /// Find word boundaries at the given line-coordinate cell.
    /// Returns (start, end) as (line_offset, col) pairs.
    fn word_at_line(&self, cell: (i32, usize)) -> Option<((i32, usize), (i32, usize))> {
        let terminal = self.terminal.as_ref()?;
        let term = terminal.term.lock();
        let grid = term.grid();
        let num_cols = grid.columns();
        let grid_line = Line(cell.0);

        let is_word_char = |c: char| -> bool {
            c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/'
        };

        if cell.1 >= num_cols { return None; }
        let ch = grid[grid_line][Column(cell.1)].c;
        if !is_word_char(ch) {
            return None;
        }

        let mut start_col = cell.1;
        while start_col > 0 {
            let prev_cell = &grid[grid_line][Column(start_col - 1)];
            if prev_cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                if start_col >= 2 { start_col -= 1; continue; }
                break;
            }
            if !is_word_char(prev_cell.c) { break; }
            start_col -= 1;
        }

        let mut end_col = cell.1;
        while end_col + 1 < num_cols {
            let next_cell = &grid[grid_line][Column(end_col + 1)];
            if next_cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                end_col += 1;
                continue;
            }
            if !is_word_char(next_cell.c) { break; }
            end_col += 1;
        }

        Some(((cell.0, start_col), (cell.0, end_col)))
    }

    /// Find word boundaries at the given grid cell (legacy, used by non-selection callers).
    #[allow(dead_code)]
    fn word_at(&self, cell: (usize, usize)) -> Option<((usize, usize), (usize, usize))> {
        let terminal = self.terminal.as_ref()?;
        let term = terminal.term.lock();
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let num_cols = grid.columns();
        let grid_line = Line(cell.0 as i32 - display_offset);

        let is_word_char = |c: char| -> bool {
            c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/'
        };

        if cell.1 >= num_cols { return None; }
        let ch = grid[grid_line][Column(cell.1)].c;
        if !is_word_char(ch) {
            return None;
        }

        // Scan left (skipping wide-char spacers)
        let mut start_col = cell.1;
        while start_col > 0 {
            let prev_cell = &grid[grid_line][Column(start_col - 1)];
            if prev_cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                if start_col >= 2 {
                    start_col -= 1;
                    continue;
                }
                break;
            }
            if !is_word_char(prev_cell.c) { break; }
            start_col -= 1;
        }

        // Scan right
        let mut end_col = cell.1;
        while end_col + 1 < num_cols {
            let next_cell = &grid[grid_line][Column(end_col + 1)];
            if next_cell.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                end_col += 1;
                continue;
            }
            if !is_word_char(next_cell.c) { break; }
            end_col += 1;
        }

        Some(((cell.0, start_col), (cell.0, end_col)))
    }

    /// Compute terminal grid size from pixel dimensions
    fn compute_size(width_px: f32, height_px: f32, cell_w: f32, cell_h: f32) -> TermSize {
        let cols = (width_px / cell_w).floor() as u16;
        let rows = (height_px / cell_h).floor() as u16;
        TermSize {
            cols: cols.max(MIN_COLS),
            rows: rows.max(MIN_ROWS),
            cell_width: cell_w as u16,
            cell_height: cell_h as u16,
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Opportunistic PTY resize: if the visible terminal area changed since
        // last render (e.g. sidebar was dragged), update the PTY size.
        {
            let viewport = window.viewport_size();
            let origin_x = self.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
            let origin_y = self.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
            if origin_x > 0.0 {
                let available_width = f32::from(viewport.width) - origin_x;
                let available_height = f32::from(viewport.height) - origin_y;
                if available_width > 100.0 && available_height > 100.0 {
                    let new_size = Self::compute_size(
                        available_width, available_height,
                        self.cell_width, self.cell_height,
                    );
                    if new_size.cols != self.last_cols || new_size.rows != self.last_rows {
                        self.last_cols = new_size.cols;
                        self.last_rows = new_size.rows;
                        if let Some(ref mut terminal) = self.terminal {
                            terminal.resize(new_size);
                        }
                    }
                }
            }
        }

        // FPS tracking
        self.frame_count += 1;
        let elapsed = self.last_fps_time.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.current_fps = self.frame_count;
            self.frame_count = 0;
            self.last_fps_time = Instant::now();
        }

        if let Some(ref error) = self.error {
            return div()
                .size_full()
                .bg(rgb(0x1e1e2e))
                .text_color(rgb(0xf38ba8))
                .child(div().p(px(12.0)).child(error.clone()))
                .into_any_element();
        }

        let Some(ref terminal) = self.terminal else {
            return div()
                .size_full()
                .bg(rgb(0x1e1e2e))
                .child("No terminal")
                .into_any_element();
        };

        let font = Font {
            family: FONT_FAMILY.into(),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
        };

        let selection = match (self.selection_anchor, self.selection_extent) {
            (Some(a), Some(e)) => Some(super::grid_element::GridSelection { anchor: a, extent: e }),
            _ => None,
        };

        let grid_element = TerminalGridElement::new(
            terminal.term.clone(),
            font,
            px(self.font_size),
            px(self.cell_width),
            px(self.cell_height),
        )
        .cursor_visible(self.cursor_visible)
        .scrollbar_opacity(self.scrollbar_opacity)
        .selection(selection)
        .search_matches(self.search_matches.clone(), self.search_current_idx)
        .hovered_url(self.hovered_url.as_ref().map(|(r, cs, ce, _)| (*r, *cs, *ce)))
        .origin_out(self.element_origin_x.clone(), self.element_origin_y.clone());

        let bell_active = self.bell_flash_start.is_some();
        let bg_color = if bell_active { rgb(0x3a2e3a) } else { rgb(0x1e1e2e) };

        div()
            .id("terminal")
            .size_full()
            .bg(bg_color)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, window, cx| {
                this.last_keypress = Instant::now();
                this.cursor_visible = true;

                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;

                // Handle search mode input
                if this.search_active {
                    match key {
                        "escape" => {
                            this.search_active = false;
                            this.search_query.clear();
                            this.search_matches.clear();
                            cx.notify();
                            return;
                        }
                        "enter" => {
                            // Next match
                            if !this.search_matches.is_empty() {
                                this.search_current_idx =
                                    (this.search_current_idx + 1) % this.search_matches.len();
                                cx.notify();
                            }
                            return;
                        }
                        "backspace" => {
                            this.search_query.pop();
                            this.update_search_matches();
                            cx.notify();
                            return;
                        }
                        _ => {
                            if let Some(ref key_char) = event.keystroke.key_char {
                                if !mods.control && !mods.platform {
                                    this.search_query.push_str(key_char);
                                    this.update_search_matches();
                                    cx.notify();
                                    return;
                                }
                            }
                            // Shift+Enter = previous match
                            if key == "enter" && mods.shift && !this.search_matches.is_empty() {
                                this.search_current_idx = if this.search_current_idx == 0 {
                                    this.search_matches.len() - 1
                                } else {
                                    this.search_current_idx - 1
                                };
                                cx.notify();
                                return;
                            }
                        }
                    }
                    return;
                }

                // Handle Cmd shortcuts (emit to parent)
                if mods.platform {
                    match key {
                        "v" => {
                            // Cmd+V: paste clipboard contents
                            if let Some(ref terminal) = this.terminal {
                                if let Some(item) = cx.read_from_clipboard() {
                                    if let Some(text) = item.text() {
                                        // Check if terminal is in bracketed paste mode
                                        let use_bracketed = terminal.term.lock().mode()
                                            .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE);
                                        if use_bracketed {
                                            terminal.write(b"\x1b[200~");
                                            terminal.write(text.as_bytes());
                                            terminal.write(b"\x1b[201~");
                                        } else {
                                            terminal.write(text.as_bytes());
                                        }
                                    }
                                }
                            }
                            return;
                        }
                        "f" => {
                            // Cmd+F: open search
                            this.search_active = true;
                            this.search_query.clear();
                            this.search_matches.clear();
                            this.search_current_idx = 0;
                            cx.notify();
                            return;
                        }
                        "=" | "+" => {
                            // Cmd+= / Cmd++: zoom in
                            let new_size = (this.font_size + 1.0).min(MAX_FONT_SIZE);
                            if new_size != this.font_size {
                                this.font_size = new_size;
                                this.remeasure_cells(window);
                                cx.notify();
                            }
                            return;
                        }
                        "-" | "_" => {
                            // Cmd+-: zoom out
                            let new_size = (this.font_size - 1.0).max(MIN_FONT_SIZE);
                            if new_size != this.font_size {
                                this.font_size = new_size;
                                this.remeasure_cells(window);
                                cx.notify();
                            }
                            return;
                        }
                        "0" => {
                            // Cmd+0: reset font size
                            if this.font_size != FONT_SIZE {
                                this.font_size = FONT_SIZE;
                                this.remeasure_cells(window);
                                cx.notify();
                            }
                            return;
                        }
                        "g" => {
                            // Cmd+G: next match (Cmd+Shift+G: previous)
                            if !this.search_matches.is_empty() {
                                if mods.shift {
                                    this.search_current_idx = if this.search_current_idx == 0 {
                                        this.search_matches.len() - 1
                                    } else {
                                        this.search_current_idx - 1
                                    };
                                } else {
                                    this.search_current_idx =
                                        (this.search_current_idx + 1) % this.search_matches.len();
                                }
                                cx.notify();
                            }
                            return;
                        }
                        "c" => {
                            // Cmd+C: copy selection if any, else send Ctrl+C
                            if let Some(text) = this.get_selected_text() {
                                if !text.is_empty() {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                    this.selection_anchor = None;
                                    this.selection_extent = None;
                                    cx.notify();
                                    return;
                                }
                            }
                            // No selection — send Ctrl+C to terminal
                            if let Some(ref terminal) = this.terminal {
                                terminal.write(&[0x03]);
                            }
                            return;
                        }
                        "backspace" => {
                            // Cmd+Backspace = macOS "delete to start of line".
                            // Map to Ctrl+U (0x15), which readline (and
                            // Claude Code's input editor) treats as
                            // backward-kill-line.
                            if let Some(ref terminal) = this.terminal {
                                terminal.write(&[0x15]);
                            }
                            return;
                        }
                        "n" => { cx.emit(TerminalEvent::NewSession); return; }
                        "w" => { cx.emit(TerminalEvent::CloseSession); return; }
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                            if let Ok(num) = key.parse::<usize>() {
                                cx.emit(TerminalEvent::SwitchSession(num - 1));
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                let Some(ref terminal) = this.terminal else { return };

                // Clear selection on any input to terminal
                if this.selection_anchor.is_some() {
                    this.selection_anchor = None;
                    this.selection_extent = None;
                }

                // Snap to bottom when scrolled back
                {
                    let t = terminal.term.lock();
                    let offset = t.grid().display_offset();
                    drop(t);
                    if offset > 0 {
                        terminal.term.lock().scroll_display(Scroll::Bottom);
                        this.scroll_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                // Handle control key combos
                if mods.control {
                    let ctrl_byte = match key {
                        "a" => Some(0x01), "b" => Some(0x02), "c" => Some(0x03),
                        "d" => Some(0x04), "e" => Some(0x05), "f" => Some(0x06),
                        "g" => Some(0x07), "h" => Some(0x08), "k" => Some(0x0b),
                        "l" => Some(0x0c), "n" => Some(0x0e), "o" => Some(0x0f),
                        "p" => Some(0x10), "r" => Some(0x12), "t" => Some(0x14),
                        "u" => Some(0x15), "w" => Some(0x17), "z" => Some(0x1a),
                        _ => None,
                    };
                    if let Some(byte) = ctrl_byte {
                        terminal.write(&[byte]);
                        return;
                    }
                }

                // Opt+Backspace = macOS "delete previous word".
                // Send ESC+DEL (Meta-Backspace), which readline and Claude
                // Code's input editor interpret as backward-kill-word.
                // Must be checked BEFORE the plain-backspace branch below.
                if mods.alt && key == "backspace" {
                    terminal.write(b"\x1b\x7f");
                    return;
                }

                // Handle special keys BEFORE key_char — enter, backspace, etc.
                let special_bytes: Option<&[u8]> = match key {
                    "enter" => Some(b"\r"),
                    "backspace" => Some(b"\x7f"),
                    "tab" => Some(b"\t"),
                    "escape" => Some(b"\x1b"),
                    "up" => Some(b"\x1b[A"),
                    "down" => Some(b"\x1b[B"),
                    "right" => Some(b"\x1b[C"),
                    "left" => Some(b"\x1b[D"),
                    "home" => Some(b"\x1b[H"),
                    "end" => Some(b"\x1b[F"),
                    "pageup" => Some(b"\x1b[5~"),
                    "pagedown" => Some(b"\x1b[6~"),
                    "delete" => Some(b"\x1b[3~"),
                    "space" => Some(b" "),
                    _ => None,
                };

                if let Some(bytes) = special_bytes {
                    terminal.write(bytes);
                    return;
                }

                // For regular character input, use key_char
                if let Some(ref key_char) = event.keystroke.key_char {
                    terminal.write(key_char.as_bytes());
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, event: &MouseDownEvent, window, cx| {
                    let viewport = window.viewport_size();
                    let click_x = f32::from(event.position.x);
                    let click_y = f32::from(event.position.y);
                    // Scrollbar is at the right edge of the terminal element
                    let origin_x = this.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
                    let term_width = f32::from(viewport.width) - origin_x;
                    let scrollbar_zone = origin_x + term_width - 12.0;

                    // Cmd+click to open URLs
                    if event.modifiers.platform {
                        let cell = this.pixel_to_cell(click_x, click_y);
                        if let Some((_, _, _, url)) = this.url_at(cell) {
                            let _ = std::process::Command::new("open").arg(&url).spawn();
                            return;
                        }
                    }

                    // Clear any existing selection on a non-shift click
                    if !event.modifiers.shift && this.selection_anchor.is_some() {
                        this.selection_anchor = None;
                        this.selection_extent = None;
                    }

                    if click_x >= scrollbar_zone {
                        // Scrollbar interaction
                        let Some(ref terminal) = this.terminal else { return };
                        let t = terminal.term.lock();
                        let grid = t.grid();
                        let total = grid.total_lines();
                        let screen = grid.screen_lines();
                        let history = total.saturating_sub(screen);
                        drop(t);

                        if history > 0 {
                            this.scrollbar_dragging = true;
                            // Set scroll position from click y relative to terminal element
                            let origin_y = this.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
                            let term_h = f32::from(viewport.height) - origin_y;
                            let local_y = (click_y - origin_y).max(0.0);
                            let fraction = (local_y / term_h).clamp(0.0, 1.0);
                            // fraction 0 = top (max offset), fraction 1 = bottom (offset 0)
                            let new_offset = ((1.0 - fraction) * history as f32).round() as i32;
                            let current = terminal.term.lock().grid().display_offset() as i32;
                            let delta = new_offset - current;
                            if delta != 0 {
                                terminal.term.lock().scroll_display(Scroll::Delta(delta));
                                this.scroll_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                                cx.notify();
                            }
                        }
                    } else {
                        let line_cell = this.pixel_to_line_cell(click_x, click_y);

                        // Shift+click extends the existing selection
                        if event.modifiers.shift && this.selection_anchor.is_some() {
                            this.selection_extent = Some(line_cell);
                            this.selecting = true;
                            cx.notify();
                            return;
                        }

                        match event.click_count {
                            2 => {
                                // Double-click: select word
                                if let Some((start, end)) = this.word_at_line(line_cell) {
                                    this.selection_anchor = Some(start);
                                    this.selection_extent = Some(end);
                                    this.selecting = false;
                                }
                            }
                            3 => {
                                // Triple-click: select entire line
                                let num_cols = this.terminal.as_ref()
                                    .map(|t| t.term.lock().grid().columns())
                                    .unwrap_or(80);
                                this.selection_anchor = Some((line_cell.0, 0));
                                this.selection_extent = Some((line_cell.0, num_cols.saturating_sub(1)));
                                this.selecting = false;
                            }
                            _ => {
                                // Single click: start drag selection
                                this.selection_anchor = Some(line_cell);
                                this.selection_extent = Some(line_cell);
                                this.selecting = true;
                            }
                        }
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this: &mut Self, event: &MouseMoveEvent, window, cx| {
                // Handle selection drag
                if this.selecting {
                    let x = f32::from(event.position.x);
                    let y = f32::from(event.position.y);

                    // Auto-scroll when drag goes past top/bottom edges
                    let origin_y = this.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
                    let viewport_h = f32::from(window.viewport_size().height);
                    let term_h = viewport_h - origin_y;
                    let local_y = y - origin_y;
                    let scroll_delta = if local_y < 0.0 {
                        // Above terminal area — scroll up
                        1
                    } else if local_y > term_h {
                        // Below terminal area — scroll down
                        -1
                    } else {
                        0
                    };
                    if scroll_delta != 0 {
                        if let Some(ref terminal) = this.terminal {
                            terminal.term.lock().scroll_display(Scroll::Delta(scroll_delta));
                            this.scroll_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }

                    let line_cell = this.pixel_to_line_cell(x, y);
                    this.selection_extent = Some(line_cell);
                    cx.notify();
                    return;
                }

                // URL detection on Cmd hover
                if event.modifiers.platform {
                    let x = f32::from(event.position.x);
                    let y = f32::from(event.position.y);
                    let cell = this.pixel_to_cell(x, y);
                    this.hovered_url = this.url_at(cell);
                    cx.notify();
                } else if this.hovered_url.is_some() {
                    this.hovered_url = None;
                    cx.notify();
                }

                if !this.scrollbar_dragging { return; }
                let Some(ref terminal) = this.terminal else { return };
                let t = terminal.term.lock();
                let total = t.grid().total_lines();
                let screen = t.grid().screen_lines();
                let history = total.saturating_sub(screen);
                let current_offset = t.grid().display_offset() as i32;
                drop(t);

                if history == 0 { return; }

                let origin_y = this.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
                let viewport_h = f32::from(window.viewport_size().height);
                let term_h = viewport_h - origin_y;
                let mouse_y = f32::from(event.position.y);
                let local_y = (mouse_y - origin_y).max(0.0);
                let fraction = (local_y / term_h).clamp(0.0, 1.0);
                let new_offset = ((1.0 - fraction) * history as f32).round() as i32;
                let delta = new_offset - current_offset;
                if delta != 0 {
                    terminal.term.lock().scroll_display(Scroll::Delta(delta));
                    this.scroll_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                    let was_selecting = this.selecting;
                    this.scrollbar_dragging = false;
                    this.selecting = false;
                    // Copy on select (macOS convention: implicit copy after mouse-up from drag)
                    if was_selecting {
                        if let Some(text) = this.get_selected_text() {
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                        }
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                    let was_selecting = this.selecting;
                    this.scrollbar_dragging = false;
                    this.selecting = false;
                    if was_selecting {
                        if let Some(text) = this.get_selected_text() {
                            if !text.is_empty() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                        }
                    }
                }),
            )
            .on_scroll_wheel({
                let term = self.terminal.as_ref().map(|t| t.term.clone());
                let scroll_dirty = self.scroll_dirty.clone();
                let cell_h = self.cell_height;
                move |event: &ScrollWheelEvent, _window: &mut Window, _cx: &mut App| {
                    let Some(ref term) = term else { return };
                    let delta = event.delta.pixel_delta(px(cell_h));
                    let lines = (f32::from(delta.y) / cell_h).round() as i32;
                    if lines != 0 {
                        term.lock().scroll_display(Scroll::Delta(lines));
                        scroll_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            })
            .child(grid_element)
            .children(if self.search_active {
                let match_count = self.search_matches.len();
                let current = if match_count > 0 { self.search_current_idx + 1 } else { 0 };
                let label = if self.search_query.is_empty() {
                    "Find...".to_string()
                } else if match_count > 0 {
                    format!("{} of {} — {}", current, match_count, self.search_query)
                } else {
                    format!("No matches — {}", self.search_query)
                };
                vec![div()
                    .absolute()
                    .top(px(4.0))
                    .right(px(12.0))
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(rgb(0x313244))
                    .border_1()
                    .border_color(rgb(0x585b70))
                    .rounded(px(6.0))
                    .text_size(px(12.0))
                    .text_color(rgb(0xcdd6f4))
                    .child(label)
                    .into_any_element()]
            } else {
                vec![]
            })
            .into_any_element()
    }
}
