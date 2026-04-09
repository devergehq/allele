use super::pty_terminal::{PtyTerminal, TermSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use gpui::*;
use std::time::Duration;

const CELL_WIDTH: f32 = 7.6; // Approximate monospace char width at 13px
const CELL_HEIGHT: f32 = 18.0; // Line height
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 4;

/// GPUI View wrapping a PTY-backed terminal
pub struct TerminalView {
    terminal: Option<PtyTerminal>,
    error: Option<String>,
    last_cols: u16,
    last_rows: u16,
}

impl TerminalView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let terminal = match PtyTerminal::new(TermSize::default()) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("Failed to create PTY: {e}");
                return Self {
                    terminal: None,
                    error: Some(format!("Failed to create PTY: {e}")),
                    last_cols: 80,
                    last_rows: 24,
                };
            }
        };

        // Poll for PTY events on a timer and re-render
        cx.spawn_in(window, async |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;

                let should_redraw = this
                    .update(cx, |this: &mut Self, _cx: &mut Context<Self>| {
                        if let Some(ref terminal) = this.terminal {
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

        Self {
            terminal,
            error: None,
            last_cols: 80,
            last_rows: 24,
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

    /// Compute terminal grid size from pixel dimensions
    fn compute_size(width_px: f32, height_px: f32) -> TermSize {
        let cols = (width_px / CELL_WIDTH).floor() as u16;
        let rows = (height_px / CELL_HEIGHT).floor() as u16;
        TermSize {
            cols: cols.max(MIN_COLS),
            rows: rows.max(MIN_ROWS),
            cell_width: CELL_WIDTH as u16,
            cell_height: CELL_HEIGHT as u16,
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                        let i = current_italic;
                        let mut el = div().text_color(fg);
                        if let Some(bg_color) = bg {
                            el = el.bg(bg_color);
                        }
                        if b {
                            el = el.font_weight(FontWeight::BOLD);
                        }
                        // italic not yet supported on Div at this GPUI rev
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
                // italic not yet supported on Div at this GPUI rev
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
            .font_family("JetBrains Mono")
            .text_size(px(13.0))
            .line_height(px(CELL_HEIGHT))
            .overflow_hidden()
            .focusable()
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, _cx| {
                let Some(ref terminal) = this.terminal else { return };

                // Forward keystroke to PTY
                if let Some(ref key_char) = event.keystroke.key_char {
                    if event.keystroke.modifiers.control {
                        // Handle ctrl+key combos
                        if let Some(ch) = key_char.chars().next() {
                            let ctrl_byte = (ch as u8).wrapping_sub(b'a').wrapping_add(1);
                            terminal.write(&[ctrl_byte]);
                            return;
                        }
                    }
                    terminal.write(key_char.as_bytes());
                } else {
                    let bytes: Option<&[u8]> = match event.keystroke.key.as_str() {
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
                    if let Some(bytes) = bytes {
                        terminal.write(bytes);
                    }
                }
            }))
            // Resize detection — fires when the terminal div is laid out
            .on_mouse_move(cx.listener(move |this: &mut Self, _event: &MouseMoveEvent, window, _cx| {
                // Use mouse move as a proxy to check bounds — lightweight resize detection
                let viewport = window.viewport_size();
                let sidebar_width = px(240.0);
                let available_width = viewport.width - sidebar_width;
                let available_height = viewport.height;

                if available_width > px(100.0) && available_height > px(100.0) {
                    let new_size = Self::compute_size(
                        f32::from(available_width),
                        f32::from(available_height),
                    );
                    if new_size.cols != this.last_cols || new_size.rows != this.last_rows {
                        this.last_cols = new_size.cols;
                        this.last_rows = new_size.rows;
                        if let Some(ref mut terminal) = this.terminal {
                            terminal.resize(new_size);
                        }
                    }
                }
            }))
            .children(row_elements)
            .into_any_element()
    }
}
