//! Conversation picker — asks which Claude conversation a workspace should
//! resume, when Allele cannot answer that safely on its own.
//!
//! Shown only when [`crate::conversations::resume_is_ambiguous`] holds: the
//! workspace has more than one conversation on disk and the pointer Allele would
//! otherwise resume is not the most recent one. Every other resume is untouched.
//!
//! Design notes, in response to how this failure actually presented:
//!
//! - The rows lead with the **last human message**. Forks of one lineage share an
//!   opening message, a uuid history and a `custom-title`, so nothing near the
//!   start of a transcript can tell them apart.
//! - Escape and the backdrop **cancel the resume** and say so. The naming modal
//!   next door treats dismissal as acceptance; doing that here would resume a
//!   conversation the user never chose, which is the bug this feature exists to
//!   fix.
//! - The default is labelled "Most recent", not "Recommended". Recency is a
//!   decent guess, not an endorsement — mtime moves for reasons that have
//!   nothing to do with where the user's work is.
//! - The conversation Allele was previously linked to is marked, so switching
//!   away from it is a visible act rather than a silent one.

use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::accessibility::DENSE_CONTROL_MIN_HEIGHT;
use crate::conversations::{format_size, relative_time, Conversation};
use crate::theme::theme;

/// Events emitted by the picker that AppState listens for.
#[derive(Debug, Clone)]
pub enum ConversationPickerEvent {
    /// Resume this conversation and remember it for next time.
    Pick { conversation_id: String },
    /// Leave the session suspended and change nothing.
    Cancel,
}

impl EventEmitter<ConversationPickerEvent> for ConversationPickerModal {}

pub struct ConversationPickerModal {
    /// Newest first, previews already loaded.
    conversations: Vec<Conversation>,
    /// The conversation Allele would have resumed without asking.
    current: Option<String>,
    /// Session label, for the modal's subtitle.
    session_label: String,
    selected: usize,
    focus_handle: FocusHandle,
}

impl ConversationPickerModal {
    pub fn new(
        cx: &mut Context<Self>,
        session_label: String,
        conversations: Vec<Conversation>,
        current: Option<String>,
    ) -> Self {
        Self {
            conversations,
            current,
            session_label,
            // Newest is pre-selected: the list is sorted newest first.
            selected: 0,
            focus_handle: cx.focus_handle(),
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.conversations.len();
        if len == 0 {
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.selected = next;
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.conversations.get(self.selected) {
            cx.emit(ConversationPickerEvent::Pick {
                conversation_id: c.id.clone(),
            });
        } else {
            cx.emit(ConversationPickerEvent::Cancel);
        }
    }

    /// Dismissal always cancels. See the module note — inheriting the naming
    /// modal's "dismiss accepts the default" behaviour would defeat the point.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(ConversationPickerEvent::Cancel);
    }

    fn row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let c = &self.conversations[index];
        let selected = index == self.selected;
        let is_current = self.current.as_deref() == Some(c.id.as_str());
        let preview = c
            .preview()
            .unwrap_or("No messages in this conversation")
            .to_string();
        let short_id: String = c.id.chars().take(8).collect();
        let meta = format!(
            "{} · {} · {}",
            relative_time(c.modified),
            format_size(c.size_bytes),
            short_id
        );
        let id = c.id.clone();

        div()
            .id(SharedString::from(format!("conversation-row-{index}")))
            .cursor_pointer()
            .min_h(px(DENSE_CONTROL_MIN_HEIGHT))
            .px(px(12.0))
            .py(px(9.0))
            .rounded(px(8.0))
            .border_1()
            .when(selected, |s| s.border_color(theme().accent))
            .when(!selected, |s| s.border_color(theme().border_default))
            .bg(if selected {
                theme().bg_hover
            } else {
                theme().bg_raised
            })
            .hover(|s| s.bg(theme().bg_hover))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _e, _w, cx| {
                    cx.stop_propagation();
                    cx.emit(ConversationPickerEvent::Pick {
                        conversation_id: id.clone(),
                    });
                    let _ = this;
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme().text_faint)
                            .child(SharedString::from(meta)),
                    )
                    .when(index == 0, |d| {
                        d.child(badge("Most recent", theme().accent))
                    })
                    .when(is_current, |d| {
                        d.child(badge("Currently linked", theme().text_secondary))
                    }),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme().text_primary)
                    .child(SharedString::from(preview)),
            )
    }
}

fn badge(label: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(color)
        .text_size(px(9.0))
        .text_color(color)
        .child(label)
}

impl Focusable for ConversationPickerModal {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConversationPickerModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div()
            .id("conversation-picker-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.5))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.cancel(cx);
                }),
            );

        let count = self.conversations.len();
        let mut list = div()
            .id("conversation-picker-list")
            .max_h(px(340.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.0));
        for i in 0..count {
            list = list.child(self.row(i, cx));
        }

        let card = div()
            .id("conversation-picker-card")
            .w(px(520.0))
            .bg(theme().bg_base)
            .rounded(px(12.0))
            .border_1()
            .border_color(theme().border_default)
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| {
                cx.stop_propagation();
            })
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _w, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.cancel(cx),
                    "enter" => this.confirm(cx),
                    "down" => this.move_selection(1, cx),
                    "up" => this.move_selection(-1, cx),
                    _ => {}
                }
            }))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme().text_primary)
                    .child("Which conversation should this session continue?"),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme().text_secondary)
                    .child(SharedString::from(format!(
                        "“{}” has {count} Claude conversations in its workspace. \
                         They can look alike — compare the last message in each.",
                        self.session_label
                    ))),
            )
            .child(list)
            .child(
                // Two rows, not one. Sharing a row with the hint let the text
                // win the space fight and clipped the confirm button's label.
                // Stacking keeps the buttons at their natural width whatever
                // the hint says.
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme().text_faint)
                            .child("Cancelling leaves the session suspended. Nothing is deleted."),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .id("conversation-picker-cancel")
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .min_h(px(DENSE_CONTROL_MIN_HEIGHT))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .bg(theme().bg_hover)
                                    .hover(|s| s.bg(theme().bg_active))
                                    .text_size(px(11.0))
                                    .text_color(theme().text_secondary)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            this.cancel(cx);
                                        }),
                                    )
                                    .child("Cancel resume"),
                            )
                            .child(
                                div()
                                    .id("conversation-picker-confirm")
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .min_h(px(DENSE_CONTROL_MIN_HEIGHT))
                                    .px(px(10.0))
                                    .py(px(5.0))
                                    .rounded(px(6.0))
                                    .bg(theme().bg_active)
                                    .hover(|s| s.bg(theme().bg_hover))
                                    .text_size(px(11.0))
                                    .text_color(theme().text_primary)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            this.confirm(cx);
                                        }),
                                    )
                                    .child("Continue selected"),
                            ),
                    ),
            );

        let card = card.with_animation(
            "conversation-picker-in",
            Animation::new(std::time::Duration::from_millis(160))
                .with_easing(gpui::ease_out_quint()),
            |card, delta| card.opacity(delta).mt(px(12.0 * (1.0 - delta))),
        );
        backdrop.child(card).with_animation(
            "conversation-picker-backdrop-in",
            Animation::new(std::time::Duration::from_millis(120)),
            |el, delta| el.opacity(delta),
        )
    }
}
