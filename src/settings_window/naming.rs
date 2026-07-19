//! Branch-naming settings section — naming mode + per-agent model overrides.

use gpui::*;

use super::widgets::{card, input_frame, labeled_row, section_note, section_title};
use super::SettingsWindowState;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::theme;
use crate::AppState;

/// Owns the Claude/OpenCode naming-model inputs.
pub(super) struct NamingSection {
    claude_input: Entity<TextInput>,
    opencode_input: Entity<TextInput>,
}

impl NamingSection {
    pub(super) fn new(
        cx: &mut Context<SettingsWindowState>,
        claude_initial: String,
        opencode_initial: String,
    ) -> Self {
        let claude_input =
            cx.new(|cx| TextInput::new(cx, claude_initial, "claude-haiku-4-5-20251001"));
        cx.subscribe(&claude_input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                this.naming.push("claude", value, &this.app, cx);
            }
        })
        .detach();

        let opencode_input =
            cx.new(|cx| TextInput::new(cx, opencode_initial, "openai/gpt-4o-mini"));
        cx.subscribe(
            &opencode_input,
            |this, input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    let value = input.read(cx).text().to_string();
                    this.naming.push("opencode", value, &this.app, cx);
                }
            },
        )
        .detach();

        Self {
            claude_input,
            opencode_input,
        }
    }

    fn push(
        &self,
        which: &str,
        value: String,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        if let Some(app) = app.upgrade() {
            let mut new_config = app.read(cx).user_settings.naming.clone();
            let model = if value.is_empty() { None } else { Some(value) };
            match which {
                "claude" => new_config.claude.model = model,
                "opencode" => new_config.opencode.model = model,
                _ => return,
            }
            app.update(cx, |state: &mut crate::AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdateNamingConfig(new_config).into());
                cx.notify();
            });
        }
    }

    pub(super) fn render(
        &self,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) -> impl IntoElement {
        use crate::naming::NamingMode;

        let naming = app
            .upgrade()
            .map(|app| app.read(cx).user_settings.naming.clone())
            .unwrap_or_default();

        let mode = naming.mode;
        let mode_label = mode.label();
        let mode_desc = mode.description();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Branch Naming"))
            .child(section_note(
                "Uses the coding agent to generate meaningful branch names \
                     from your first prompt. Falls back to keyword extraction \
                     when the agent binary is unavailable.",
            ))
            .child(
                card()
                    // Mode toggle (clickable to cycle)
                    .child(
                        div()
                            .id("naming-mode-toggle")
                            .cursor_pointer()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, _window, cx| {
                                    let next_mode = match mode {
                                        NamingMode::Auto => NamingMode::Interactive,
                                        NamingMode::Interactive => NamingMode::Legacy,
                                        NamingMode::Legacy => NamingMode::Auto,
                                    };
                                    if let Some(app) = this.app.upgrade() {
                                        let mut new_config =
                                            app.read(cx).user_settings.naming.clone();
                                        new_config.mode = next_mode;
                                        app.update(cx, |state: &mut crate::AppState, cx| {
                                            state.pending_action = Some(
                                                crate::SettingsAction::UpdateNamingConfig(
                                                    new_config,
                                                )
                                                .into(),
                                            );
                                            cx.notify();
                                        });
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme().text_faint)
                                    .min_w(px(50.0))
                                    .child("Mode"),
                            )
                            .child(
                                div()
                                    .px(px(8.0))
                                    .py(px(2.0))
                                    .rounded(px(6.0))
                                    .bg(theme().bg_raised)
                                    .text_size(px(12.0))
                                    .text_color(theme().accent)
                                    .child(SharedString::from(mode_label.to_string())),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme().text_faint)
                                    .child(SharedString::from(mode_desc.to_string())),
                            ),
                    )
                    // Claude model
                    .child(labeled_row(
                        "Claude",
                        input_frame(self.claude_input.clone()),
                    ))
                    // OpenCode model
                    .child(labeled_row(
                        "OpenCode",
                        input_frame(self.opencode_input.clone()),
                    )),
            )
    }
}
