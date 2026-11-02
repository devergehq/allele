//! Machines settings section (DEV-1159) — this machine's name and id on the
//! network, and the relay server its endpoint uses.
//!
//! Pairing other machines (DEV-1160) will add to this pane. For now it shows
//! who this machine is. Opening it is the identity's "first use": the secret
//! key is generated and stored in the Keychain the first time it is needed.

use std::sync::Arc;

use gpui::*;

use super::widgets::{card, input_frame, labeled_row, section_note, section_title};
use super::SettingsWindowState;
use crate::remote::{resolve_machine_name, LocalIdentity, RelayChoice};
use crate::settings::RemoteSettings;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::theme;
use crate::AppState;

/// Machine id lookup, which may hit the Keychain (and its permission prompt).
enum IdStatus {
    Loading,
    Ready(String),
    Failed(String),
}

pub(super) struct MachinesSection {
    name_input: Entity<TextInput>,
    relay_input: Entity<TextInput>,
    /// Why the relay text was not saved, if it was not.
    relay_error: Option<String>,
    id: IdStatus,
    /// What the name resolves to when the field is blank.
    computer_name: Option<String>,
}

impl MachinesSection {
    pub(super) fn new(cx: &mut Context<SettingsWindowState>, settings: &RemoteSettings) -> Self {
        let name_input = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.machine_name.clone().unwrap_or_default(),
                "Computer name",
            )
        });
        cx.subscribe(&name_input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                push(
                    crate::SettingsAction::UpdateMachineName(value),
                    &this.app,
                    cx,
                );
            }
        })
        .detach();

        let relay_input = cx.new(|cx| {
            TextInput::new(
                cx,
                settings.relay_url.clone().unwrap_or_default(),
                "n0 public relays (default)",
            )
        });
        cx.subscribe(&relay_input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                // An invalid URL is shown, never saved: saving it would leave
                // the next bind failing rather than falling back silently.
                match RelayChoice::from_setting(Some(&value)) {
                    Ok(_) => {
                        this.machines.relay_error = None;
                        push(crate::SettingsAction::UpdateRelayUrl(value), &this.app, cx);
                    }
                    Err(e) => this.machines.relay_error = Some(e.to_string()),
                }
                cx.notify();
            }
        })
        .detach();

        Self {
            name_input,
            relay_input,
            relay_error: None,
            id: IdStatus::Loading,
            computer_name: None,
        }
    }

    /// Resolve the machine id and the computer name off the UI thread. Called
    /// once, when the settings window opens.
    pub(super) fn load(
        &mut self,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let Some(platform) = app.upgrade().map(|a| a.read(cx).platform.clone_arcs()) else {
            return;
        };
        let secrets = Arc::clone(&platform.secrets);
        let shell = Arc::clone(&platform.shell);
        cx.spawn(async move |this, cx| {
            let (id, computer_name) = cx
                .background_executor()
                .spawn(async move {
                    let id =
                        LocalIdentity::for_this_instance(&*secrets).map(|i| i.id().to_string());
                    (id, shell.computer_name())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.machines.id = match id {
                    Ok(id) => IdStatus::Ready(id),
                    Err(e) => {
                        tracing::warn!("machine identity unavailable: {e}");
                        IdStatus::Failed(e.to_string())
                    }
                };
                this.machines.computer_name = computer_name;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let typed = self.name_input.read(cx).text().to_string();
        let shown_as = resolve_machine_name(Some(&typed), self.computer_name.clone());
        let (id_text, id_color) = match &self.id {
            IdStatus::Loading => ("Loading…".to_string(), theme().text_faint),
            IdStatus::Ready(id) => (id.clone(), theme().text_secondary),
            IdStatus::Failed(e) => (e.clone(), theme().danger),
        };

        let mut relay_card = card().child(labeled_row(
            "Relay server",
            input_frame(self.relay_input.clone()),
        ));
        if let Some(err) = &self.relay_error {
            relay_card = relay_card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme().danger)
                    .child(err.clone()),
            );
        }

        div()
            .id("machines-pane")
            // Same scroll contract as every other section (DEV-399).
            .h_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_x_hidden()
            .overflow_y_scroll()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Machines"))
            .child(section_note(
                "How this Mac appears to your other Macs. Leave the name blank to \
                 use the computer name from System Settings.",
            ))
            .child(
                card()
                    .child(labeled_row("Name", input_frame(self.name_input.clone())))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme().text_faint)
                            .child(format!(
                                "Shown to other machines as \u{201c}{shown_as}\u{201d}"
                            )),
                    ),
            )
            .child(
                card().child(labeled_row(
                    "Machine ID",
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(11.0))
                        .font_family("Menlo")
                        .text_color(id_color)
                        .overflow_hidden()
                        .child(id_text),
                )),
            )
            .child(section_note(
                "The machine ID is this Mac's public key. Its secret half is kept in \
                 the macOS Keychain, never in ~/.allele. A sandboxed or redirected \
                 Allele has an ID of its own.",
            ))
            .child(relay_card)
            .child(section_note(
                "Used when two Macs cannot reach each other directly. Leave blank for \
                 n0's public relays, or enter the URL of your own iroh-relay. Traffic \
                 through a relay stays end-to-end encrypted. Takes effect the next \
                 time the connection starts.",
            ))
    }
}

fn push(
    action: crate::SettingsAction,
    app: &WeakEntity<AppState>,
    cx: &mut Context<SettingsWindowState>,
) {
    app.update(cx, |state: &mut AppState, cx| {
        state.pending_action = Some(action.into());
        cx.notify();
    })
    .ok();
}
