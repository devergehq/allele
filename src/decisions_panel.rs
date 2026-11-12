//! The decision strip (DEV-1267): every open question sessions have posted for
//! the developer, in one place, directly under the attention bar.
//!
//! The header carries the count and stays visible when the rows are collapsed,
//! so the number can be read without opening the list. Clicking an option
//! answers the question; the note field above the rows goes with the next
//! answer clicked. "History" swaps the open rows for answered, withdrawn and
//! expired ones.
//!
//! The strip shows while any question is open, or while history is shown —
//! `allele.toggle_decision_history` opens it with nothing open.

use gpui::*;

use crate::app_state::AppState;
use crate::decisions::{ago, Decision, DecisionState, Delivery};
use crate::icon::{icon, name as icons};
use crate::text_input::TextInput;
use crate::theme::theme;

/// Rows scroll past this height so the strip can never take the window.
const MAX_ROWS_HEIGHT: f32 = 320.0;

impl AppState {
    /// The strip, or `None` when nothing is open and history is not shown.
    pub(crate) fn render_decisions_panel(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let open_count = self.decisions.book.open().len();
        let show_history = self.decisions.show_history;
        if open_count == 0 && !show_history {
            return None;
        }
        if self.decisions.note.is_none() {
            self.decisions.note = Some(cx.new(|cx| {
                TextInput::new(
                    cx,
                    "",
                    "Optional note — sent with the next option you click",
                )
            }));
        }

        let collapsed = self.decisions.collapsed;
        let mut strip = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme().bg_base)
            .border_b_1()
            .border_color(theme().border_default)
            .child(self.render_decisions_header(open_count, collapsed, show_history, cx));
        if collapsed {
            return Some(strip);
        }

        let mut rows = div()
            .id("decision-rows")
            .w_full()
            .flex()
            .flex_col()
            .max_h(px(MAX_ROWS_HEIGHT))
            .overflow_y_scroll();

        if show_history {
            let history = self.decisions.book.history();
            if history.is_empty() {
                rows = rows.child(dim_line("No answered questions yet."));
            }
            for (idx, d) in history.into_iter().enumerate() {
                rows = rows.child(self.render_history_row(idx, d));
            }
        } else {
            if let Some(note) = self.decisions.note.clone() {
                rows = rows.child(
                    div()
                        .w_full()
                        .px(px(12.0))
                        .py(px(4.0))
                        .text_size(px(11.0))
                        .child(note),
                );
            }
            let this: &Self = self;
            for (idx, d) in this.decisions.book.open().into_iter().enumerate() {
                rows = rows.child(this.render_open_row(idx, d, cx));
            }
        }

        strip = strip.child(rows);
        Some(strip)
    }

    fn render_decisions_header(
        &self,
        open: usize,
        collapsed: bool,
        show_history: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let headline = match open {
            0 => "No open decisions".to_string(),
            1 => "1 decision for you".to_string(),
            n => format!("{n} decisions for you"),
        };
        let color = if open > 0 {
            theme().attention
        } else {
            theme().text_dim
        };
        let chevron = if collapsed {
            icons::CHEVRON_RIGHT
        } else {
            icons::CHEVRON_DOWN
        };

        div()
            .w_full()
            .bg(theme().bg_raised)
            .flex()
            .flex_row()
            .items_center()
            .child(
                div()
                    .id("decisions-header")
                    .flex_1()
                    .px(px(12.0))
                    .py(px(5.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme().bg_hover))
                    .child(icon(icons::HELP_CIRCLE, 13.0, color))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.0))
                            .text_color(theme().text_primary)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(headline),
                    )
                    .child(icon(chevron, 12.0, theme().text_secondary))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _e, _w, cx| {
                            this.decisions.collapsed = !this.decisions.collapsed;
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .id("decisions-history-toggle")
                    .px(px(10.0))
                    .py(px(5.0))
                    .cursor_pointer()
                    .text_size(px(10.0))
                    .text_color(if show_history {
                        theme().text_primary
                    } else {
                        theme().text_secondary
                    })
                    .hover(|s| s.bg(theme().bg_hover))
                    .child(if show_history { "Open" } else { "History" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _e, _w, cx| {
                            this.decisions.show_history = !this.decisions.show_history;
                            this.decisions.collapsed = false;
                            cx.notify();
                        }),
                    ),
            )
    }

    /// The live name of the asking session, or the one it had when it asked.
    fn asker(&self, d: &Decision) -> String {
        self.projects
            .iter()
            .flat_map(|p| p.sessions.iter())
            .find(|s| s.id == d.session_id)
            .map(|s| s.label.clone())
            .unwrap_or_else(|| format!("{} (gone)", d.session_label))
    }

    fn render_open_row(&self, idx: usize, d: &Decision, cx: &mut Context<Self>) -> Div {
        let mut meta = format!("{} · {}", self.asker(d), ago(d.posted_at));
        if !d.unblocks.is_empty() {
            meta.push_str(&format!(" · unblocks: {}", d.unblocks.join(", ")));
        }

        let mut options = div().flex().flex_row().flex_wrap().gap(px(6.0));
        for (i, option) in d.options.iter().enumerate() {
            let assumed = i == d.assumed;
            let id = d.id.clone();
            let label = if assumed {
                format!("{option}  (assumed)")
            } else {
                option.clone()
            };
            options = options.child(
                div()
                    .id(SharedString::from(format!("decision-{}-{i}", d.id)))
                    .cursor_pointer()
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(if assumed {
                        theme().accent
                    } else {
                        theme().border_default
                    })
                    .bg(theme().bg_raised)
                    .text_size(px(11.0))
                    .text_color(theme().text_primary)
                    .hover(|s| s.bg(theme().bg_hover))
                    .child(label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _w, cx| {
                            this.answer_decision(&id, i, cx);
                        }),
                    ),
            );
        }

        row(idx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_primary)
                    .child(d.question.clone()),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme().text_secondary)
                    .child(meta),
            )
            .child(options)
    }

    fn render_history_row(&self, idx: usize, d: &Decision) -> Div {
        let assumed = d.options.get(d.assumed).cloned().unwrap_or_default();
        let outcome = match (&d.state, &d.answer) {
            (DecisionState::Answered, Some(a)) => {
                let chosen = d.options.get(a.choice).cloned().unwrap_or_default();
                let delivery = match a.delivery {
                    Delivery::Waiting => "waiting for the session to be idle",
                    Delivery::Sent { .. } => "sent",
                    Delivery::SessionGone => "session gone, not sent",
                };
                let mut s = format!("Chose: {chosen} · assumed: {assumed} · {delivery}");
                if let Some(note) = &a.note {
                    s.push_str(&format!(" · note: {note}"));
                }
                s
            }
            (state, _) => format!("{} · assumed: {assumed}", capitalise(state.label())),
        };
        let when = ago(d.closed_at.unwrap_or(d.posted_at));

        row(idx)
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_secondary)
                    .child(d.question.clone()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme().text_primary)
                    .child(outcome),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme().text_dim)
                    .child(format!("{} · {when}", self.asker(d))),
            )
    }
}

fn row(idx: usize) -> Div {
    div()
        .w_full()
        .px(px(12.0))
        .py(px(6.0))
        .bg(if idx.is_multiple_of(2) {
            theme().bg_base
        } else {
            theme().bg_row_alt
        })
        .flex()
        .flex_col()
        .gap(px(3.0))
}

fn dim_line(text: &'static str) -> Div {
    div()
        .px(px(12.0))
        .py(px(6.0))
        .text_size(px(11.0))
        .text_color(theme().text_dim)
        .child(text)
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}
