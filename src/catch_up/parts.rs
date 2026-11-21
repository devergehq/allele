//! The three catch-up parts, drawn from their facts alone (DEV-1353, R2-parts).
//!
//! 1. [`prompt_line`]: what you last asked, how long ago, and one line of the
//!    reply.
//! 2. [`change_summary`]: what changed since you looked, file by file.
//! 3. [`wants_now_row`]: what the session needs from you, in four states.
//!
//! Each takes plain facts plus `now` and returns an element. None of them
//! reads git, the PTY, the transcript or `AppState`; the buttons call back
//! through [`WantsActions`] so the caller decides what Allow once means. The
//! strip in `mod.rs` is one frame for them; DEV-1272's remote window is
//! meant to be another.

use std::rc::Rc;

use gpui::*;

use super::facts::{duration_short, ChangeFacts, PromptFacts, WantsNow};
use crate::icon::{icon, name as icons};
use crate::theme::{theme, with_alpha, FONT_MONO};

/// A button callback.
pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// What the wants-now row's buttons do. Only the permission and
/// terminal-only states draw buttons.
#[derive(Clone)]
pub struct WantsActions {
    pub allow_once: Handler,
    pub deny: Handler,
    /// "Answer in terminal" and "Open the terminal".
    pub open_terminal: Handler,
}

/// Width of the "You asked" / "Claude" label column, so the prompt and the
/// reply start at the same x.
const LABEL_W: f32 = 92.0;

/// Most files listed by name before the rest are counted.
const MAX_FILES_LISTED: usize = 6;

fn rounded_box() -> Div {
    div()
        .rounded(px(6.0))
        .bg(theme().bg_sunken)
        .px(px(10.0))
        .py(px(6.0))
}

/// Part 1: "↳ You asked … 2h 14m ago" with the reply's first line below.
pub fn prompt_line(prompt: &PromptFacts, now: u64) -> Div {
    let t = theme();
    let mut asked = div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .flex_shrink_0()
                .w(px(LABEL_W))
                .pt(px(1.0))
                .child(icon(icons::CORNER_DOWN_RIGHT, 12.0, t.info))
                .child(
                    div()
                        .text_color(t.info)
                        .font_weight(FontWeight::MEDIUM)
                        .child("You asked"),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(t.text_body)
                .line_clamp(2)
                .child(prompt.text.clone()),
        );
    if let Some(at) = prompt.asked_at {
        asked = asked.child(
            div()
                .flex_shrink_0()
                .text_color(t.text_faint)
                .child(format!("{} ago", duration_short(now.saturating_sub(at)))),
        );
    }

    let mut col = rounded_box()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .text_size(px(12.0))
        .child(asked);
    if let Some(reply) = &prompt.reply {
        col = col.child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(LABEL_W))
                        .text_color(t.lavender)
                        .font_weight(FontWeight::MEDIUM)
                        .child("Claude"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(t.text_secondary)
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(reply.clone()),
                ),
        );
    }
    col
}

/// Part 2: the change summary. `None` when nothing changed — the strip shows
/// no summary rather than an empty one.
///
/// `so_far` labels it as work in progress ("CHANGED SO FAR").
pub fn change_summary(changes: &ChangeFacts, so_far: bool) -> Option<Div> {
    if changes.is_empty() {
        return None;
    }
    let t = theme();
    let (added, removed) = changes.totals();
    let suffix = if so_far { " so far" } else { "" };
    let state = if changes.uncommitted {
        "not committed"
    } else {
        "committed"
    };

    let heading = div()
        .flex()
        .flex_row()
        .justify_between()
        .text_size(px(10.0))
        .text_color(t.text_faint)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(if so_far {
            "CHANGED SO FAR"
        } else {
            "WHAT CHANGED SINCE YOU LOOKED"
        }))
        .child(format!("+{added} −{removed}{suffix} · {state}"));

    let n = changes.files.len();
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px(12.0))
        .gap_y(px(2.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(t.border_subtle)
        .text_size(px(11.5))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(icon(icons::FILE_TEXT, 12.0, t.text_muted))
                .child(
                    div()
                        .text_color(t.text_primary)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{n} {}", if n == 1 { "file" } else { "files" })),
                ),
        );
    for file in changes.files.iter().take(MAX_FILES_LISTED) {
        let mut chip = div()
            .flex()
            .flex_row()
            .gap(px(4.0))
            .font_family(FONT_MONO)
            .child(div().text_color(t.text_body).child(file.path.clone()));
        if file.binary {
            chip = chip.child(div().text_color(t.text_faint).child("binary"));
        } else {
            if file.added > 0 || file.removed == 0 {
                chip = chip.child(
                    div()
                        .text_color(t.success)
                        .child(format!("+{}", file.added)),
                );
            }
            if file.removed > 0 {
                chip = chip.child(
                    div()
                        .text_color(t.danger)
                        .child(format!("-{}", file.removed)),
                );
            }
        }
        row = row.child(chip);
    }
    if n > MAX_FILES_LISTED {
        row = row.child(
            div()
                .text_color(t.text_faint)
                .child(format!("and {} more", n - MAX_FILES_LISTED)),
        );
    }

    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(heading)
            .child(row),
    )
}

fn button(
    id: &'static str,
    label: &'static str,
    fg: Hsla,
    bg: Option<Hsla>,
    on: Handler,
) -> Stateful<Div> {
    let mut b = div()
        .id(id)
        .cursor_pointer()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(4.0))
        .text_size(px(11.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on(window, cx)
        });
    b = match bg {
        Some(bg) => b.bg(bg).hover(|s| s.opacity(0.8)),
        None => b.hover(|s| s.bg(theme().bg_hover)),
    };
    b
}

/// Part 3: the wants-now row. `actions` is `None` where nothing can answer
/// (a view-only remote); the row then draws without buttons.
pub fn wants_now_row(wants: &WantsNow, now: u64, actions: Option<&WantsActions>) -> Div {
    let t = theme();
    let (icon_name, icon_color, urgent) = match wants {
        WantsNow::Nothing { .. } => (icons::CHECK, t.text_secondary, false),
        WantsNow::Working { .. } => (icons::LOADER, t.info, false),
        WantsNow::Permission { .. } => (icons::LOCK, t.attention, true),
        WantsNow::TerminalOnly { .. } => (icons::SQUARE_TERMINAL, t.attention, true),
    };

    let mut text = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(1.0))
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_primary)
                .child(wants.title()),
        );
    match wants {
        WantsNow::Permission { target, tool, .. } => {
            text = text.child(
                div()
                    .text_size(px(11.5))
                    .font_family(FONT_MONO)
                    .text_color(t.text_secondary)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(target.clone().unwrap_or_else(|| tool.clone())),
            );
        }
        _ => {
            if let Some(detail) = wants.detail(now) {
                text = text.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(t.text_secondary)
                        .child(detail),
                );
            }
        }
    }

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .child(icon(icon_name, 13.0, icon_color))
        .child(text);
    row = if urgent {
        row.bg(with_alpha(t.attention, 0.07))
            .border_1()
            .border_color(with_alpha(t.attention, 0.45))
    } else {
        row.bg(t.bg_sunken)
    };

    if let Some(actions) = actions {
        match wants {
            WantsNow::Permission { .. } => {
                row = row.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_shrink_0()
                        .gap(px(6.0))
                        .child(button(
                            "catch-up-allow-once",
                            "Allow once",
                            t.success,
                            Some(with_alpha(t.success, 0.14)),
                            actions.allow_once.clone(),
                        ))
                        .child(button(
                            "catch-up-deny",
                            "Deny",
                            t.danger,
                            Some(with_alpha(t.danger, 0.14)),
                            actions.deny.clone(),
                        ))
                        .child(button(
                            "catch-up-answer-in-terminal",
                            "Answer in terminal",
                            t.info,
                            None,
                            actions.open_terminal.clone(),
                        )),
                );
            }
            WantsNow::TerminalOnly { .. } => {
                row = row.child(
                    button(
                        "catch-up-open-terminal",
                        "Open the terminal",
                        t.text_on_accent,
                        Some(t.accent),
                        actions.open_terminal.clone(),
                    )
                    .flex_shrink_0(),
                );
            }
            _ => {}
        }
    }
    row
}
