use super::pty_terminal::{PtyTerminal, ShellCommand, TermSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use gpui::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FONT_FAMILY: &str = "JetBrains Mono";
const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = 18.0;
const DEFAULT_CELL_WIDTH: f32 = 7.6; // Fallback if measurement fails
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
    cell_width: f32,
    cell_height: f32,
    scroll_offset: i32, // 0 = bottom (showing latest), negative = scrolled up into history
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

        // Measure actual monospace character width from the font
        let cell_width = {
            let text_system = window.text_system();
            let font = Font {
                family: FONT_FAMILY.into(),
                ..Default::default()
            };
            let font_id = text_system.resolve_font(&font);
            let font_size = px(FONT_SIZE);
            text_system.em_advance(font_id, font_size)
                .map(|px_val| f32::from(px_val))
                .unwrap_or(DEFAULT_CELL_WIDTH)
        };
        let cell_height = LINE_HEIGHT;
        eprintln!("Measured cell dimensions: {cell_width}px x {cell_height}px");

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
                    cell_width,
                    cell_height,
                    scroll_offset: 0,
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
                        if let Some(ref mut terminal) = this.terminal {
                            terminal.drain_events()
                        } else {
                            false
                        }
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

        // Observe window bounds changes for resize
        cx.observe_window_bounds(window, |this: &mut Self, window, _cx| {
            let viewport = window.viewport_size();
            let sidebar_width = px(240.0);
            let available_width = viewport.width - sidebar_width;
            let available_height = viewport.height;

            if available_width > px(100.0) && available_height > px(100.0) {
                let new_size = Self::compute_size(
                    f32::from(available_width),
                    f32::from(available_height),
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
            cell_width,
            cell_height,
            scroll_offset: 0,
            frame_count: 0,
            last_fps_time: Instant::now(),
            current_fps: 0,
        }
    }

    /// Convert an ANSI colour to GPUI Hsla
    fn ansi_to_hsla(color: &AnsiColor) -> Hsla {
        let rgba_val: u32 = match color {
            AnsiColor::Named(named) => match named {
                NamedColor::Black => 0x45475a,
                NamedColor::Red => 0xf38ba8,
                NamedColor::Green => 0xa6e3a1,
                NamedColor::Yellow => 0xf9e2af,
                NamedColor::Blue => 0x89b4fa,
                NamedColor::Magenta => 0xcba6f7,
                NamedColor::Cyan => 0x94e2d5,
                NamedColor::White => 0xbac2de,
                NamedColor::BrightBlack => 0x585b70,
                NamedColor::BrightRed => 0xf38ba8,
                NamedColor::BrightGreen => 0xa6e3a1,
                NamedColor::BrightYellow => 0xf9e2af,
                NamedColor::BrightBlue => 0x89b4fa,
                NamedColor::BrightMagenta => 0xcba6f7,
                NamedColor::BrightCyan => 0x94e2d5,
                NamedColor::BrightWhite => 0xffffff,
                NamedColor::Foreground => 0xcdd6f4,
                NamedColor::Background => 0x1e1e2e,
                _ => 0xcdd6f4,
            },
            AnsiColor::Spec(rgb_color) => {
                return Hsla::from(Rgba {
                    r: rgb_color.r as f32 / 255.0,
                    g: rgb_color.g as f32 / 255.0,
                    b: rgb_color.b as f32 / 255.0,
                    a: 1.0,
                });
            }
            AnsiColor::Indexed(idx) => {
                match *idx {
                    0 => 0x45475a,
                    1 => 0xf38ba8,
                    2 => 0xa6e3a1,
                    3 => 0xf9e2af,
                    4 => 0x89b4fa,
                    5 => 0xcba6f7,
                    6 => 0x94e2d5,
                    7 => 0xbac2de,
                    8 => 0x585b70,
                    9 => 0xf38ba8,
                    10 => 0xa6e3a1,
                    11 => 0xf9e2af,
                    12 => 0x89b4fa,
                    13 => 0xcba6f7,
                    14 => 0x94e2d5,
                    15 => 0xffffff,
                    // 16-231: 6x6x6 colour cube
                    16..=231 => {
                        let i = idx - 16;
                        let r = (i / 36) * 51;
                        let g = ((i / 6) % 6) * 51;
                        let b = (i % 6) * 51;
                        return Hsla::from(Rgba {
                            r: r as f32 / 255.0,
                            g: g as f32 / 255.0,
                            b: b as f32 / 255.0,
                            a: 1.0,
                        });
                    }
                    // 232-255: grayscale ramp
                    _ => {
                        let v = (idx - 232) * 10 + 8;
                        return Hsla::from(Rgba {
                            r: v as f32 / 255.0,
                            g: v as f32 / 255.0,
                            b: v as f32 / 255.0,
                            a: 1.0,
                        });
                    }
                }
            }
        };
        let r = ((rgba_val >> 16) & 0xFF) as f32 / 255.0;
        let g = ((rgba_val >> 8) & 0xFF) as f32 / 255.0;
        let b = (rgba_val & 0xFF) as f32 / 255.0;
        Hsla::from(Rgba { r, g, b, a: 1.0 })
    }

    /// Focus this terminal view
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus_handle.focus(window, cx);
    }

    /// Check if the PTY process has exited
    pub fn has_exited(&self) -> bool {
        self.terminal.as_ref().map_or(true, |t| t.exited)
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let Some(ref mut terminal) = self.terminal else {
            return div()
                .size_full()
                .bg(rgb(0x1e1e2e))
                .child("No terminal")
                .into_any_element();
        };

        // Lock the terminal and read the grid
        let term = terminal.term.lock();
        let grid = term.grid();
        let cursor_point = grid.cursor.point;

        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();

        // Build rows with per-cell colour spans
        let mut row_elements: Vec<AnyElement> = Vec::with_capacity(num_lines);
        let default_fg = Self::ansi_to_hsla(&AnsiColor::Named(NamedColor::Foreground));

        for line_idx in 0..num_lines {
            let mut spans: Vec<AnyElement> = Vec::new();
            let mut current_text = String::new();
            let mut current_fg = default_fg;
            let mut current_bg: Option<Hsla> = None;
            let mut current_bold = false;
            let mut current_italic = false;

            for col_idx in 0..num_cols {
                let cell = &grid[Line(line_idx as i32)][Column(col_idx)];
                let c = cell.c;
                let is_cursor = line_idx == cursor_point.line.0 as usize
                    && col_idx == cursor_point.column.0;

                // Skip wide char spacers
                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let cell_fg = if is_cursor {
                    Self::ansi_to_hsla(&AnsiColor::Named(NamedColor::Background))
                } else {
                    Self::ansi_to_hsla(&cell.fg)
                };

                let cell_bg = if is_cursor {
                    Some(Self::ansi_to_hsla(&AnsiColor::Named(NamedColor::Foreground)))
                } else {
                    // Only set bg if it's not the default background
                    let bg = Self::ansi_to_hsla(&cell.bg);
                    let default_bg = Self::ansi_to_hsla(&AnsiColor::Named(NamedColor::Background));
                    if bg != default_bg {
                        Some(bg)
                    } else {
                        None
                    }
                };

                let bold = cell.flags.contains(CellFlags::BOLD);
                let italic = cell.flags.contains(CellFlags::ITALIC);

                // If style changed, flush the current span
                if cell_fg != current_fg
                    || cell_bg != current_bg
                    || bold != current_bold
                    || italic != current_italic
                {
                    if !current_text.is_empty() {
                        let text = std::mem::take(&mut current_text);
                        let fg = current_fg;
                        let bg = current_bg;
                        let b = current_bold;
                        let mut el = div().text_color(fg);
                        if let Some(bg_color) = bg {
                            el = el.bg(bg_color);
                        }
                        if b {
                            el = el.font_weight(FontWeight::BOLD);
                        }
                        spans.push(el.child(text).into_any_element());
                    }
                    current_fg = cell_fg;
                    current_bg = cell_bg;
                    current_bold = bold;
                    current_italic = italic;
                }

                let ch = if c == '\0' { ' ' } else { c };
                current_text.push(ch);
            }

            // Flush remaining text in the row
            if !current_text.is_empty() {
                let text = current_text;
                let fg = current_fg;
                let bg = current_bg;
                let mut el = div().text_color(fg);
                if let Some(bg_color) = bg {
                    el = el.bg(bg_color);
                }
                if current_bold {
                    el = el.font_weight(FontWeight::BOLD);
                }
                spans.push(el.child(text).into_any_element());
            }

            // If empty row, push a space to maintain line height
            if spans.is_empty() {
                spans.push(div().child(" ").into_any_element());
            }

            row_elements.push(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .children(spans)
                    .into_any_element(),
            );
        }

        drop(term);

        // Capture cols/rows for resize detection closure
        let last_cols = self.last_cols;
        let last_rows = self.last_rows;

        div()
            .id("terminal")
            .size_full()
            .bg(rgb(0x1e1e2e))
            .font_family(FONT_FAMILY)
            .text_size(px(FONT_SIZE))
            .line_height(px(self.cell_height))
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;

                // Handle Cmd shortcuts (emit to parent)
                if mods.platform {
                    match key {
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

                // Handle special keys BEFORE key_char — enter, backspace, etc.
                // must be handled as control sequences, not as their character value
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
            // Scroll wheel handling — uses raw closure since on_scroll_wheel
            // doesn't accept cx.listener pattern at this GPUI rev
            .on_scroll_wheel({
                let term = self.terminal.as_ref().map(|t| t.term.clone());
                let cell_h = self.cell_height;
                move |event: &ScrollWheelEvent, _window: &mut Window, _cx: &mut App| {
                    let Some(ref term) = term else { return };
                    let delta = event.delta.pixel_delta(px(cell_h));
                    let lines = (f32::from(delta.y) / cell_h) as i32;
                    if lines != 0 {
                        use alacritty_terminal::grid::Scroll;
                        term.lock().scroll_display(Scroll::Delta(lines));
                    }
                }
            })
            .children(row_elements)
            .into_any_element()
    }
}
