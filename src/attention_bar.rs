//! Attention bar — the strip above the main tab strip listing every session
//! whose agent is blocked on input, so the user can see who is waiting without
//! walking the sidebar.
//!
//! Extracted from `src/main.rs` when DEV-525 pushed that file past the §7.7
//! size ratchet. The bar is a self-contained unit: it reads session status and
//! `attention_bar_collapsed`, and emits `SidebarAction::ToggleAttentionBar`
//! and `SessionAction::SelectSession`. Nothing else in the render tree
//! depends on its internals.

use gpui::*;

use crate::actions::{SessionAction, SidebarAction};
use crate::app_state::{AppState, ATTENTION_BAR_MAX_ROWS, ATTENTION_BAR_ROW_HEIGHT};
use crate::icon::{icon, name as icons};
use crate::session;
use crate::session::AttentionKind;
use crate::theme::theme;

impl AppState {
    /// Render the attention bar — a summary header plus a row per session in
    /// AwaitingInput state, showing what each session wants so the user can
    /// act without switching. Returns `None` when no sessions need attention
    /// (renders nothing).
    ///
    /// The header collapses the rows away (DEV-525): at twenty-odd waiting
    /// sessions the unbounded list ate most of the viewport and the terminal
    /// below it was unusable. Collapsed keeps the count — the part worth
    /// glancing at — and gives the space back. Even expanded the list is
    /// capped at `ATTENTION_BAR_MAX_ROWS` rows tall and scrolls past that, so
    /// the bar can never take the window no matter how many agents are blocked.
    pub(crate) fn render_attention_bar(&self, cx: &mut Context<Self>) -> Option<Div> {
        // Each item: (project_idx, session_idx, label, kind, tool, summary)
        let mut items: Vec<(usize, usize, String, AttentionKind, String, String)> = Vec::new();

        for (p_idx, project) in self.projects.iter().enumerate() {
            for (s_idx, session) in project.sessions.iter().enumerate() {
                if session.status != session::SessionStatus::AwaitingInput {
                    continue;
                }
                let label = session.label.clone();
                let (kind, tool, summary) = if let Some(ref ctx) = session.attention_context {
                    let tool = ctx.tool_name.clone().unwrap_or_default();
                    // Prefer the concrete thing being asked about over Claude
                    // Code's generic copy: "npm install" tells the user more
                    // than "Claude needs your permission" does. The kind's own
                    // label carries the generic part.
                    let summary = ctx
                        .tool_input_summary
                        .clone()
                        .or_else(|| ctx.informative_message().map(str::to_string))
                        .unwrap_or_else(|| ctx.kind.label().into());
                    (ctx.kind, tool, summary)
                } else {
                    // No context attached. `None` sorts and renders as blocking
                    // for the same reason it does in the sidebar: under-warning
                    // about a stuck agent is the worse error.
                    (
                        AttentionKind::Permission,
                        String::new(),
                        "Waiting for input".into(),
                    )
                };
                items.push((p_idx, s_idx, label, kind, tool, summary));
            }
        }

        if items.is_empty() {
            return None;
        }

        // Blocked first, then merely idle — the bar is a triage queue, and the
        // rows that need a human have to be the ones above the scroll fold
        // (DEV-788; the cap is ATTENTION_BAR_MAX_ROWS).
        items.sort_by_key(|(_, _, _, kind, _, _)| match kind {
            AttentionKind::Permission => 0u8,
            AttentionKind::Question => 1,
            AttentionKind::Idle => 2,
        });

        let collapsed = self.user_settings.attention_bar_collapsed;
        let blocked = items.iter().filter(|i| i.3.is_blocking()).count();
        let idle = items.len() - blocked;

        let mut bar = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme().bg_base)
            .border_b_1()
            .border_color(theme().border_default)
            .child(self.render_attention_bar_header(blocked, idle, collapsed, cx));

        if collapsed {
            return Some(bar);
        }

        // Cap the list rather than the bar: the header must stay visible, so
        // the max height belongs to the scrolling row container beneath it.
        let mut rows = div()
            .id("attention-bar-rows")
            .w_full()
            .flex()
            .flex_col()
            .max_h(px(ATTENTION_BAR_ROW_HEIGHT * ATTENTION_BAR_MAX_ROWS))
            .overflow_y_scroll();

        for (idx, (p_idx, s_idx, label, kind, tool, summary)) in items.into_iter().enumerate() {
            let row_id = SharedString::from(format!("attention-row-{p_idx}-{s_idx}"));

            let tool_display = if tool.is_empty() {
                String::new()
            } else {
                format!("{tool}: ")
            };

            let is_active = self
                .active
                .map(|c| c.project_idx == p_idx && c.session_idx == s_idx)
                .unwrap_or(false);

            let bg = if is_active {
                theme().bg_attention
            } else if idx % 2 == 0 {
                theme().bg_base
            } else {
                theme().bg_row_alt
            };

            let mut row = div()
                .id(row_id)
                .w_full()
                .px(px(12.0))
                .py(px(5.0))
                .bg(bg)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(icon(kind.icon_name(), 13.0, kind.color()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_x_hidden()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _event, _window, cx| {
                                this.pending_action = Some(
                                    SessionAction::SelectSession {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    }
                                    .into(),
                                );
                                cx.notify();
                            }),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme().text_primary)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.0))
                                .text_color(kind.color())
                                .child(kind.label()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_x_hidden()
                                .text_size(px(11.0))
                                .text_color(theme().text_secondary)
                                .child(format!("{tool_display}{summary}")),
                        ),
                );

            // Allow is offered for permission prompts and nothing else.
            //
            // It sends a bare `\r`, which grants a permission but on a
            // *question* tool silently picks whichever option the TUI has
            // highlighted — an answer the user never saw, with nothing
            // recording which one it was. The previous gate was
            // `tool_name.is_some()`, and `AskUserQuestion` has a tool name:
            // 49 of 364 permission notifications in a 156-session sample were
            // exactly that case (DEV-788). Question and Idle rows get the
            // click-to-switch row only.
            if kind == AttentionKind::Permission {
                let allow_id = SharedString::from(format!("attention-allow-{p_idx}-{s_idx}"));
                row = row.child(
                    div()
                        .id(allow_id)
                        .cursor_pointer()
                        .px(px(8.0))
                        .py(px(2.0))
                        .rounded(px(6.0))
                        .bg(theme().bg_raised)
                        .text_size(px(10.0))
                        .text_color(theme().success) // green
                        .hover(|s| s.bg(theme().bg_hover))
                        .child("Allow")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _event, _window, cx| {
                                if let Some(session) = this
                                    .projects
                                    .get_mut(p_idx)
                                    .and_then(|p| p.sessions.get_mut(s_idx))
                                {
                                    if let Some(ref tv) = session.terminal_view {
                                        tv.read(cx).send_input(b"\r");
                                    }
                                    session.status = session::SessionStatus::Running;
                                    session.attention_context = None;
                                }
                                cx.notify();
                            }),
                        ),
                );
            }

            rows = rows.child(row);
        }

        bar = bar.child(rows);
        Some(bar)
    }

    /// The attention bar's summary header: alert glyph, waiting count, and a
    /// chevron that reads as the collapse control. Clicking anywhere on the
    /// row toggles — it is the only affordance for the bar, so the whole
    /// strip is the hit target rather than the chevron alone.
    fn render_attention_bar_header(
        &self,
        blocked: usize,
        idle: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let chevron = if collapsed {
            icons::CHEVRON_RIGHT
        } else {
            icons::CHEVRON_DOWN
        };

        // "18 sessions waiting for your input" was true and useless: it counted
        // blocked agents together with ones that simply had nothing to do, so
        // the number never told the user whether to act (DEV-788). Split, the
        // headline answers the only question being asked of it.
        let plural = |n: usize| if n == 1 { "session" } else { "sessions" };
        let (glyph, glyph_color, headline) = match (blocked, idle) {
            (0, n) => (
                icons::CIRCLE,
                theme().text_dim,
                format!("{n} {} waiting for a prompt", plural(n)),
            ),
            (b, 0) => (
                icons::LOCK,
                theme().attention,
                format!("{b} {} blocked on you", plural(b)),
            ),
            (b, n) => (
                icons::LOCK,
                theme().attention,
                format!(
                    "{b} {} blocked on you · {n} waiting for a prompt",
                    plural(b)
                ),
            ),
        };

        div()
            .id("attention-bar-header")
            .w_full()
            .px(px(12.0))
            .py(px(5.0))
            .bg(theme().bg_raised)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme().bg_hover))
            .child(icon(glyph, 13.0, glyph_color))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(11.0))
                    .text_color(theme().text_primary)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(headline),
            )
            .child(icon(chevron, 12.0, theme().text_secondary))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut Self, _event, _window, cx| {
                    this.pending_action = Some(SidebarAction::ToggleAttentionBar.into());
                    cx.notify();
                }),
            )
    }
}
