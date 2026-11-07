//! Remote attention (DEV-1164): sessions on a paired Mac that are waiting for
//! the user show up in this Mac's attention bar, and a session starting to
//! wait raises one local notification.
//!
//! Two inputs, both from [`crate::remote::follow::HostFeeds`]:
//!
//! - **Rows** are read from the feeds' current state at render time, so a row
//!   leaves the bar the moment its session stops waiting. A redraw is driven
//!   by the feeds' change tick, not the render tick (that one stops while the
//!   display sleeps, and a remote host does not).
//! - **Notifications** come only from live transitions
//!   (`subscribe_transitions`). A reconnect snapshot is never broadcast there,
//!   and `SnapshotOutcome::missed` is deliberately not read: a session that
//!   started waiting while we were away is shown, not announced.
//!
//! Everything a remote host sends — machine name, session label, project
//! name — is chosen by the other machine. It is stripped of control and
//! format characters before display, and the notification text still goes
//! through `hooks::applescript_escape` on its way to osascript.

use gpui::Context;
use tokio::sync::broadcast::error::RecvError;

use crate::app_state::AppState;
use crate::hooks;
use crate::remote::follow::HostTransition;
use crate::remote::peers::is_format_char;
use crate::remote::PeerId;
use crate::session::AttentionKind;
use crate::settings;
use crate::status_feed::client::Reachability;
use crate::status_feed::model::FeedAttention;
use crate::status_feed::{RemoteHostState, SessionEntry};

/// Separator between the host's name and the session label, as in
/// `Desk · fix-dispatch`.
const HOST_SEP: &str = " · ";

/// Longest remote string shown, in characters. A label is a sidebar name; a
/// host that sends a page of text gets it cut, not a wall of notification.
const MAX_DISPLAY_CHARS: usize = 80;

/// One remote session waiting for the user, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteRow {
    /// Unique across hosts: the peer and the host's session id.
    pub key: String,
    /// The host and its session id: what a click opens (DEV-1167).
    pub peer: PeerId,
    pub session_id: String,
    /// `Host · label`.
    pub label: String,
    pub kind: AttentionKind,
    /// The secondary text: the project, and whether the host is reachable.
    pub detail: String,
}

/// What a remote transition should do here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Effects {
    /// Play the awaiting-input sound.
    pub sound: bool,
    /// Show this notification (title, body).
    pub notice: Option<(String, String)>,
}

/// The local settings that gate a remote notification — the same two that
/// gate a local session entering `AwaitingInput` (`hook_events.rs`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Gates {
    pub notifications_enabled: bool,
    pub sound_on_awaiting_input: bool,
}

/// `raw` made safe to draw: control and format characters (newlines, bidi
/// overrides, zero-width spaces) removed, whitespace runs collapsed, length
/// capped. Not an escape — osascript escaping happens separately.
pub(crate) fn display_text(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    let mut count = 0;
    for c in raw.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_format_char(c) {
            continue;
        }
        // The cap counts the space this character brings with it.
        if count + usize::from(pending_space) >= MAX_DISPLAY_CHARS {
            out.push('…');
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
            count += 1;
        }
        out.push(c);
        count += 1;
    }
    out
}

fn kind_of(attention: Option<FeedAttention>) -> AttentionKind {
    match attention {
        Some(FeedAttention::Question) => AttentionKind::Question,
        Some(FeedAttention::Idle) => AttentionKind::Idle,
        // `None` reads as blocking, as it does for local sessions: under-warning
        // about a stuck agent is the worse error.
        Some(FeedAttention::Permission) | None => AttentionKind::Permission,
    }
}

fn labelled(host: &str, entry: &SessionEntry) -> String {
    format!(
        "{}{HOST_SEP}{}",
        display_text(host),
        display_text(&entry.label)
    )
}

/// Rows for every remote session awaiting input. Hosts are ordered by name
/// so rows don't jump around between renders; sessions keep host order.
/// `host_name` resolves a peer to the name shown; a peer it returns `None`
/// for (no longer trusted) is left out.
pub(crate) fn rows(
    mut hosts: Vec<(PeerId, RemoteHostState)>,
    host_name: impl Fn(PeerId) -> Option<String>,
) -> Vec<RemoteRow> {
    let mut named: Vec<(String, PeerId, RemoteHostState)> = hosts
        .drain(..)
        .filter_map(|(peer, state)| Some((host_name(peer)?, peer, state)))
        .collect();
    named.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut out = Vec::new();
    for (name, peer, state) in &named {
        let reachable = !matches!(state.reachability(), Reachability::Unreachable { .. });
        for entry in state.awaiting_input() {
            let project = display_text(&entry.project_name);
            // An unreachable host's sessions are its last-known state. Say so
            // rather than present a stale "blocked" as current.
            let detail = if reachable {
                project
            } else {
                format!("{project} · last known, {} unreachable", display_text(name))
            };
            out.push(RemoteRow {
                key: format!("{peer}-{}", entry.id),
                peer: *peer,
                session_id: entry.id.clone(),
                label: labelled(name, entry),
                kind: kind_of(entry.attention),
                detail,
            });
        }
    }
    out
}

/// What to do about one live transition. Only a session *entering*
/// `AwaitingInput` does anything, and then only what the local settings allow.
/// `entry` is the session as the feed now holds it (for its label and
/// project); a transition for a session we can no longer see does nothing.
pub(crate) fn decide(
    t: &HostTransition,
    host: &str,
    entry: Option<&SessionEntry>,
    gates: Gates,
) -> Effects {
    if !t.transition.enters_awaiting_input() {
        return Effects::default();
    }
    let Some(entry) = entry else {
        return Effects::default();
    };
    let notice = gates.notifications_enabled.then(|| {
        (
            format!(
                "{}{HOST_SEP}{} — needs input",
                display_text(host),
                display_text(&entry.project_name)
            ),
            format!(
                "{} is blocked and waiting for you",
                display_text(&entry.label)
            ),
        )
    });
    Effects {
        sound: gates.sound_on_awaiting_input,
        notice,
    }
}

/// The name shown for a host, or `None` if it is no longer trusted. A peer
/// that was just unpaired can still have a transition queued, or a feed not
/// yet forgotten; neither may reach the screen.
pub(crate) fn trusted_host_name(
    remote: &crate::remote::RemoteService,
    peer: PeerId,
) -> Option<String> {
    let name = display_text(&remote.peer_name(peer)?);
    Some(if name.is_empty() { peer.short() } else { name })
}

impl AppState {
    /// Every remote session awaiting input, for the attention bar.
    pub(crate) fn remote_attention_rows(&self) -> Vec<RemoteRow> {
        rows(self.remote.hosts().all(), |peer| {
            trusted_host_name(&self.remote, peer)
        })
    }

    /// Act on one live remote transition: chime and notify, as a local
    /// session entering `AwaitingInput` would.
    fn on_remote_transition(&self, t: &HostTransition) {
        let Some(host) = trusted_host_name(&self.remote, t.peer) else {
            return; // unpaired since this was queued
        };
        let state = self.remote.hosts().get(t.peer);
        let entry = state.as_ref().and_then(|s| s.session(&t.transition.id));
        let gates = Gates {
            notifications_enabled: self.user_settings.notifications_enabled,
            sound_on_awaiting_input: self.user_settings.sound_on_awaiting_input,
        };
        let effects = decide(t, &host, entry, gates);
        if effects.sound {
            let sound_path = self
                .user_settings
                .awaiting_input_sound_path
                .clone()
                .unwrap_or_else(|| settings::DEFAULT_AWAITING_INPUT_SOUND.to_string());
            hooks::play_sound(&sound_path);
        }
        if let Some((title, body)) = effects.notice {
            hooks::show_notification(&title, &body);
        }
    }
}

/// Start the two remote-attention tasks. Called once, at launch.
pub(crate) fn spawn(cx: &mut Context<AppState>) {
    // This runs inside `AppState`'s constructor, so the entity cannot be read
    // here — that is a double-lease panic, and at launch it aborts the app.
    // Each task reads the feeds once it first runs, after construction.

    // Notifications: live transitions only.
    cx.spawn(async move |this, cx| {
        let Ok(mut transitions) =
            this.read_with(cx, |state, _| state.remote.hosts().subscribe_transitions())
        else {
            return;
        };
        loop {
            let t = match transitions.recv().await {
                Ok(t) => t,
                Err(RecvError::Lagged(n)) => {
                    // The state is intact in HostFeeds; only these
                    // announcements are lost.
                    tracing::warn!("remote attention: skipped {n} transition(s)");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            if this
                .update(cx, |this: &mut AppState, _cx| this.on_remote_transition(&t))
                .is_err()
            {
                break; // AppState dropped — app is exiting
            }
        }
    })
    .detach();

    // Redraw: the attention bar reads the feeds at render time, so any change
    // to any host only needs a frame.
    cx.spawn(async move |this, cx| {
        let Ok(mut changes) =
            this.read_with(cx, |state, _| state.remote.hosts().subscribe_changes())
        else {
            return;
        };
        while changes.changed().await.is_ok() {
            if this.update(cx, |_: &mut AppState, cx| cx.notify()).is_err() {
                break;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_feed::model::{FeedStatus, StatusSnapshot};
    use crate::status_feed::Transition;
    use std::time::SystemTime;

    const HOSTILE: &str = "x\\\" & do shell script \"touch /tmp/pwned\" & \"\\";

    fn peer() -> PeerId {
        crate::remote::LocalIdentity::ephemeral().id()
    }

    fn entry(id: &str, label: &str, status: FeedStatus) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            label: label.into(),
            project_id: "p1".into(),
            project_name: "allele".into(),
            group: None,
            status,
            attention: (status == FeedStatus::AwaitingInput).then_some(FeedAttention::Permission),
            last_activity_ms: 1,
        }
    }

    fn host(sessions: Vec<SessionEntry>) -> RemoteHostState {
        let mut h = RemoteHostState::new();
        h.apply_snapshot(StatusSnapshot { seq: 0, sessions }, SystemTime::now());
        h
    }

    fn into_waiting(p: PeerId, id: &str, from: Option<FeedStatus>) -> HostTransition {
        HostTransition {
            peer: p,
            transition: Transition {
                id: id.into(),
                from,
                to: FeedStatus::AwaitingInput,
            },
        }
    }

    const ALL_ON: Gates = Gates {
        notifications_enabled: true,
        sound_on_awaiting_input: true,
    };

    #[test]
    fn a_waiting_remote_session_is_a_row_labelled_with_its_host() {
        let rows = rows(
            vec![(
                peer(),
                host(vec![
                    entry("a", "fix-dispatch", FeedStatus::AwaitingInput),
                    entry("b", "busy", FeedStatus::Running),
                ]),
            )],
            |_| Some("Desktop".into()),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "Desktop · fix-dispatch");
        assert_eq!(rows[0].kind, AttentionKind::Permission);
        assert_eq!(rows[0].detail, "allele");
    }

    #[test]
    fn a_session_that_stops_waiting_leaves_the_rows() {
        let mut h = host(vec![entry("a", "one", FeedStatus::AwaitingInput)]);
        assert_eq!(
            rows(vec![(peer(), h.clone())], |_| Some("D".into())).len(),
            1
        );
        h.apply_delta(
            crate::status_feed::StatusDelta {
                seq: 1,
                change: crate::status_feed::StatusChange::StatusChanged {
                    id: "a".into(),
                    status: FeedStatus::Running,
                    attention: None,
                    last_activity_ms: 2,
                },
            },
            SystemTime::now(),
        );
        assert!(rows(vec![(peer(), h)], |_| Some("D".into())).is_empty());
    }

    #[test]
    fn rows_from_an_unreachable_host_say_they_are_last_known() {
        let mut h = host(vec![entry("a", "one", FeedStatus::AwaitingInput)]);
        h.connection_lost(SystemTime::now());
        let rows = rows(vec![(peer(), h)], |_| Some("Desk".into()));
        assert_eq!(rows[0].detail, "allele · last known, Desk unreachable");
    }

    #[test]
    fn rows_are_ordered_by_host_name_and_keyed_per_host() {
        let (zed, alpha) = (peer(), peer());
        let rows = rows(
            vec![
                (
                    alpha,
                    host(vec![entry("a", "z", FeedStatus::AwaitingInput)]),
                ),
                (zed, host(vec![entry("a", "y", FeedStatus::AwaitingInput)])),
            ],
            |p| {
                if p == zed {
                    Some("Zed".into())
                } else {
                    Some("Alpha".into())
                }
            },
        );
        assert_eq!(rows[0].label, "Alpha · z");
        assert_eq!(rows[1].label, "Zed · y");
        assert_ne!(rows[0].key, rows[1].key, "same session id, different hosts");
    }

    #[test]
    fn an_untrusted_hosts_sessions_are_not_rows() {
        let rows = rows(
            vec![(
                peer(),
                host(vec![entry("a", "x", FeedStatus::AwaitingInput)]),
            )],
            |_| None,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn entering_awaiting_input_notifies_once_with_the_host_name() {
        let e = entry("a", "fix-dispatch", FeedStatus::AwaitingInput);
        let fx = decide(
            &into_waiting(peer(), "a", Some(FeedStatus::Running)),
            "Desktop",
            Some(&e),
            ALL_ON,
        );
        assert!(fx.sound);
        let (title, body) = fx.notice.expect("a notice");
        assert_eq!(title, "Desktop · allele — needs input");
        assert_eq!(body, "fix-dispatch is blocked and waiting for you");
    }

    #[test]
    fn a_new_session_that_arrives_waiting_notifies() {
        let e = entry("a", "new", FeedStatus::AwaitingInput);
        let fx = decide(&into_waiting(peer(), "a", None), "D", Some(&e), ALL_ON);
        assert!(fx.notice.is_some());
    }

    #[test]
    fn staying_in_awaiting_input_does_not_notify_again() {
        let e = entry("a", "one", FeedStatus::AwaitingInput);
        let fx = decide(
            &into_waiting(peer(), "a", Some(FeedStatus::AwaitingInput)),
            "D",
            Some(&e),
            ALL_ON,
        );
        assert_eq!(fx, Effects::default());
    }

    #[test]
    fn leaving_awaiting_input_does_not_notify() {
        let e = entry("a", "one", FeedStatus::Running);
        let t = HostTransition {
            peer: peer(),
            transition: Transition {
                id: "a".into(),
                from: Some(FeedStatus::AwaitingInput),
                to: FeedStatus::Running,
            },
        };
        assert_eq!(decide(&t, "D", Some(&e), ALL_ON), Effects::default());
    }

    #[test]
    fn local_settings_gate_remote_notifications() {
        let e = entry("a", "one", FeedStatus::AwaitingInput);
        let t = into_waiting(peer(), "a", Some(FeedStatus::Idle));

        let quiet = decide(
            &t,
            "D",
            Some(&e),
            Gates {
                notifications_enabled: false,
                sound_on_awaiting_input: true,
            },
        );
        assert!(quiet.notice.is_none(), "notifications off → no banner");
        assert!(quiet.sound, "the sound has its own switch");

        let silent = decide(
            &t,
            "D",
            Some(&e),
            Gates {
                notifications_enabled: true,
                sound_on_awaiting_input: false,
            },
        );
        assert!(silent.notice.is_some());
        assert!(!silent.sound, "sound off → no chime");
    }

    #[test]
    fn a_transition_for_an_unknown_session_does_nothing() {
        let t = into_waiting(peer(), "gone", Some(FeedStatus::Running));
        assert_eq!(decide(&t, "D", None, ALL_ON), Effects::default());
    }

    #[test]
    fn display_text_strips_line_breaks_and_bidi_overrides() {
        assert_eq!(display_text("a\nb\u{202E}c\u{200B}d"), "a bcd");
        assert_eq!(display_text("  x \t y  "), "x y");
        assert_eq!(
            display_text(&"x".repeat(200)).chars().count(),
            MAX_DISPLAY_CHARS + 1
        );
        // A space just before the cap counts against it too.
        let spaced = format!("{} {}", "x".repeat(MAX_DISPLAY_CHARS - 1), "y".repeat(10));
        let cut = display_text(&spaced);
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.chars().count() <= MAX_DISPLAY_CHARS + 1, "{cut}");
    }

    /// The string literals in an AppleScript source line, unescaped, plus
    /// whatever lies outside them. A literal that ends early shows up as
    /// extra code outside the literals — which is what an injection is.
    fn applescript_literals(src: &str) -> (Vec<String>, String) {
        let mut literals = Vec::new();
        let mut outside = String::new();
        let mut chars = src.chars();
        while let Some(c) = chars.next() {
            if c != '"' {
                outside.push(c);
                continue;
            }
            let mut lit = String::new();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some('n') => lit.push('\n'),
                        Some('r') => lit.push('\r'),
                        Some(other) => lit.push(other),
                        None => panic!("dangling escape in {src:?}"),
                    },
                    Some(other) => lit.push(other),
                    None => panic!("unterminated literal in {src:?}"),
                }
            }
            literals.push(lit);
            outside.push('@');
        }
        (literals, outside)
    }

    #[test]
    fn a_hostile_remote_label_and_host_name_reach_osascript_escaped() {
        // Quotes and backslashes in every peer-chosen field: the host's
        // machine name, the session label and the project name.
        let mut e = entry("a", HOSTILE, FeedStatus::AwaitingInput);
        e.project_name = HOSTILE.into();
        let fx = decide(
            &into_waiting(peer(), "a", Some(FeedStatus::Running)),
            HOSTILE,
            Some(&e),
            ALL_ON,
        );
        let (title, body) = fx.notice.expect("a notice");
        assert!(body.contains(HOSTILE), "the label is shown, not mangled");
        assert!(title.contains(HOSTILE), "so is the host name");

        let script = hooks::notification_script(&title, &body);
        let (literals, outside) = applescript_literals(&script);
        assert_eq!(
            outside, "display notification @ with title @",
            "nothing escaped its string literal: {script}"
        );
        assert_eq!(literals, vec![body, title], "both round-trip exactly");
    }
}
