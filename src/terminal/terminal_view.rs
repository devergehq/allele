use super::grid_element::TerminalGridElement;
use super::keymap::{self, AppAction, KeymapConfig};
use super::pty_terminal::{PtyTerminal, ShellCommand, TermSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use gpui::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FONT_FAMILY: &str = "JetBrains Mono";
pub const DEFAULT_FONT_SIZE: f32 = 13.0;
pub const MIN_FONT_SIZE: f32 = 8.0;
pub const MAX_FONT_SIZE: f32 = 32.0;

/// Clamp any f32 into the valid terminal font-size range.
pub fn clamp_font_size(size: f32) -> f32 {
    size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;
/// Milliseconds the desired terminal size must be stable before we commit
/// the resize to the PTY (sends SIGWINCH). Prevents rapid size oscillation
/// from cascading through CC's TUI re-render and duplicating scrollback.
const RESIZE_DEBOUNCE_MS: u64 = 80;

/// Events emitted by the terminal view to the parent
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    NewSession,
    CloseSession,
    SwitchSession(usize), // 0-indexed
    /// Cycle to the previous running session (skips Suspended).
    PrevSession,
    /// Cycle to the next running session (skips Suspended).
    NextSession,
    /// Toggle the bottom drawer terminal panel.
    ToggleDrawer,
    /// Toggle the left sidebar visibility.
    ToggleSidebar,
    /// Toggle the right sidebar visibility.
    ToggleRightSidebar,
    /// Open the scratch pad compose overlay (Cmd+K).
    OpenScratchPad,
    /// User hit Cmd+= / Cmd+- — adjust the global default font size by `delta`
    /// points. AppState clamps, persists, and broadcasts to every terminal.
    AdjustFontSize(f32),
    /// User hit Cmd+0 — reset the global default font size to the built-in
    /// default. Handled by AppState the same way as AdjustFontSize.
    ResetFontSize,
    /// Right-click on a detected file path in terminal output selected
    /// "Open in External Editor". Parent resolves editor command from
    /// user_settings and spawns it with optional line/col jump.
    OpenExternalEditor {
        path: PathBuf,
        line_col: Option<(u32, Option<u32>)>,
    },
}

impl EventEmitter<TerminalEvent> for TerminalView {}

/// State for the floating right-click context menu rendered on top of the
/// terminal grid when the user right-clicks a detected file path.
pub struct TerminalCtxMenu {
    pub path: PathBuf,
    pub line_col: Option<(u32, Option<u32>)>,
    pub position: Point<Pixels>,
}

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
    // Sub-cell pixel remainder for trackpad (ScrollDelta::Pixels) scrolling.
    // Accumulates fractional deltas across events so small continuous trackpad
    // input produces fluid scrolling instead of staccato single-line ticks.
    scroll_pixel_accumulator: std::sync::Arc<std::sync::Mutex<f32>>,
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
    // Path detection (Cmd hover). Same shape as hovered_url + parsed line/col.
    hovered_path: Option<(usize, usize, usize, PathBuf, Option<(u32, Option<u32>)>)>,
    /// Working directory captured at spawn. Used to resolve relative paths
    /// encountered in terminal output (e.g. grep results, compiler errors).
    working_dir: Option<PathBuf>,
    /// Persistent visual highlight spans for URLs in the visible viewport.
    /// Recomputed on grid changes. (row, col_start, col_end).
    visible_url_spans: Vec<(usize, usize, usize)>,
    /// Persistent visual highlight spans for file paths in the visible viewport.
    visible_path_spans: Vec<(usize, usize, usize)>,
    /// Active right-click context menu state. `None` = no menu visible.
    terminal_context_menu: Option<TerminalCtxMenu>,
    /// Pixels reserved below this terminal (e.g. drawer panel + chrome).
    /// Set by the parent before render so the PTY resize computation
    /// accounts for space that isn't available to this view.
    pub bottom_inset: f32,
    // Resize debounce — record desired size + timestamp, only commit
    // the resize to the PTY once the size has been stable for RESIZE_DEBOUNCE_MS.
    pending_resize: Option<(TermSize, Instant)>,
    // Bell flash state
    bell_flash_start: Option<Instant>,
    // FPS tracking
    frame_count: u32,
    last_fps_time: Instant,
    pub current_fps: u32,
    // Keymap
    keymap: KeymapConfig,
}

impl TerminalView {
    /// Access the underlying PTY terminal, if the session is still running.
    /// Used by external callers (e.g. the Scratch Pad) that need to write
    /// raw bytes into the PTY.
    pub fn pty(&self) -> Option<&PtyTerminal> {
        self.terminal.as_ref()
    }

    /// Create a terminal view running a specific command, or default shell if None
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        command: Option<ShellCommand>,
        working_dir: Option<PathBuf>,
        initial_font_size: f32,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let font_size = clamp_font_size(initial_font_size);

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
            TerminalGridElement::measure_cell(window, &font, px(font_size));
        let cell_width = f32::from(measured_w);
        let cell_height = f32::from(measured_h);

        let saved_working_dir = working_dir.clone();
        let terminal = match PtyTerminal::spawn(TermSize::default(), command, working_dir) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("Failed to create PTY: {e}");
                return Self {
                    terminal: None,
                    error: Some(format!("Failed to create PTY: {e}")),
                    last_cols: 80,
                    last_rows: 24,
                    focus_handle,
                    font_size,
                    cell_width,
                    cell_height,
                    scroll_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    scroll_pixel_accumulator: std::sync::Arc::new(std::sync::Mutex::new(0.0)),
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
                    hovered_path: None,
                    working_dir: saved_working_dir.clone(),
                    visible_url_spans: Vec::new(),
                    visible_path_spans: Vec::new(),
                    terminal_context_menu: None,
                    bottom_inset: 0.0,
                    pending_resize: None,
                    bell_flash_start: None,
                    frame_count: 0,
                    last_fps_time: Instant::now(),
                    current_fps: 0,
                    keymap: KeymapConfig::default(),
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

                        // Commit pending resize if stable for RESIZE_DEBOUNCE_MS.
                        let mut resize_committed = false;
                        if let Some((pending_size, recorded_at)) = this.pending_resize {
                            if recorded_at.elapsed() >= Duration::from_millis(RESIZE_DEBOUNCE_MS) {
                                tracing::info!(
                                    "[RESIZE-DIAG] COMMIT: {}x{} -> {}x{} | debounce={:.0}ms | {:?}",
                                    this.last_cols, this.last_rows,
                                    pending_size.cols, pending_size.rows,
                                    recorded_at.elapsed().as_millis(),
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis())
                                        .unwrap_or(0),
                                );
                                // Reset the entire grid before sending SIGWINCH.
                                // CC's ink re-renders the full conversation on resize —
                                // that repaint IS the correct canonical state at the new
                                // terminal width. clear_history() alone leaves the visible
                                // grid intact, so the old content persists as a single
                                // ghost copy when CC repaints on top. reset() clears both
                                // scrollback AND visible cells, giving CC a blank canvas.
                                if let Some(ref terminal) = this.terminal {
                                    let mut term = terminal.term.lock();
                                    let in_alt_screen = term.mode()
                                        .contains(alacritty_terminal::term::TermMode::ALT_SCREEN);
                                    if in_alt_screen {
                                        term.grid_mut().reset::<alacritty_terminal::vte::ansi::Color>();
                                        tracing::info!("[RESIZE-DIAG] RESET grid (alt-screen, pre-SIGWINCH)");
                                    } else {
                                        tracing::info!("[RESIZE-DIAG] SKIP reset (primary screen — preserve scrollback)");
                                    }
                                }
                                this.last_cols = pending_size.cols;
                                this.last_rows = pending_size.rows;
                                if let Some(ref mut terminal) = this.terminal {
                                    terminal.resize(pending_size);
                                }
                                this.pending_resize = None;
                                resize_committed = true;
                            }
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

                        pty_events || scrolled || blink_changed || opacity_changed || bell || bell_expired || resize_committed
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

        // Resize is handled exclusively in render() using fresh origin
        // values from the last paint pass. An earlier observe_window_bounds
        // handler was removed because it raced with render() and used stale
        // origin values, causing spurious resize oscillation that made CC
        // re-render its entire TUI and duplicate scrollback content.


        Self {
            terminal,
            error: None,
            last_cols: 80,
            last_rows: 24,
            focus_handle,
            font_size,
            cell_width,
            cell_height,
            scroll_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            scroll_pixel_accumulator: std::sync::Arc::new(std::sync::Mutex::new(0.0)),
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
            hovered_path: None,
            working_dir: saved_working_dir,
            visible_url_spans: Vec::new(),
            visible_path_spans: Vec::new(),
            terminal_context_menu: None,
            bottom_inset: 0.0,
            pending_resize: None,
            bell_flash_start: None,
            frame_count: 0,
            last_fps_time: Instant::now(),
            current_fps: 0,
            keymap: KeymapConfig::default(),
        }
    }

    /// Apply a new font size coming from AppState (settings change or
    /// broadcast after Cmd+=/Cmd+-). Clamps, remeasures, notifies. No-op
    /// when the value matches the current size.
    pub fn set_font_size(&mut self, size: f32, window: &mut Window, cx: &mut Context<Self>) {
        let new_size = clamp_font_size(size);
        if (new_size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = new_size;
        self.remeasure_cells(window);
        cx.notify();
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

    /// Write bytes directly to the PTY, as if the user had typed them.
    /// Used to inject startup commands into interactive shells spawned by
    /// `allele.json` — the shell reads them from its stdin buffer once the
    /// rc files finish loading, so the command runs but the shell stays
    /// interactive afterwards.
    pub fn send_input(&self, bytes: &[u8]) {
        if let Some(ref t) = self.terminal {
            t.write(bytes);
        }
    }

    /// Get the current terminal title set by the shell via OSC sequences.
    pub fn title(&self) -> Option<String> {
        self.terminal.as_ref().and_then(|t| t.title.clone())
    }

    /// Register a cleanup callback to run when this terminal is dropped
    /// (suspend, remove, app exit). Forwards to `PtyTerminal::on_close`.
    /// No-op if the PTY failed to spawn.
    pub fn on_close<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + 'static,
    {
        if let Some(t) = self.terminal.as_mut() {
            t.on_close(hook);
        }
    }

    /// Convert window-relative pixel position to visible screen cell (row, col).
    /// row is the visible row index (0..screen_lines), col is the column.
    /// Convert a window-relative pixel position into (row, col) grid
    /// coordinates, clamped to the current grid bounds.
    ///
    /// Returns `None` if:
    /// - there is no attached terminal
    /// - the grid is zero-sized (transient during resize/init)
    ///
    /// This is the single source of "valid grid cell" truth — any pixel
    /// position outside the grid area (padding, out of window, etc.) is
    /// clamped to the nearest valid cell. Without this clamping, downstream
    /// calls like `url_at` and `word_at` would build a `Line(row - offset)`
    /// with `row >= screen_lines`, which trips alacritty's
    /// `compute_index` debug_assert and aborts the app.
    fn pixel_to_cell(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let origin_x = self.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
        let origin_y = self.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
        let local_x = (x - origin_x).max(0.0);
        let local_y = (y - origin_y).max(0.0);
        let raw_col = (local_x / self.cell_width).floor() as usize;
        let raw_row = (local_y / self.cell_height).floor() as usize;

        let terminal = self.terminal.as_ref()?;
        let term = terminal.term.lock();
        let grid = term.grid();
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        drop(term);

        if num_lines == 0 || num_cols == 0 {
            return None;
        }

        let row = raw_row.min(num_lines - 1);
        let col = raw_col.min(num_cols - 1);
        Some((row, col))
    }

    /// Convert window-relative pixel position to an alacritty line-coordinate cell.
    /// Returns (line_offset, col) where line_offset is negative for history,
    /// stable across scroll events. Use this for selection anchors.
    ///
    /// Returns `None` if the underlying `pixel_to_cell` does.
    fn pixel_to_line_cell(&self, x: f32, y: f32) -> Option<(i32, usize)> {
        let (row, col) = self.pixel_to_cell(x, y)?;
        let display_offset = self
            .terminal
            .as_ref()
            .map(|t| t.term.lock().grid().display_offset() as i32)
            .unwrap_or(0);
        // line_offset = visible_row - display_offset
        // (Same formula the renderer uses: grid_line = line_idx - display_offset)
        let line_offset = row as i32 - display_offset;
        Some((line_offset, col))
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
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        if num_lines == 0 || num_cols == 0 {
            return None;
        }
        // Valid alacritty Line indices are [-history_size, num_lines-1].
        // Persisted selection anchors can go stale if the grid resized since
        // the selection was made — clamp to the current valid range before
        // we touch any grid cell.
        let history_size = grid.total_lines().saturating_sub(num_lines) as i32;
        let min_line = -history_size;
        let max_line = num_lines as i32 - 1;

        // Normalise: start <= end
        let (start_line, start_col, end_line, end_col) = if anchor.0 < extent.0
            || (anchor.0 == extent.0 && anchor.1 <= extent.1)
        {
            (anchor.0, anchor.1, extent.0, extent.1)
        } else {
            (extent.0, extent.1, anchor.0, anchor.1)
        };

        // Clamp to the current grid's visible range — if the selection was
        // entirely outside the current grid, give up.
        let start_line = start_line.clamp(min_line, max_line);
        let end_line = end_line.clamp(min_line, max_line);
        if start_line > end_line {
            return None;
        }

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

    /// Build (chars, columns) parallel arrays for a visible row, skipping
    /// WIDE_CHAR_SPACER cells. Returns empty vecs if row is out of range.
    fn line_chars_cols(&self, row: usize) -> (Vec<char>, Vec<usize>) {
        let Some(terminal) = self.terminal.as_ref() else { return (vec![], vec![]) };
        let term = terminal.term.lock();
        let grid = term.grid();
        let num_cols = grid.columns();
        let num_lines = grid.screen_lines();
        if row >= num_lines {
            return (vec![], vec![]);
        }
        let display_offset = grid.display_offset() as i32;
        let grid_line = Line(row as i32 - display_offset);
        let mut chars = Vec::with_capacity(num_cols);
        let mut cols = Vec::with_capacity(num_cols);
        for col in 0..num_cols {
            let c = &grid[grid_line][Column(col)];
            if c.flags.contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let ch = if c.c == '\0' { ' ' } else { c.c };
            chars.push(ch);
            cols.push(col);
        }
        (chars, cols)
    }

    /// Classify a raw token into a (resolved_path, line_col) pair if it
    /// refers to an existing filesystem path, otherwise `None`.
    ///
    /// Accepts: absolute paths (`/...`, `~/...`), explicit relative
    /// (`./...`, `../...`), and bare relative forms that must exist
    /// under `working_dir`. Parses an optional `:line[:col]` suffix.
    fn classify_path_token(
        &self,
        token: &str,
    ) -> Option<(PathBuf, Option<(u32, Option<u32>)>)> {
        if token.is_empty() {
            return None;
        }

        // Split trailing :N[:M] into (path_part, line_col). Only accept
        // purely numeric segments — otherwise leave the colon in the path.
        let (path_part, line_col) = parse_line_col_suffix(token);

        // Expand ~ → $HOME.
        let expanded: PathBuf = if let Some(rest) = path_part.strip_prefix("~/") {
            match dirs::home_dir() {
                Some(h) => h.join(rest),
                None => return None,
            }
        } else if path_part == "~" {
            dirs::home_dir()?
        } else {
            PathBuf::from(path_part)
        };

        // Resolve relative to working_dir when applicable.
        let candidate: PathBuf = if expanded.is_absolute() {
            expanded
        } else if let Some(ref wd) = self.working_dir {
            wd.join(&expanded)
        } else {
            expanded
        };

        if candidate.exists() {
            Some((candidate, line_col))
        } else {
            None
        }
    }

    /// Cheap syntactic check used by the persistent viewport scan — no
    /// filesystem stat. Returns true if the token *looks like* it could
    /// be a path, based on shape only.
    fn token_looks_like_path(token: &str) -> bool {
        if token.len() < 2 {
            return false;
        }
        let (path_part, _) = parse_line_col_suffix(token);
        if path_part.is_empty() {
            return false;
        }
        if path_part.starts_with('/') || path_part.starts_with("~/") || path_part == "~" {
            return true;
        }
        if path_part.starts_with("./") || path_part.starts_with("../") {
            return true;
        }
        // Bare relative: require at least one '/' or a file extension to
        // avoid matching random words. e.g. `src/main.rs` or `foo.rs`.
        if path_part.contains('/') {
            return true;
        }
        if let Some(dot) = path_part.rfind('.') {
            let ext = &path_part[dot + 1..];
            // Extensions: 1-8 alphanumeric chars.
            if (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return true;
            }
        }
        false
    }

    /// Find a file path at the given grid cell, if any. Mirrors `url_at`:
    /// returns `(row, col_start, col_end, resolved_path, line_col)` so the
    /// caller can draw a highlight and resolve the click target.
    fn path_at(
        &self,
        cell: (usize, usize),
    ) -> Option<(usize, usize, usize, PathBuf, Option<(u32, Option<u32>)>)> {
        let (row, col) = cell;
        let (chars, cols) = self.line_chars_cols(row);
        if chars.is_empty() {
            return None;
        }

        // Locate the whitespace-delimited token at `col`, stripping common
        // surrounding punctuation ([]()"'{}<>).
        let hovered_idx = cols.iter().position(|&c| c == col)?;

        if chars[hovered_idx].is_whitespace() {
            return None;
        }

        // Walk left/right to find token bounds.
        let mut tok_start = hovered_idx;
        while tok_start > 0 && !chars[tok_start - 1].is_whitespace() {
            tok_start -= 1;
        }
        let mut tok_end = hovered_idx;
        while tok_end + 1 < chars.len() && !chars[tok_end + 1].is_whitespace() {
            tok_end += 1;
        }

        // Trim matched brackets/quotes at edges.
        while tok_start < tok_end
            && matches!(chars[tok_start], '(' | '[' | '{' | '<' | '"' | '\'' | '`')
        {
            tok_start += 1;
        }
        while tok_end > tok_start
            && matches!(chars[tok_end], ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | ';' | '.' | '!' | '?')
        {
            tok_end -= 1;
        }

        if tok_start > tok_end {
            return None;
        }

        let token: String = chars[tok_start..=tok_end].iter().collect();
        let (path, line_col) = self.classify_path_token(&token)?;
        let col_start = cols[tok_start];
        let col_end = cols[tok_end];
        Some((row, col_start, col_end, path, line_col))
    }

    /// Scan the visible viewport for URL and path spans. Called when the
    /// grid mutates; results are cached in `visible_url_spans` /
    /// `visible_path_spans` for the renderer to consume.
    ///
    /// Path detection uses `token_looks_like_path` (no stat) to stay cheap
    /// on large outputs; the exact `path_at` filesystem check only runs
    /// on hover/right-click.
    fn scan_visible_spans(&mut self) {
        self.visible_url_spans.clear();
        self.visible_path_spans.clear();

        let Some(terminal) = self.terminal.as_ref() else { return };
        let num_lines = {
            let term = terminal.term.lock();
            term.grid().screen_lines()
        };

        for row in 0..num_lines {
            let (chars, cols) = self.line_chars_cols(row);
            if chars.is_empty() {
                continue;
            }

            // URLs: http(s):// scan (same as url_at).
            let text: String = chars.iter().collect();
            for (scheme_start, _) in text.match_indices("http://").chain(text.match_indices("https://")) {
                // Convert byte offset → char offset in `chars`.
                let prefix = &text[..scheme_start];
                let char_start = prefix.chars().count();
                let mut char_end = char_start;
                while char_end < chars.len() {
                    let c = chars[char_end];
                    if c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'') {
                        break;
                    }
                    char_end += 1;
                }
                while char_end > char_start
                    && matches!(chars[char_end - 1], '.' | ',' | ';' | ':' | '!' | '?')
                {
                    char_end -= 1;
                }
                if char_end > char_start {
                    let col_start = cols[char_start];
                    let col_end = cols[char_end - 1];
                    self.visible_url_spans.push((row, col_start, col_end));
                }
            }

            // Paths: whitespace-delimited tokens that look like paths.
            let mut i = 0;
            while i < chars.len() {
                if chars[i].is_whitespace() {
                    i += 1;
                    continue;
                }
                let tok_start = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                let mut tok_end = i - 1;

                // Trim matched brackets/quotes.
                let mut ts = tok_start;
                while ts < tok_end
                    && matches!(chars[ts], '(' | '[' | '{' | '<' | '"' | '\'' | '`')
                {
                    ts += 1;
                }
                while tok_end > ts
                    && matches!(chars[tok_end], ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | ';' | '.' | '!' | '?')
                {
                    tok_end -= 1;
                }
                if ts > tok_end {
                    continue;
                }
                let token: String = chars[ts..=tok_end].iter().collect();
                if Self::token_looks_like_path(&token) {
                    let col_start = cols[ts];
                    let col_end = cols[tok_end];
                    self.visible_path_spans.push((row, col_start, col_end));
                }
            }
        }
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

    /// Render the floating right-click context menu for a detected file
    /// path. Returns an empty vec when no menu is active.
    fn render_terminal_context_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(menu) = self.terminal_context_menu.as_ref() else {
            return Vec::new();
        };
        let path = menu.path.clone();
        let line_col = menu.line_col;
        let position = menu.position;

        let item = |id: &'static str,
                    label: String,
                    on_click: Box<
            dyn Fn(&mut TerminalView, &mut Context<TerminalView>) + 'static,
        >| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x45475a)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        on_click(this, cx);
                        this.terminal_context_menu = None;
                        cx.notify();
                    }),
                )
        };

        let label_open = match line_col {
            Some((line, Some(col))) => format!("Open in Editor ({}:{})", line, col),
            Some((line, None)) => format!("Open in Editor (line {})", line),
            None => "Open in Editor".to_string(),
        };

        let path_for_open = path.clone();
        let path_for_reveal = path;

        let menu_el = div()
            .flex()
            .flex_col()
            .min_w(px(220.0))
            .py(px(4.0))
            .bg(rgb(0x181825))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded(px(6.0))
            .shadow_md()
            .child(item(
                "term-ctx-open-editor",
                label_open,
                Box::new(move |this: &mut TerminalView, cx: &mut Context<TerminalView>| {
                    cx.emit(TerminalEvent::OpenExternalEditor {
                        path: path_for_open.clone(),
                        line_col,
                    });
                    let _ = this;
                }),
            ))
            .child(item(
                "term-ctx-reveal",
                "Reveal in Finder".to_string(),
                Box::new(move |_this: &mut TerminalView, _cx: &mut Context<TerminalView>| {
                    crate::platform::global().shell.reveal_in_files(&path_for_reveal);
                }),
            ));

        vec![deferred(
            anchored()
                .position(position)
                .snap_to_window()
                .child(menu_el),
        )
        .into_any_element()]
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Opportunistic PTY resize: if the visible terminal area changed since
        // last render (e.g. sidebar was dragged), record the desired size.
        // The actual resize is debounced — committed by the poll loop once
        // the size has been stable for RESIZE_DEBOUNCE_MS.
        {
            let viewport = window.viewport_size();
            let origin_x = self.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
            let origin_y = self.element_origin_y.load(std::sync::atomic::Ordering::Relaxed) as f32;
            if origin_x > 0.0 {
                let available_width = f32::from(viewport.width) - origin_x;
                let available_height = f32::from(viewport.height) - origin_y - self.bottom_inset;
                if available_width > 100.0 && available_height > 100.0 {
                    let new_size = Self::compute_size(
                        available_width, available_height,
                        self.cell_width, self.cell_height,
                    );
                    if new_size.cols != self.last_cols || new_size.rows != self.last_rows {
                        // Check if the pending resize already matches — only
                        // reset the debounce timer if the desired size changed.
                        let should_record = match self.pending_resize {
                            Some((pending, _)) => {
                                pending.cols != new_size.cols || pending.rows != new_size.rows
                            }
                            None => true,
                        };
                        if should_record {
                            tracing::info!(
                                "[RESIZE-DIAG] RECORD: {}x{} -> {}x{} | origin=({:.1},{:.1}) viewport=({:.1},{:.1}) avail=({:.1},{:.1}) cell=({:.1},{:.1}) | {:?}",
                                self.last_cols, self.last_rows,
                                new_size.cols, new_size.rows,
                                origin_x, origin_y,
                                f32::from(viewport.width), f32::from(viewport.height),
                                available_width, available_height,
                                self.cell_width, self.cell_height,
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0),
                            );
                            self.pending_resize = Some((new_size, Instant::now()));
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

        // Rescan visible viewport for URL/path highlight spans. Runs only
        // when render() runs, which is driven by cx.notify() after pty
        // events / scroll / resize / blink / bell — so pinned to the
        // existing render cadence, not per-frame.
        self.scan_visible_spans();

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
        .hovered_path(self.hovered_path.as_ref().map(|(r, cs, ce, _, _)| (*r, *cs, *ce)))
        .url_spans(self.visible_url_spans.clone())
        .path_spans(self.visible_path_spans.clone())
        .origin_out(self.element_origin_x.clone(), self.element_origin_y.clone());

        let bell_active = self.bell_flash_start.is_some();
        let bg_color = if bell_active { rgb(0x3a2e3a) } else { rgb(0x1e1e2e) };

        div()
            .id("terminal")
            .size_full()
            .bg(bg_color)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                this.last_keypress = Instant::now();
                this.cursor_visible = true;

                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;

                // Escape dismisses an open terminal context menu first.
                if key == "escape" && this.terminal_context_menu.is_some() {
                    this.terminal_context_menu = None;
                    cx.notify();
                    return;
                }

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

                // ── App-level shortcuts (Cmd key) ─────────────────────
                if let Some(action) = keymap::app_action(key, mods) {
                    match action {
                        AppAction::Paste => {
                            if let Some(ref terminal) = this.terminal {
                                if let Some(item) = cx.read_from_clipboard() {
                                    if let Some(text) = item.text() {
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
                        }
                        AppAction::Copy => {
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
                        }
                        AppAction::OpenSearch => {
                            this.search_active = true;
                            this.search_query.clear();
                            this.search_matches.clear();
                            this.search_current_idx = 0;
                            cx.notify();
                        }
                        AppAction::FindNext => {
                            if !this.search_matches.is_empty() {
                                this.search_current_idx =
                                    (this.search_current_idx + 1) % this.search_matches.len();
                                cx.notify();
                            }
                        }
                        AppAction::FindPrevious => {
                            if !this.search_matches.is_empty() {
                                this.search_current_idx = if this.search_current_idx == 0 {
                                    this.search_matches.len() - 1
                                } else {
                                    this.search_current_idx - 1
                                };
                                cx.notify();
                            }
                        }
                        AppAction::ZoomIn => cx.emit(TerminalEvent::AdjustFontSize(1.0)),
                        AppAction::ZoomOut => cx.emit(TerminalEvent::AdjustFontSize(-1.0)),
                        AppAction::ZoomReset => cx.emit(TerminalEvent::ResetFontSize),
                        AppAction::NewSession => cx.emit(TerminalEvent::NewSession),
                        AppAction::CloseSession => cx.emit(TerminalEvent::CloseSession),
                        AppAction::PrevSession => cx.emit(TerminalEvent::PrevSession),
                        AppAction::NextSession => cx.emit(TerminalEvent::NextSession),
                        AppAction::SwitchSession(idx) => cx.emit(TerminalEvent::SwitchSession(idx)),
                        AppAction::ToggleDrawer => cx.emit(TerminalEvent::ToggleDrawer),
                        AppAction::ToggleSidebar => cx.emit(TerminalEvent::ToggleSidebar),
                        AppAction::ToggleRightSidebar => cx.emit(TerminalEvent::ToggleRightSidebar),
                        AppAction::OpenScratchPad => cx.emit(TerminalEvent::OpenScratchPad),
                        AppAction::SendBytes(bytes) => {
                            if let Some(ref terminal) = this.terminal {
                                terminal.write(bytes);
                            }
                        }
                    }
                    return;
                }

                // ── Terminal input (policy-based) ─────────────────────
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

                // Resolve keystroke → bytes via keymap policy engine
                let key_char = event.keystroke.key_char.as_deref();
                if let Some(bytes) = this.keymap.resolve(key, mods, key_char) {
                    terminal.write(&bytes);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, event: &MouseDownEvent, window, cx| {
                    // Any left-click dismisses an open terminal context menu.
                    if this.terminal_context_menu.is_some() {
                        this.terminal_context_menu = None;
                        cx.notify();
                    }
                    let viewport = window.viewport_size();
                    let click_x = f32::from(event.position.x);
                    let click_y = f32::from(event.position.y);
                    // Scrollbar is at the right edge of the terminal element
                    let origin_x = this.element_origin_x.load(std::sync::atomic::Ordering::Relaxed) as f32;
                    let term_width = f32::from(viewport.width) - origin_x;
                    let scrollbar_zone = origin_x + term_width - 12.0;

                    // Cmd+click to open URLs
                    if event.modifiers.platform {
                        if let Some(cell) = this.pixel_to_cell(click_x, click_y) {
                            if let Some((_, _, _, url)) = this.url_at(cell) {
                                crate::platform::global().shell.open_url(&url);
                                return;
                            }
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
                        // Bail out silently if the click landed outside the
                        // grid area (padding region, zero-sized grid during
                        // resize, etc.). Prevents downstream OOB grid access.
                        let Some(line_cell) = this.pixel_to_line_cell(click_x, click_y) else {
                            return;
                        };

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

                    // Update the drag selection only if the mouse is still
                    // inside the grid area. Outside positions are swallowed —
                    // the selection continues to reflect the last in-bounds cell.
                    if let Some(line_cell) = this.pixel_to_line_cell(x, y) {
                        this.selection_extent = Some(line_cell);
                    }
                    cx.notify();
                    return;
                }

                // URL / path detection on Cmd hover
                if event.modifiers.platform {
                    let x = f32::from(event.position.x);
                    let y = f32::from(event.position.y);
                    // If the hover is out of grid bounds (padding, etc.)
                    // clear any existing hovered URL. Prevents calling
                    // url_at with an OOB cell.
                    let cell = this.pixel_to_cell(x, y);
                    this.hovered_url = cell.and_then(|c| this.url_at(c));
                    this.hovered_path = if this.hovered_url.is_some() {
                        None
                    } else {
                        cell.and_then(|c| this.path_at(c))
                    };
                    cx.notify();
                } else if this.hovered_url.is_some() || this.hovered_path.is_some() {
                    this.hovered_url = None;
                    this.hovered_path = None;
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
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this: &mut Self, event: &MouseDownEvent, _window, cx| {
                    let x = f32::from(event.position.x);
                    let y = f32::from(event.position.y);
                    let Some(cell) = this.pixel_to_cell(x, y) else { return };

                    // URL: open immediately in default browser, no menu.
                    if let Some((_, _, _, url)) = this.url_at(cell) {
                        crate::platform::global().shell.open_url(&url);
                        return;
                    }

                    // Path: show context menu anchored at the click point.
                    if let Some((_, _, _, path, line_col)) = this.path_at(cell) {
                        this.terminal_context_menu = Some(TerminalCtxMenu {
                            path,
                            line_col,
                            position: event.position,
                        });
                        cx.notify();
                    } else if this.terminal_context_menu.is_some() {
                        this.terminal_context_menu = None;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                    let was_selecting = this.selecting;
                    this.scrollbar_dragging = false;
                    this.selecting = false;
                    // If the selection is trivially small (same cell or adjacent),
                    // clear it — single clicks shouldn't leave a highlight.
                    if let (Some(a), Some(e)) = (this.selection_anchor, this.selection_extent) {
                        let span = if a.0 == e.0 {
                            (e.1 as isize - a.1 as isize).unsigned_abs()
                        } else {
                            // Multi-line selection is always meaningful
                            usize::MAX
                        };
                        if span < 2 {
                            this.selection_anchor = None;
                            this.selection_extent = None;
                            cx.notify();
                            return;
                        }
                    }
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
                    // Clear trivially small selections on mouse-up-out too.
                    if let (Some(a), Some(e)) = (this.selection_anchor, this.selection_extent) {
                        let span = if a.0 == e.0 {
                            (e.1 as isize - a.1 as isize).unsigned_abs()
                        } else {
                            usize::MAX
                        };
                        if span < 2 {
                            this.selection_anchor = None;
                            this.selection_extent = None;
                            cx.notify();
                            return;
                        }
                    }
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
                let accumulator = self.scroll_pixel_accumulator.clone();
                let cell_h = self.cell_height;
                move |event: &ScrollWheelEvent, _window: &mut Window, _cx: &mut App| {
                    let Some(ref term) = term else { return };
                    // Trackpad (Pixels) delivers small sub-cell deltas per frame;
                    // we must accumulate them to produce fluid scrolling. Mouse
                    // wheel (Lines) delivers discrete line counts and must bypass
                    // the accumulator so its precision isn't diluted by a stale
                    // trackpad remainder.
                    let lines = match event.delta {
                        ScrollDelta::Pixels(delta_px) => {
                            // SAFETY: single-threaded access on the UI thread —
                            // the Mutex is interior-mutability across closures,
                            // not cross-thread sync. Poisoning is impossible.
                            let mut acc = accumulator.lock().unwrap();
                            *acc += f32::from(delta_px.y);
                            let whole = (*acc / cell_h).trunc() as i32;
                            if whole != 0 {
                                *acc -= whole as f32 * cell_h;
                            }
                            whole
                        }
                        ScrollDelta::Lines(delta_ln) => {
                            // Mouse wheel — reset any fractional trackpad remainder
                            // so direction changes between devices feel immediate.
                            // SAFETY: same as above — UI-thread-only Mutex.
                            *accumulator.lock().unwrap() = 0.0;
                            delta_ln.y.round() as i32
                        }
                    };
                    if lines != 0 {
                        term.lock().scroll_display(Scroll::Delta(lines));
                        scroll_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            })
            .child(grid_element)
            .children(self.render_terminal_context_menu(cx))
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

/// Split `foo.rs:12:34` into (`foo.rs`, Some((12, Some(34)))).
/// If no numeric suffix is present, returns (token, None).
pub(crate) fn parse_line_col_suffix(token: &str) -> (&str, Option<(u32, Option<u32>)>) {
    // Walk from end, peeling at most two numeric segments.
    let bytes = token.as_bytes();
    let mut end = bytes.len();
    let mut nums: Vec<(usize, u32)> = Vec::with_capacity(2);

    while nums.len() < 2 {
        // Scan backwards over digits.
        let mut digit_start = end;
        while digit_start > 0 && bytes[digit_start - 1].is_ascii_digit() {
            digit_start -= 1;
        }
        if digit_start == end || digit_start == 0 {
            break;
        }
        // Require ':' right before the digit run.
        if bytes[digit_start - 1] != b':' {
            break;
        }
        let digits = &token[digit_start..end];
        let Ok(n) = digits.parse::<u32>() else { break };
        nums.push((digit_start - 1, n)); // colon position
        end = digit_start - 1;
    }

    if nums.is_empty() {
        return (token, None);
    }
    // nums[0] is the rightmost (col if two), nums[last] is leftmost (line).
    // SAFETY: is_empty early-return above guarantees nums has ≥ 1 element.
    let path_end = nums.last().unwrap().0;
    let path_part = &token[..path_end];
    if path_part.is_empty() {
        return (token, None);
    }
    let line_col = match nums.len() {
        1 => Some((nums[0].1, None)),
        2 => Some((nums[1].1, Some(nums[0].1))),
        _ => None,
    };
    (path_part, line_col)
}
