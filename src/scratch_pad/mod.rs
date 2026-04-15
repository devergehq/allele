//! Scratch Pad — a compose overlay that lets the user write a multi-line
//! message (with file/image attachments) and paste it into the active Claude
//! Code session on submit.
//!
//! Opened with Cmd+K. Submitted with Cmd+Enter. Cancelled with Escape or
//! by clicking the backdrop.

mod clipboard_image;
mod editor;

use editor::{KeyOutcome, ScratchEditor};
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
                    .child(self.editor.render()),
            )
            .child(self.render_footer(cx));

        backdrop.child(card)
    }
}
