//! Shared, stateless building blocks for the Settings panes (macOS Settings
//! style). Pure functions over `gpui` + the theme — no window state — so every
//! section module composes its pane from the same vocabulary.

use gpui::*;

use crate::text_input::TextInput;
use crate::theme::{theme, with_alpha};

/// Frame around a [`TextInput`] entity — bordered, sunken, single-line.
pub(crate) fn input_frame(child: Entity<TextInput>) -> Div {
    div()
        .flex_1()
        .min_w(px(0.0))
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme().border_default)
        .bg(theme().bg_sunken)
        .text_size(px(12.0))
        .text_color(theme().text_primary)
        .overflow_hidden()
        .child(child)
}

/// Pane heading rendered above cards.
pub(crate) fn section_title(text: &'static str) -> Div {
    div()
        .text_size(px(crate::theme::TEXT_LG))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme().text_primary)
        .child(text)
}

/// Explanatory copy under a section title, outside the card.
pub(crate) fn section_note(text: &'static str) -> Div {
    div()
        .w_full()
        .text_size(px(crate::theme::TEXT_SM))
        .text_color(theme().text_secondary)
        .child(text)
}

/// Card container grouping a section's controls (macOS Settings style).
pub(crate) fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(14.0))
        .rounded(px(crate::theme::RADIUS_LG))
        .bg(with_alpha(theme().bg_raised, 0.4))
        .border_1()
        .border_color(with_alpha(theme().border_subtle, 0.6))
}

/// Labeled control row: fixed-width caption + control.
pub(crate) fn labeled_row(label: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(crate::theme::TEXT_XS))
                .text_color(theme().text_faint)
                .min_w(px(64.0))
                .child(label),
        )
        .child(control)
}

/// Animated toggle switch — 36x20 track, 16px knob, 120ms travel.
/// Pure display; the enclosing row owns the click handler.
pub(crate) fn toggle_switch(id: &'static str, enabled: bool) -> impl IntoElement {
    div()
        .w(px(36.0))
        .h(px(20.0))
        .flex_shrink_0()
        .rounded_full()
        .bg(if enabled {
            theme().accent
        } else {
            theme().bg_hover
        })
        .flex()
        .items_center()
        .px(px(2.0))
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .rounded_full()
                .bg(theme().bg_base)
                .with_animation(
                    SharedString::from(format!("{id}-{enabled}")),
                    Animation::new(std::time::Duration::from_millis(120))
                        .with_easing(gpui::ease_out_quint()),
                    move |knob, delta| {
                        let t = if enabled { delta } else { 1.0 - delta };
                        knob.ml(px(16.0 * t))
                    },
                ),
        )
}

/// Bold accent sub-heading used inside panes to group controls.
pub(crate) fn section_header(label: &str) -> Div {
    div()
        .text_size(px(11.0))
        .font_weight(FontWeight::BOLD)
        .text_color(theme().accent)
        .pb(px(2.0))
        .child(SharedString::from(label.to_string()))
}
