//! Infrastructure settings section — the global Traefik proxy + shared Docker
//! network toggle, plus status and the managed paths.

use gpui::*;

use super::widgets::{card, section_header, section_note, section_title, toggle_switch};
use super::SettingsWindowState;
use crate::theme::theme;
use crate::AppState;

/// Owns the mirrored base-infra toggle.
pub(super) struct InfraSection {
    enabled: bool,
}

impl InfraSection {
    pub(super) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn push(
        &self,
        enabled: bool,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action = Some(crate::SettingsAction::UpdateBaseInfra(enabled).into());
            cx.notify();
        })
        .ok();
    }

    pub(super) fn render(
        &self,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) -> impl IntoElement {
        let enabled = self.enabled;
        let status = app
            .upgrade()
            .and_then(|app| app.read(cx).base_infra_status.clone());
        let dynamic_path = crate::base_infra::dynamic_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let certs_path = crate::base_infra::certs_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let toggle = div()
            .id("base-infra-toggle")
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.infrastructure.enabled = !this.infrastructure.enabled;
                    let enabled = this.infrastructure.enabled;
                    this.infrastructure.push(enabled, &this.app, cx);
                    cx.notify();
                }),
            )
            .child(toggle_switch("base-infra-knob", enabled))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_primary)
                    .child("Enable global Traefik reverse proxy + shared network"),
            );

        let status_line = status.map(|s| {
            let color = if s.contains("Running") {
                theme().success
            } else if s.contains("Starting") || s.contains("Stopping") || s.contains("Stopped") {
                theme().warning
            } else {
                theme().danger // error
            };
            div()
                .text_size(px(11.0))
                .text_color(color)
                .child(SharedString::from(format!("Status: {s}")))
        });

        let path_row = |label: &str, value: &str| -> AnyElement {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme().text_faint)
                        .child(SharedString::from(label.to_string())),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme().text_secondary)
                        .child(SharedString::from(value.to_string())),
                )
                .into_any_element()
        };

        let mut pane = div()
            .id("infrastructure-pane")
            .flex_1()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .overflow_y_scroll()
            .child(section_title("Infrastructure"))
            .child(section_note(
                "A single Traefik reverse proxy + shared 'allele' Docker network that all \
                     sessions register HTTPS routes against. Requires Docker. Allele manages \
                     only the proxy and network — project services stay in your startup scripts.",
            ))
            .child(card().child(toggle));

        if let Some(line) = status_line {
            pane = pane.child(line);
        }

        pane = pane.child(section_header("Paths")).child(
            card()
                .child(path_row(
                    "Dynamic routes (session-start writes here)",
                    &dynamic_path,
                ))
                .child(path_row("TLS certificates (drop *.pem here)", &certs_path))
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme().text_faint)
                        .child(
                        "DNS: point your local domains at 127.0.0.1 via dnsmasq + /etc/resolver \
                         (one-time, needs sudo).",
                    ),
                ),
        );

        pane
    }
}
