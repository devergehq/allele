//! Appearance settings section — terminal font size.

use gpui::*;

use super::widgets::{card, section_note, section_title};
use super::SettingsWindowState;
use crate::theme::theme;
use crate::AppState;

/// Owns the mirrored terminal font size.
pub(super) struct AppearanceSection {
    font_size: f32,
}

impl AppearanceSection {
    pub(super) fn new(font_size: f32) -> Self {
        Self {
            font_size: crate::terminal::clamp_font_size(font_size),
        }
    }

    fn push(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let value = self.font_size;
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action = Some(crate::SettingsAction::UpdateFontSize(value).into());
            cx.notify();
        })
        .ok();
    }

    fn adjust(
        &mut self,
        delta: f32,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let new_size = crate::terminal::clamp_font_size(self.font_size + delta);
        if (new_size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = new_size;
        self.push(app, cx);
        cx.notify();
    }

    fn reset(&mut self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let default = crate::terminal::DEFAULT_FONT_SIZE;
        if (default - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = default;
        self.push(app, cx);
        cx.notify();
    }

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let size = self.font_size;
        let min = crate::terminal::MIN_FONT_SIZE;
        let max = crate::terminal::MAX_FONT_SIZE;
        let default = crate::terminal::DEFAULT_FONT_SIZE;
        let at_min = size <= min + f32::EPSILON;
        let at_max = size >= max - f32::EPSILON;
        let at_default = (size - default).abs() < f32::EPSILON;

        let stepper_button = |id: &'static str, label: &'static str, enabled: bool| {
            let base = div()
                .id(SharedString::from(id))
                .w(px(28.0))
                .h(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .border_1()
                .border_color(theme().border_default)
                .bg(theme().bg_sunken)
                .text_size(px(14.0))
                .text_color(if enabled {
                    theme().text_primary
                } else {
                    theme().text_dim
                })
                .child(label);
            if enabled {
                base.cursor_pointer().hover(|s| s.bg(theme().bg_raised))
            } else {
                base.hover(|s| s)
            }
        };

        let minus = stepper_button("font-size-minus", "−", !at_min).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.appearance.adjust(-1.0, &this.app, cx);
            }),
        );

        let plus = stepper_button("font-size-plus", "+", !at_max).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.appearance.adjust(1.0, &this.app, cx);
            }),
        );

        let readout = div()
            .w(px(56.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .bg(theme().bg_surface)
            .text_size(px(12.0))
            .text_color(theme().text_primary)
            .child(format!("{:.0} pt", size));

        let reset = {
            let base = div()
                .id("font-size-reset")
                .px(px(10.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(theme().border_default)
                .bg(theme().bg_sunken)
                .text_size(px(11.0))
                .text_color(if at_default {
                    theme().text_dim
                } else {
                    theme().text_primary
                })
                .child("Reset");
            if at_default {
                base
            } else {
                base.cursor_pointer()
                    .hover(|s| s.bg(theme().bg_raised))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.appearance.reset(&this.app, cx);
                        }),
                    )
            }
        };

        let controls = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(minus)
            .child(readout)
            .child(plus)
            .child(reset);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Appearance"))
            .child(section_note(
                "Terminal font size — applies to every terminal (sessions \
                     and drawer tabs) live. Cmd+= / Cmd+- / Cmd+0 change this \
                     same value from inside a terminal.",
            ))
            .child(card().child(controls))
    }
}
