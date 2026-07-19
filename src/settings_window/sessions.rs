//! Sessions settings section — clone cleanup paths + session-creation toggles.

use gpui::*;

use super::widgets::{card, input_frame, section_note, section_title, toggle_switch};
use super::SettingsWindowState;
use crate::icon::{icon, name as icons};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::theme;
use crate::AppState;

/// Owns the cleanup-path list, its draft input, and the two session toggles.
pub(super) struct SessionsSection {
    cleanup_paths: Vec<String>,
    draft_input: Entity<TextInput>,
    git_pull_before_new_session: bool,
    promote_attention_sessions: bool,
}

impl SessionsSection {
    pub(super) fn new(
        cx: &mut Context<SettingsWindowState>,
        initial_paths: Vec<String>,
        git_pull_before_new_session: bool,
        promote_attention_sessions: bool,
    ) -> Self {
        let draft_input =
            cx.new(|cx| TextInput::new(cx, "", "Add a path (e.g. tmp/pids/server.pid)"));
        cx.subscribe(&draft_input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                this.sessions.commit_draft(value, &this.app, cx);
                input.update(cx, |i, cx| i.set_text_silent("", cx));
            }
        })
        .detach();

        Self {
            cleanup_paths: initial_paths,
            draft_input,
            git_pull_before_new_session,
            promote_attention_sessions,
        }
    }

    fn push_cleanup_paths(
        &self,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let paths = self.cleanup_paths.clone();
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action = Some(crate::SettingsAction::UpdateCleanupPaths(paths).into());
            cx.notify();
        })
        .ok();
    }

    fn commit_draft(
        &mut self,
        value: String,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.cleanup_paths.iter().any(|p| p == trimmed) {
            self.cleanup_paths.push(trimmed.to_string());
            self.push_cleanup_paths(app, cx);
        }
        cx.notify();
    }

    fn remove_path(
        &mut self,
        idx: usize,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        if idx < self.cleanup_paths.len() {
            self.cleanup_paths.remove(idx);
            self.push_cleanup_paths(app, cx);
            cx.notify();
        }
    }

    fn push_git_pull(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let value = self.git_pull_before_new_session;
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action =
                Some(crate::SettingsAction::UpdateGitPullBeforeNewSession(value).into());
            cx.notify();
        })
        .ok();
    }

    fn push_promote(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let value = self.promote_attention_sessions;
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action =
                Some(crate::SettingsAction::UpdatePromoteAttentionSessions(value).into());
            cx.notify();
        })
        .ok();
    }

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let mut list = div().flex().flex_col().w_full().gap(px(4.0));
        for (idx, path) in self.cleanup_paths.iter().enumerate() {
            let row = div()
                .flex()
                .flex_row()
                .w_full()
                .min_w(px(0.0))
                .items_center()
                .gap(px(8.0))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(theme().bg_surface)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(theme().text_primary)
                        .child(path.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("cleanup-remove-{idx}")))
                        .cursor_pointer()
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme().bg_raised))
                        .child(icon(icons::X, 12.0, theme().text_faint))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.sessions.remove_path(idx, &this.app, cx);
                            }),
                        ),
                );
            list = list.child(row);
        }

        let input = input_frame(self.draft_input.clone());

        let pull_enabled = self.git_pull_before_new_session;
        let pull_toggle = div()
            .id("git-pull-toggle")
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.sessions.git_pull_before_new_session =
                        !this.sessions.git_pull_before_new_session;
                    this.sessions.push_git_pull(&this.app, cx);
                    cx.notify();
                }),
            )
            .child(toggle_switch("git-pull-knob", pull_enabled))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_primary)
                    .child("Run `git pull` on source before creating a new session"),
            );

        let promote_enabled = self.promote_attention_sessions;
        let promote_toggle = div()
            .id("promote-attention-toggle")
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.sessions.promote_attention_sessions =
                        !this.sessions.promote_attention_sessions;
                    this.sessions.push_promote(&this.app, cx);
                    cx.notify();
                }),
            )
            .child(toggle_switch("promote-knob", promote_enabled))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_primary)
                    .child("Move attention-needed sessions to top of list"),
            );

        let add_button = div()
            .id("cleanup-add")
            .cursor_pointer()
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(theme().accent)
            .text_size(px(12.0))
            .text_color(theme().text_on_accent)
            .hover(|s| s.bg(theme().lavender))
            .child("Add")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    let value = this.sessions.draft_input.read(cx).text().to_string();
                    this.sessions.commit_draft(value, &this.app, cx);
                    this.sessions
                        .draft_input
                        .update(cx, |i, cx| i.set_text_silent("", cx));
                }),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Sessions"))
            .child(card().child(pull_toggle).child(promote_toggle))
            .child(section_note(
                "Cleanup paths — deleted from each new session clone. \
                     Useful for stale runtime files that the parent working \
                     tree left behind (e.g. .overmind.sock, \
                     tmp/pids/server.pid).",
            ))
            .child(
                card().child(list.w_full()).child(
                    div()
                        .flex()
                        .flex_row()
                        .w_full()
                        .min_w(px(0.0))
                        .gap(px(8.0))
                        .items_center()
                        .child(input)
                        .child(add_button),
                ),
            )
    }
}
