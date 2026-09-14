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
        // Each item: (project_idx, session_idx, label, tool, summary, is_permission_prompt)
        let mut items: Vec<(usize, usize, String, String, String, bool)> = Vec::new();

        for (p_idx, project) in self.projects.iter().enumerate() {
            for (s_idx, session) in project.sessions.iter().enumerate() {
                if session.status != session::SessionStatus::AwaitingInput {
                    continue;
                }
                let label = session.label.clone();
                let (tool, summary, is_permission) =
                    if let Some(ref ctx) = session.attention_context {
                        let tool = ctx.tool_name.clone().unwrap_or_default();
                        let summary = ctx
                            .tool_input_summary
                            .clone()
                            .or_else(|| ctx.message.clone())
                            .unwrap_or_else(|| "Waiting for input".into());
                        // Permission prompts: either tool_name is set (rich payload)
                        // or the message contains "permission" (Claude Code's
                        // Notification hook doesn't include tool details, only the
                        // message text like "Claude needs your permission").
                        let is_perm = ctx.tool_name.is_some()
                            || ctx
                                .message
                                .as_deref()
                                .map(|m| m.contains("permission"))
                                .unwrap_or(false);
                        (tool, summary, is_perm)
                    } else {
                        (String::new(), "Waiting for input".into(), false)
                    };
                items.push((p_idx, s_idx, label, tool, summary, is_permission));
            }
        }

        if items.is_empty() {
            return None;
        }

        let collapsed = self.user_settings.attention_bar_collapsed;
        let count = items.len();

        let mut bar = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme().bg_base)
            .border_b_1()
            .border_color(theme().border_default)
            .child(self.render_attention_bar_header(count, collapsed, cx));

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

        for (idx, (p_idx, s_idx, label, tool, summary, is_permission)) in
            items.into_iter().enumerate()
        {
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
                .child(icon(icons::ALERT_TRIANGLE, 13.0, theme().attention))
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
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_x_hidden()
                                .text_size(px(11.0))
                                .text_color(theme().text_secondary)
                                .child(format!("{tool_display}{summary}")),
                        ),
                );

            // Only show Allow for real permission prompts (tool_name present).
            // Informational notifications (ResponseReady, idle) just get the
            // click-to-switch row — no button that would inject keystrokes.
            if is_permission {
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
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let plural = if count == 1 { "session" } else { "sessions" };
        let chevron = if collapsed {
            icons::CHEVRON_RIGHT
        } else {
            icons::CHEVRON_DOWN
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
            .child(icon(icons::ALERT_TRIANGLE, 13.0, theme().attention))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(11.0))
                    .text_color(theme().text_primary)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{count} {plural} waiting for your input")),
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
