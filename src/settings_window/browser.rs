//! Browser settings section — the Chrome tab-integration toggle.

use gpui::*;

use super::widgets::{card, section_note, section_title, toggle_switch};
use super::SettingsWindowState;
use crate::theme::theme;
use crate::AppState;

/// Owns the mirrored browser-integration toggle.
pub(super) struct BrowserSection {
    enabled: bool,
}

impl BrowserSection {
    pub(super) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn push(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let value = self.enabled;
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action =
                Some(crate::SettingsAction::UpdateBrowserIntegration(value).into());
            cx.notify();
        })
        .ok();
    }

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let enabled = self.enabled;
        let toggle = div()
            .id("browser-toggle")
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.browser.enabled = !this.browser.enabled;
                    this.browser.push(&this.app, cx);
                    cx.notify();
                }),
            )
            .child(toggle_switch("browser-knob", enabled))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_primary)
                    .child(if enabled { "Enabled" } else { "Disabled" }),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Browser"))
            .child(section_note(
                "Link each Allele session to a tab in your running \
                     Google Chrome. Switching sessions activates the \
                     matching tab; new sessions open a tab at the project's \
                     allele.json preview URL. Uses AppleScript against your \
                     real Chrome (first use prompts for Automation \
                     permission). When disabled, preview URLs fall back to \
                     your system default browser.",
            ))
            .child(card().child(toggle))
    }
}
