//! Editor settings section — the external-editor command used by the file
//! tree's "Open in External Editor" action.

use gpui::*;

use super::widgets::{card, input_frame, section_note, section_title};
use super::SettingsWindowState;
use crate::text_input::{TextInput, TextInputEvent};
use crate::AppState;

/// Owns the external-editor input and pushes edits back to settings.
pub(super) struct EditorSection {
    input: Entity<TextInput>,
}

impl EditorSection {
    pub(super) fn new(cx: &mut Context<SettingsWindowState>, initial: String) -> Self {
        let input =
            cx.new(|cx| TextInput::new(cx, initial, crate::settings::DEFAULT_EXTERNAL_EDITOR));
        cx.subscribe(&input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                this.editor.push(value, &this.app, cx);
            }
        })
        .detach();
        Self { input }
    }

    fn push(
        &self,
        value: String,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action = Some(crate::SettingsAction::UpdateExternalEditor(value).into());
            cx.notify();
        })
        .ok();
    }

    pub(super) fn render(&self, _cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let input = input_frame(self.input.clone()).w_full();
        div()
            .id("editor-pane")
            // The section fills the window and scrolls inside it. Without
            // `min_h`, `min-height: auto` stops this flex item shrinking below
            // its content, so it lays out at content height — taller than the
            // window — and `overflow_y_scroll` above has nothing to scroll,
            // leaving the lower fields clipped and unreachable. See DEV-399.
            .h_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            // Horizontal stays clipped as before; vertical scrolls so a short
            // window can still reach the bottom of the section (DEV-399).
            .overflow_x_hidden()
            .overflow_y_scroll()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Editor"))
            .child(section_note(
                "External editor command — used by \"Open in External Editor\" \
                     in the file tree's right-click menu. Bare binary name (e.g. \
                     `subl`, `code`, `mate`) if on PATH, or an absolute path. \
                     Leave blank to use the default (Sublime Text's `subl`).",
            ))
            .child(card().child(input))
    }
}
