//! The pairing half of *Settings → Machines* (DEV-1160): the *Allow remote
//! connections* switch, *Pair a new machine* (code, QR, approval prompt),
//! *Add machine*, and the list of paired machines.
//!
//! Everything here reads one [`RemoteStatus`] snapshot and emits
//! `SettingsAction`s; no click talks to the network directly.

use std::time::Duration;

use gpui::*;

use super::widgets::{card, input_frame, section_header, section_note, toggle_switch};
use super::SettingsWindowState;
use crate::remote::pairing::{HostView, Outcome, PairingRequest};
use crate::remote::{ClientView, PeerId, PeerRow, RemoteStatus};
use crate::text_input::TextInput;
use crate::theme::theme;
use crate::SettingsAction;

/// Size of one QR module on screen.
const QR_MODULE: f32 = 4.0;
/// Blank modules around the QR, which scanners need.
const QR_QUIET: usize = 4;

/// Render the pairing controls from `status`.
pub(super) fn render(
    status: &RemoteStatus,
    allow: bool,
    code_input: &Entity<TextInput>,
    confirm_remove: Option<PeerId>,
    cx: &mut Context<SettingsWindowState>,
) -> Div {
    let mut pane = div().flex().flex_col().gap(px(12.0));
    pane = pane
        .child(section_header("Remote connections"))
        .child(switch_card(status, allow, cx));
    if let Some(e) = &status.error {
        pane = pane.child(danger_line(e.clone()));
    }
    pane.child(section_header("Pair a new machine"))
        .child(section_note(
            "Show a one-time code here, then enter or scan it on your other Mac. Nothing \
             is trusted until you approve it on this Mac. A code works once and expires \
             after five minutes.",
        ))
        .child(host_card(&status.host, cx))
        .child(section_header("Add machine"))
        .child(section_note(
            "On a Mac that is showing a pairing code, enter that code here.",
        ))
        .child(client_card(&status.client, code_input, cx))
        .child(section_header("Paired machines"))
        .child(peers_card(status, confirm_remove, cx))
}

fn switch_card(status: &RemoteStatus, allow: bool, cx: &mut Context<SettingsWindowState>) -> Div {
    let state = match (allow, status.listening) {
        (true, true) => ("Paired machines can connect.", theme().success),
        (true, false) => ("Starting…", theme().text_faint),
        (false, _) if status.peers.is_empty() => (
            "Turns on by itself when you approve your first machine.",
            theme().text_faint,
        ),
        (false, _) => ("Paired machines cannot connect.", theme().warning),
    };
    card()
        .child(
            div()
                .id("allow-remote-toggle")
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _e, _w, cx| {
                        this.push_settings(SettingsAction::SetAllowRemoteConnections(!allow), cx);
                    }),
                )
                .child(toggle_switch("allow-remote-knob", allow))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme().text_primary)
                        .child("Allow remote connections"),
                ),
        )
        .child(div().text_size(px(11.0)).text_color(state.1).child(state.0))
}

fn host_card(host: &HostView, cx: &mut Context<SettingsWindowState>) -> Div {
    match host {
        HostView::Idle => card().child(
            button(
                "pair-start",
                "Pair a new machine",
                theme().accent,
                theme().bg_base,
            )
            .on_click(cx.listener(|this, _e, _w, cx| {
                let machine_name = this.machines.machine_name(cx);
                this.push_settings(SettingsAction::StartPairing { machine_name }, cx);
            })),
        ),
        HostView::Showing {
            code,
            expires_in,
            failed_attempts,
        } => {
            let copy = code.clone();
            let mut c = card()
                .child(qr(code))
                .child(
                    div()
                        .font_family("Menlo")
                        .text_size(px(12.0))
                        .text_color(theme().text_primary)
                        .child(code.clone()),
                )
                .child(faint(format!("Expires in {}", mm_ss(*expires_in))));
            if *failed_attempts > 0 {
                c = c.child(danger_line(format!(
                    "{failed_attempts} wrong code(s) tried against this one. If that was not \
                     you, cancel it."
                )));
            }
            c.child(
                row()
                    .child(
                        button(
                            "pair-copy",
                            "Copy code",
                            theme().bg_hover,
                            theme().text_primary,
                        )
                        .on_click(cx.listener(move |_this, _e, _w, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        })),
                    )
                    .child(
                        button(
                            "pair-cancel",
                            "Cancel",
                            theme().bg_hover,
                            theme().text_primary,
                        )
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.push_settings(SettingsAction::CancelPairing, cx);
                        })),
                    ),
            )
        }
        HostView::AwaitingApproval {
            request,
            expires_in,
        } => approval_card(request, *expires_in, cx),
        HostView::Finishing {
            machine_name,
            confirmation,
        } => card().child(faint(format!(
            "Waiting for \u{201c}{machine_name}\u{201d} to confirm the number {confirmation}…"
        ))),
        HostView::Ended(outcome) => {
            let (text, ok) = outcome_text(outcome);
            card()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(if ok { theme().success } else { theme().warning })
                        .child(text),
                )
                .child(
                    button("pair-dismiss", "OK", theme().bg_hover, theme().text_primary).on_click(
                        cx.listener(|this, _e, _w, cx| {
                            this.push_settings(SettingsAction::DismissPairing, cx);
                        }),
                    ),
                )
        }
    }
}

/// The approval prompt. The machine name is the peer's *claim*; the
/// confirmation number is the check, so it is the biggest thing on the card.
fn approval_card(
    request: &PairingRequest,
    expires_in: Duration,
    cx: &mut Context<SettingsWindowState>,
) -> Div {
    let id = request.id;
    card()
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme().text_primary)
                .child(format!(
                    "\u{201c}{}\u{201d} wants to pair with this Mac.",
                    request.machine_name
                )),
        )
        .child(faint(format!(
            "Its ID begins {}",
            fingerprint(request.client)
        )))
        .child(big_number(&request.confirmation))
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme().text_primary)
                .child(
                    "Approve only if your other Mac is showing this same number. If it \
                     shows a different number, or nothing, decline.",
                ),
        )
        .child(faint(format!(
            "It will be able to view sessions, not control them. Declines itself in {}.",
            mm_ss(expires_in)
        )))
        .child(
            row()
                .child(
                    button("pair-approve", "Approve", theme().accent, theme().bg_base).on_click(
                        cx.listener(move |this, _e, _w, cx| {
                            this.push_settings(
                                SettingsAction::AnswerPairing {
                                    request: id,
                                    approve: true,
                                },
                                cx,
                            );
                        }),
                    ),
                )
                .child(
                    button(
                        "pair-decline",
                        "Decline",
                        theme().bg_hover,
                        theme().text_primary,
                    )
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.push_settings(
                            SettingsAction::AnswerPairing {
                                request: id,
                                approve: false,
                            },
                            cx,
                        );
                    })),
                ),
        )
}

fn client_card(
    client: &ClientView,
    code_input: &Entity<TextInput>,
    cx: &mut Context<SettingsWindowState>,
) -> Div {
    let entry = || {
        let input = code_input.clone();
        row()
            .child(div().flex_1().child(input_frame(code_input.clone())))
            .child(
                button("add-machine", "Add", theme().accent, theme().bg_base).on_click(
                    cx.listener(move |this, _e, _w, cx| {
                        let code = input.read(cx).text().to_string();
                        let machine_name = this.machines.machine_name(cx);
                        this.push_settings(SettingsAction::AddMachine { code, machine_name }, cx);
                    }),
                ),
            )
    };
    let dismiss = |cx: &mut Context<SettingsWindowState>| {
        button("add-dismiss", "OK", theme().bg_hover, theme().text_primary).on_click(cx.listener(
            |this, _e, _w, cx| {
                this.push_settings(SettingsAction::DismissAddMachine, cx);
            },
        ))
    };
    match client {
        ClientView::Idle => card().child(entry()),
        ClientView::Connecting => card().child(faint("Connecting to the other Mac…".into())),
        ClientView::AwaitingConfirmation { confirmation } => card()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_primary)
                    .child("Does the other Mac show this number? Confirm only if it matches."),
            )
            .child(big_number(confirmation))
            .child(
                row()
                    .child(
                        button("add-confirm", "Confirm", theme().accent, theme().bg_base).on_click(
                            cx.listener(|this, _e, _w, cx| {
                                this.push_settings(
                                    SettingsAction::ConfirmAddMachine { confirmed: true },
                                    cx,
                                );
                            }),
                        ),
                    )
                    .child(
                        button(
                            "add-cancel",
                            "Cancel",
                            theme().bg_hover,
                            theme().text_primary,
                        )
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.push_settings(
                                SettingsAction::ConfirmAddMachine { confirmed: false },
                                cx,
                            );
                        })),
                    ),
            ),
        ClientView::Confirmed { confirmation } => card()
            .child(faint(
                "Confirmed. Now approve this Mac on the other Mac.".into(),
            ))
            .child(big_number(confirmation)),
        ClientView::Paired { machine_name } => card()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().success)
                    .child(format!("Paired with \u{201c}{machine_name}\u{201d}.")),
            )
            .child(dismiss(cx)),
        ClientView::Failed(e) => card().child(danger_line(e.clone())).child(entry()),
    }
}

fn peers_card(
    status: &RemoteStatus,
    confirm_remove: Option<PeerId>,
    cx: &mut Context<SettingsWindowState>,
) -> Div {
    let mut c = card();
    if let Some(e) = &status.peers_error {
        c = c.child(danger_line(format!(
            "peers.json could not be read, so no machine is trusted until it is fixed: {e}"
        )));
    }
    if status.peers.is_empty() {
        return c.child(faint("No machines paired yet.".into()));
    }
    for (i, row_) in status.peers.iter().enumerate() {
        c = c.child(peer_row(i, row_, confirm_remove == Some(row_.peer.id), cx));
    }
    c
}

fn peer_row(
    i: usize,
    peer_row: &PeerRow,
    confirming: bool,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let peer = &peer_row.peer;
    let id = peer.id;
    let control = peer.permissions.control;
    let dot = if peer_row.connected {
        theme().success
    } else {
        theme().text_faint
    };
    let remove_label = if confirming {
        "Confirm remove"
    } else {
        "Remove"
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .py(px(4.0))
        .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(dot))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme().text_primary)
                        .child(peer.machine_name.clone()),
                )
                .child(faint(format!(
                    "{} · paired {} · {}",
                    fingerprint(id),
                    date(peer.paired_at),
                    if control {
                        "view + control"
                    } else {
                        "view only"
                    }
                ))),
        )
        .child(
            div()
                .id(("peer-control", i))
                .cursor_pointer()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _e, _w, cx| {
                        this.push_settings(
                            SettingsAction::SetPeerControl {
                                peer: id,
                                allowed: !control,
                            },
                            cx,
                        );
                    }),
                )
                .child(toggle_switch(format!("peer-control-knob-{i}"), control))
                .child(faint("Control".into())),
        )
        .child(
            button(
                ("peer-remove", i),
                remove_label,
                if confirming {
                    theme().danger
                } else {
                    theme().bg_hover
                },
                if confirming {
                    theme().bg_base
                } else {
                    theme().text_primary
                },
            )
            .on_click(cx.listener(move |this, _e, _w, cx| {
                if this.machines.confirm_remove == Some(id) {
                    this.machines.confirm_remove = None;
                    this.push_settings(SettingsAction::RemovePeer(id), cx);
                } else {
                    this.machines.confirm_remove = Some(id);
                    cx.notify();
                }
            })),
        )
}

/// The code as a QR, drawn from divs: dark modules on a light field with a
/// quiet zone. Light in both themes, because scanners expect dark-on-light.
fn qr(text: &str) -> Div {
    let Ok(code) = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M) else {
        return div();
    };
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QR_QUIET;
    let mut grid = div()
        .flex()
        .flex_col()
        .bg(gpui::white())
        .p(px(QR_MODULE * QR_QUIET as f32))
        .w(px(QR_MODULE * size as f32));
    for y in 0..width {
        let mut line = div().flex().flex_row();
        for x in 0..width {
            let dark = colors[y * width + x] == qrcode::Color::Dark;
            let cell = div().w(px(QR_MODULE)).h(px(QR_MODULE));
            line = line.child(if dark { cell.bg(gpui::black()) } else { cell });
        }
        grid = grid.child(line);
    }
    grid
}

fn outcome_text(outcome: &Outcome) -> (String, bool) {
    match outcome {
        Outcome::Paired { machine_name } => (
            format!("Paired with \u{201c}{machine_name}\u{201d}. It can view sessions; turn on Control below to let it type."),
            true,
        ),
        Outcome::Declined => ("Pairing declined. Nothing was saved.".into(), false),
        Outcome::TimedOut => ("Nobody approved in time. Nothing was saved.".into(), false),
        Outcome::Expired => ("The code expired unused.".into(), false),
        Outcome::Cancelled => ("Pairing cancelled.".into(), false),
        Outcome::Contested => (
            "Two machines used the same code, so the pairing was stopped. Someone else may \
             have seen the code — make a new one."
                .into(),
            false,
        ),
        Outcome::Failed(e) => (format!("Pairing did not complete: {e}"), false),
    }
}

/// The first 16 hex digits of a peer id in groups of four — 64 bits, too many
/// to grind a look-alike key for, few enough to read aloud.
pub(super) fn fingerprint(id: PeerId) -> String {
    let hex = id.to_string();
    hex.as_bytes()[..16]
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

fn mm_ss(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// `YYYY-MM-DD` (UTC) for seconds since the epoch.
pub(super) fn date(secs: u64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn row() -> Div {
    div().flex().flex_row().items_center().gap(px(8.0))
}

/// A confirmation number, as both Macs show it.
fn big_number(number: &str) -> Div {
    div()
        .font_family("Menlo")
        .text_size(px(28.0))
        .text_color(theme().accent)
        .child(number.to_string())
}

fn faint(text: String) -> Div {
    div()
        .text_size(px(11.0))
        .text_color(theme().text_faint)
        .child(text)
}

fn danger_line(text: String) -> Div {
    div()
        .text_size(px(11.0))
        .text_color(theme().danger)
        .child(text)
}

fn button(id: impl Into<ElementId>, label: &str, bg: Hsla, fg: Hsla) -> Stateful<Div> {
    div()
        .id(id)
        .cursor_pointer()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .bg(bg)
        .text_size(px(12.0))
        .text_color(fg)
        .child(label.to_string())
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: that glob brings in `gpui::*`, whose `test`
    // attribute would shadow the standard one.
    use super::{date, fingerprint, mm_ss};
    use crate::remote::PeerId;
    use std::time::Duration;

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_793_577_600), "2026-11-02");
    }

    #[test]
    fn fingerprints_are_sixteen_hex_digits_in_fours() {
        let id: PeerId = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6"
            .parse()
            .unwrap();
        assert_eq!(fingerprint(id), "ae58 ff88 3324 1ac8");
    }

    #[test]
    fn countdowns_are_minutes_and_seconds() {
        assert_eq!(mm_ss(Duration::from_secs(299)), "4:59");
        assert_eq!(mm_ss(Duration::from_secs(5)), "0:05");
    }

    #[test]
    fn a_pairing_code_fits_in_a_qr() {
        let code = crate::remote::pairing::PairingCode {
            host: crate::remote::LocalIdentity::ephemeral().id(),
            secret: crate::remote::pairing::PairingSecret::generate().unwrap(),
        };
        let qr = qrcode::QrCode::with_error_correction_level(code.to_text(), qrcode::EcLevel::M)
            .unwrap();
        // Alphanumeric mode keeps it small enough to scan off a screen.
        assert!(qr.width() <= 41, "QR is {} modules wide", qr.width());
    }
}
