//! Scratch Pad — a compose overlay that lets the user write a multi-line
//! message (with file/image attachments) and paste it into the active Claude
//! Code session on submit.
//!
//! Opened with Cmd+K. Submitted with Cmd+Enter. Cancelled with Escape or
//! by clicking the backdrop.

mod clipboard_image;
mod editor;

use editor::{KeyOutcome, Pos, ScratchEditor};
use gpui::*;
use std::path::PathBuf;

/// Events emitted by the scratch pad that the AppState listens for to drive
/// the actual PTY write and modal dismissal.
#[derive(Debug, Clone)]
pub enum ScratchPadEvent {
    Send { text: String, attachments: Vec<PathBuf> },
    Close,
}

impl EventEmitter<ScratchPadEvent> for ScratchPad {}

pub struct ScratchPad {
    editor: ScratchEditor,
    attachments: Vec<PathBuf>,
}

impl ScratchPad {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            editor: ScratchEditor::new(cx),
            attachments: Vec::new(),
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.editor.focus.clone()
    }

    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this: &mut Self, cx| {
                    this.attachments.extend(paths);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.text();
        // Nothing to send if both empty — just close.
        if text.is_empty() && self.attachments.is_empty() {
            cx.emit(ScratchPadEvent::Close);
            return;
        }
        cx.emit(ScratchPadEvent::Send {
            text,
            attachments: std::mem::take(&mut self.attachments),
        });
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ScratchPadEvent::Close);
    }

    fn try_paste_image(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(bytes) = clipboard_image::read_image_png_bytes() else { return false; };
        match clipboard_image::save_clipboard_png(&bytes) {
            Ok(path) => {
                self.attachments.push(path);
                cx.notify();
                true
            }
            Err(e) => {
                eprintln!("scratch pad: failed to save pasted image: {e}");
                false
            }
        }
    }

    /// Render the editor (lines + cursor) with per-char click handlers
    /// that reposition the cursor. Lives here rather than on the editor
    /// itself so `cx.listener` is in scope for the click closures.
    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selection = self.editor.selection_range();
        let cursor = self.editor.cursor();
        let mut col = div()
            .flex()
            .flex_col()
            .font_family("JetBrains Mono")
            .text_size(px(13.0))
            .text_color(rgb(0xcdd6f4));
        for (line_idx, line_text) in self.editor.lines().iter().enumerate() {
            col = col.child(self.render_line(cx, line_idx, line_text, cursor, selection));
        }
        col
    }

    fn render_line(
        &self,
        cx: &mut Context<Self>,
        line_idx: usize,
        text: &str,
        cursor: Pos,
        selection: Option<(Pos, Pos)>,
    ) -> Stateful<Div> {
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        let cursor_color = rgb(0xcdd6f4);
        let is_cursor_line = cursor.line == line_idx;

        // Row click handler — fires when the click lands in the row but
        // not on a child cell (i.e. in the empty flex space to the right
        // of the last character). Positions cursor at end of line.
        let mut row = div()
            .id(("scratch-line", line_idx))
            .flex()
            .flex_row()
            .min_h(px(19.0))
            .w_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut Self, event: &MouseDownEvent, _w, cx| {
                    let extend = event.modifiers.shift;
                    let line_end = this.editor.line_char_count(line_idx);
                    this.editor.set_cursor(Pos { line: line_idx, col: line_end }, extend);
                    this.editor.focus.focus(_w, cx);
                    cx.notify();
                }),
            );

        let sel_range = selection.and_then(|(s, e)| {
            if line_idx < s.line || line_idx > e.line {
                return None;
            }
            let start_col = if line_idx == s.line { s.col } else { 0 };
            let end_col = if line_idx == e.line { e.col } else { len + 1 };
            Some((start_col, end_col))
        });

        for i in 0..=len {
            // Cursor bar at column i (before char i)
            if is_cursor_line && cursor.col == i {
                row = row.child(
                    div()
                        .w(px(2.0))
                        .min_w(px(2.0))
                        .bg(cursor_color)
                        .h(px(17.0)),
                );
            }
            if i == len { break; }

            let ch = chars[i];
            let ch_str: String = ch.to_string();
            let in_sel = sel_range
                .map(|(s, e)| i >= s && i < e)
                .unwrap_or(false);
            // Each char cell is its own click target → cursor lands
            // before that char. stop_propagation prevents the row's
            // end-of-line handler from also firing.
            // Pack (line, col) into a single id integer — ElementId's
            // From impls don't cover 3-tuples, and lines won't exceed
            // 2^32 cols in any sane input.
            let cell_id = ((line_idx as u64) << 32) | (i as u64);
            let cell_base = div()
                .id(("scratch-cell", cell_id as usize))
                .cursor_text()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, event: &MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        let extend = event.modifiers.shift;
                        this.editor.set_cursor(Pos { line: line_idx, col: i }, extend);
                        this.editor.focus.focus(_w, cx);
                        cx.notify();
                    }),
                )
                .child(ch_str);
            let cell = if in_sel {
                cell_base.bg(rgb(0x45475a))
            } else {
                cell_base
            };
            row = row.child(cell);
        }

        // Empty-line selection bar — show a thin highlight if the selection
        // covers this line's newline but the line has no chars.
        if len == 0 {
            if let Some((s, e)) = sel_range {
                if s == 0 && e > 0 {
                    row = row.child(
                        div()
                            .w(px(6.0))
                            .bg(rgb(0x45475a))
                            .h(px(17.0)),
                    );
                }
            }
        }

        row
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(14.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(rgb(0x313244))
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0xcdd6f4))
                    .child("Scratch Pad"),
            )
            .child(
                div()
                    .id("scratch-close")
                    .cursor_pointer()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .text_size(px(14.0))
                    .text_color(rgb(0x6c7086))
                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                    .child("×")
                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _ev, _w, cx| {
                        this.close(cx);
                    })),
            )
    }

    fn render_chips(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.0))
            .px(px(14.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(rgb(0x313244));

        for (idx, path) in self.attachments.iter().enumerate() {
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            row = row.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .bg(rgb(0x313244))
                    .text_size(px(11.0))
                    .text_color(rgb(0xcdd6f4))
                    .child(label)
                    .child(
                        div()
                            .id(("scratch-chip-remove", idx))
                            .cursor_pointer()
                            .text_color(rgb(0x6c7086))
                            .hover(|s| s.text_color(rgb(0xf38ba8)))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this: &mut Self, _ev, _w, cx| {
                                    if idx < this.attachments.len() {
                                        this.attachments.remove(idx);
                                        cx.notify();
                                    }
                                }),
                            ),
                    ),
            );
        }

        row = row.child(
            div()
                .id("scratch-attach-btn")
                .cursor_pointer()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(rgb(0x45475a))
                .text_size(px(11.0))
                .text_color(rgb(0xa6adc8))
                .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                .child("+ Attach file")
                .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _ev, _w, cx| {
                    this.pick_files(cx);
                })),
        );

        row
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(14.0))
            .py(px(10.0))
            .border_t_1()
            .border_color(rgb(0x313244))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(0x6c7086))
                    .child("Esc to cancel · Cmd+Enter to send"),
            )
            .child(
                div()
                    .id("scratch-send-btn")
                    .cursor_pointer()
                    .px(px(14.0))
                    .py(px(5.0))
                    .rounded(px(4.0))
                    .bg(rgb(0x89b4fa))
                    .text_size(px(11.0))
                    .text_color(rgb(0x1e1e2e))
                    .font_weight(FontWeight::BOLD)
                    .hover(|s| s.bg(rgb(0x74c7ec)))
                    .child("Send")
                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _ev, _w, cx| {
                        this.submit(cx);
                    })),
            )
    }
}

impl Render for ScratchPad {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_focus = self.editor.focus.clone();

        // Backdrop covers the whole app; clicking it closes.
        let backdrop = div()
            .id("scratch-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(rgba(0x00000099))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _ev, _w, cx| {
                    this.close(cx);
                }),
            );

        let card = div()
            .id("scratch-card")
            .w(px(720.0))
            .max_h(px(560.0))
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded(px(8.0))
            .shadow_lg()
            .overflow_hidden()
            // Stop clicks inside the card from reaching the backdrop's
            // "click-to-close" handler.
            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| {
                cx.stop_propagation();
            })
            .child(self.render_header(cx))
            .child(self.render_chips(cx))
            .child(
                div()
                    .id("scratch-editor-scroll")
                    .flex_1()
                    .min_h(px(240.0))
                    .overflow_y_scroll()
                    .px(px(14.0))
                    .py(px(10.0))
                    .track_focus(&editor_focus)
                    .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                        // Intercept Cmd+V first so we can check for image
                        // data before GPUI's text-only clipboard is read.
                        let key = event.keystroke.key.as_str();
                        let mods = &event.keystroke.modifiers;
                        if key == "v" && mods.platform && !mods.alt && !mods.shift {
                            if this.try_paste_image(cx) {
                                return;
                            }
                            // Fall through to editor for text paste.
                        }
                        match this.editor.handle_key(event, cx) {
                            KeyOutcome::Handled => cx.notify(),
                            KeyOutcome::Send => this.submit(cx),
                            KeyOutcome::Close => this.close(cx),
                            KeyOutcome::Ignored => {}
                        }
                    }))
                    // Click in the empty space below the text → cursor
                    // jumps to end of document. Child line / cell handlers
                    // stop_propagation so this only fires for the padding
                    // region below the last line.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, event: &MouseDownEvent, window, cx| {
                            let extend = event.modifiers.shift;
                            let last_line = this.editor.lines().len().saturating_sub(1);
                            let end_col = this.editor.line_char_count(last_line);
                            this.editor.set_cursor(Pos { line: last_line, col: end_col }, extend);
                            this.editor.focus.focus(window, cx);
                            cx.notify();
                        }),
                    )
                    .child(self.render_editor(cx)),
            )
            .child(self.render_footer(cx));

        backdrop.child(card)
    }
}
