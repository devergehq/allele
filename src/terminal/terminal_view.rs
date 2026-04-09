use super::pty_terminal::{PtyTerminal, TermSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use gpui::*;
use std::time::Duration;

/// GPUI View wrapping a PTY-backed terminal
pub struct TerminalView {
    terminal: Option<PtyTerminal>,
    error: Option<String>,
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
        }
    }

    fn ansi_to_hsla(color: &AnsiColor) -> Hsla {
        let rgba_val = match color {
            AnsiColor::Named(named) => match named {
                NamedColor::Black => 0x1e1e2eu32,
                NamedColor::Red => 0xf38ba8u32,
                NamedColor::Green => 0xa6e3a1u32,
                NamedColor::Yellow => 0xf9e2afu32,
                NamedColor::Blue => 0x89b4fau32,
                NamedColor::Magenta => 0xcba6f7u32,
                NamedColor::Cyan => 0x94e2d5u32,
                NamedColor::White => 0xcdd6f4u32,
                NamedColor::BrightBlack => 0x585b70u32,
                NamedColor::BrightRed => 0xf38ba8u32,
                NamedColor::BrightGreen => 0xa6e3a1u32,
                NamedColor::BrightYellow => 0xf9e2afu32,
                NamedColor::BrightBlue => 0x89b4fau32,
                NamedColor::BrightMagenta => 0xcba6f7u32,
                NamedColor::BrightCyan => 0x94e2d5u32,
                NamedColor::BrightWhite => 0xffffffu32,
                NamedColor::Foreground => 0xcdd6f4u32,
                NamedColor::Background => 0x1e1e2eu32,
                _ => 0xcdd6f4u32,
            },
            AnsiColor::Spec(rgb_color) => {
                let r = rgb_color.r as f32 / 255.0;
                let g = rgb_color.g as f32 / 255.0;
                let b = rgb_color.b as f32 / 255.0;
                return Hsla::from(Rgba { r, g, b, a: 1.0 });
            }
            AnsiColor::Indexed(idx) => match *idx {
                0 => 0x1e1e2eu32,
                1 => 0xf38ba8u32,
                2 => 0xa6e3a1u32,
                3 => 0xf9e2afu32,
                4 => 0x89b4fau32,
                5 => 0xcba6f7u32,
                6 => 0x94e2d5u32,
                7 => 0xcdd6f4u32,
                8..=15 => 0xcdd6f4u32,
                _ => 0xcdd6f4u32,
            },
        };
        let r = ((rgba_val >> 16) & 0xFF) as f32 / 255.0;
        let g = ((rgba_val >> 8) & 0xFF) as f32 / 255.0;
        let b = (rgba_val & 0xFF) as f32 / 255.0;
        Hsla::from(Rgba { r, g, b, a: 1.0 })
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

        let Some(ref terminal) = self.terminal else {
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

        // Build row strings with basic rendering
        let mut row_elements: Vec<AnyElement> = Vec::with_capacity(num_lines);

        for line_idx in 0..num_lines {
            let mut line_text = String::with_capacity(num_cols);

            for col_idx in 0..num_cols {
                let cell = &grid[Line(line_idx as i32)][Column(col_idx)];
                let c = cell.c;
                let ch = if c == '\0' { ' ' } else { c };
                line_text.push(ch);
            }

            // Trim trailing spaces for cleaner rendering
            let trimmed = line_text.trim_end();
            let display_text = if trimmed.is_empty() {
                " ".to_string() // Keep at least one space so the line has height
            } else {
                trimmed.to_string()
            };

            let is_cursor_line = line_idx == cursor_point.line.0 as usize;

            row_elements.push(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .child(
                        div()
                            .text_color(rgb(0xcdd6f4))
                            .child(display_text),
                    )
                    .into_any_element(),
            );
        }

        drop(term);

        // Build the terminal container with keyboard handling
        let term_ptr = &terminal.term as *const _;
        let pty_tx_ptr = &terminal.pty_tx as *const _;

        div()
            .id("terminal")
            .size_full()
            .bg(rgb(0x1e1e2e))
            .font_family("JetBrains Mono")
            .text_size(px(13.0))
            .line_height(px(18.0))
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
                    // Handle special keys
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
            .children(row_elements)
            .into_any_element()
    }
}
