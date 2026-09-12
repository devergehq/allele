//! Writing to a session's terminal (DEV-430).
//!
//! One place for the byte sequences allele sends to a session on a caller's
//! behalf. Delivery is the fragile part of this system — it has been lost
//! twice in production — so the writes live together rather than being
//! rediscovered at each call site.

use std::time::Duration;

use alacritty_terminal::term::TermMode;
use gpui::{Context, Entity};

use crate::app_state::AppState;
use crate::terminal::TerminalView;

/// Escape — the keystroke that stops what an agent is doing.
///
/// Not Ctrl-C. Claude Code's own UI says "esc to interrupt" while it works,
/// and allele already uses Escape as the decline keystroke for a permission
/// prompt (DEV-78), so this matches both the agent and the rest of the app.
///
/// Ctrl-C is also worse than merely inaccurate here: two in quick succession
/// exit Claude Code outright, which would kill the session rather than stop
/// its turn. An automated caller retrying an interrupt is exactly the thing
/// that would trigger that.
pub(super) const INTERRUPT: &[u8] = b"\x1b";

/// Press Enter.
///
/// Safe to send repeatedly: on an empty input it is a no-op, which is what
/// makes retry-until-observed safe to do blindly. Never retry [`paste`] the
/// same way — that would send the prompt twice.
pub(super) fn submit(state: &AppState, session_id: &str, cx: &Context<AppState>) -> bool {
    with_terminal(state, session_id, cx, |t| {
        t.write(b"\r");
    })
}

/// Press Escape — stop whatever the agent is currently doing.
pub(super) fn interrupt(state: &AppState, session_id: &str, cx: &Context<AppState>) -> bool {
    with_terminal(state, session_id, cx, |t| {
        t.write(INTERRUPT);
    })
}

/// How long to wait for the agent's input editor before pasting anyway.
///
/// Generous on purpose: a cold agent on a loaded machine takes seconds to draw
/// its first frame, and waiting costs nothing a caller notices, while pasting
/// early costs the prompt.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often readiness is re-checked while waiting.
const READY_POLL: Duration = Duration::from_millis(50);

/// Gap between the paste and the Enter that submits it.
///
/// Claude Code's input editor treats bytes arriving back-to-back as one paste,
/// absorbing a trailing `\r` as another newline instead of firing the submit.
/// The gap is what makes the Enter read as a keystroke.
const SUBMIT_GAP: Duration = Duration::from_millis(80);

/// When to re-send the submit keystroke after delivering a *new session's*
/// first prompt, as milliseconds since the first attempt.
///
/// Only safe where nobody is typing. A new session qualifies: it was created a
/// moment ago and no one is at its keyboard, so a repeated Enter can only land
/// on an empty input, where it is a no-op. `scratch_pad_send` deliberately
/// passes `&[]` instead — it targets the session the user is looking at.
pub(crate) const CREATION_SUBMIT_RETRIES_MS: &[u64] = &[1_000, 2_500, 5_000, 10_000, 20_000];

/// Deliver a composed payload into a session's terminal (DEV-603).
///
/// One primitive for every path that types into an agent on someone's behalf.
/// There were three copies of this sequence and they had already drifted: only
/// the clipboard path in `terminal_view.rs` checked whether the terminal could
/// accept a bracketed paste, while the two that deliver on a caller's behalf
/// emitted the markers unconditionally.
///
/// Three steps:
///
/// 1. **Wait for the input editor.** `BRACKETED_PASTE` is set by the
///    application when it initialises its editor, so it is direct evidence
///    from the agent's own terminal that there is something there to receive
///    the paste — rather than a timer's guess about how long booting takes.
///    Bounded by [`READY_TIMEOUT`]; on expiry the payload is written plainly,
///    with no markers, exactly as the clipboard path does when the mode is
///    off. **The fallback is not optional**: an agent-less Shell session may
///    never set the mode, and gating delivery on it with no escape would turn
///    a delivery that works today into a silent never.
/// 2. **Paste**, bracketed only when the mode is actually on.
/// 3. **Submit** after [`SUBMIT_GAP`], re-sending on `retries`.
pub(crate) fn deliver(
    tv: &Entity<TerminalView>,
    payload: String,
    retries: &'static [u64],
    cx: &mut Context<AppState>,
) {
    let tv = tv.downgrade();
    cx.spawn(async move |_this, cx| {
        // Step 1 — readiness.
        let mut waited = Duration::ZERO;
        let bracketed = loop {
            match cx.update(|cx| {
                tv.upgrade()
                    .and_then(|tv| tv.read(cx).pty().map(accepts_bracketed_paste))
            }) {
                Some(true) => break true,
                Some(false) => {}
                // No terminal, or the app is going away: nothing to deliver
                // into, and nothing a caller can do about it.
                None => return,
            }
            if waited >= READY_TIMEOUT {
                break false;
            }
            cx.background_executor().timer(READY_POLL).await;
            waited += READY_POLL;
        };

        // Step 2 — paste.
        let bytes = paste_sequence(&payload, bracketed);
        if !write_bytes(&tv, &bytes, cx) {
            return;
        }

        // Step 3 — submit, then retry on the schedule the caller chose.
        cx.background_executor().timer(SUBMIT_GAP).await;
        let mut sent_at_ms = 0u64;
        let mut schedule = retries.iter();
        loop {
            if !write_bytes(&tv, b"\r", cx) {
                return;
            }
            let Some(&due) = schedule.next() else { return };
            cx.background_executor()
                .timer(Duration::from_millis(due.saturating_sub(sent_at_ms)))
                .await;
            sent_at_ms = due;
        }
    })
    .detach();
}

/// Whether the terminal has bracketed paste enabled — i.e. whether an
/// application is up and listening for input.
fn accepts_bracketed_paste(terminal: &crate::terminal::pty_terminal::PtyTerminal) -> bool {
    terminal
        .term
        .lock()
        .mode()
        .contains(TermMode::BRACKETED_PASTE)
}

/// The exact bytes a paste puts on the wire.
///
/// Unbracketed is not a degraded copy of bracketed — it is the correct thing
/// to send to a terminal that has not enabled the mode, because there the
/// markers are not markers, just escape bytes landing in the input.
fn paste_sequence(payload: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return payload.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
}

/// Write to the view's PTY if it still has one. False means the terminal or
/// the app has gone, and the caller should stop.
fn write_bytes(tv: &gpui::WeakEntity<TerminalView>, bytes: &[u8], cx: &mut gpui::AsyncApp) -> bool {
    cx.update(|cx| {
        tv.upgrade()
            .and_then(|tv| tv.read(cx).pty().map(|terminal| terminal.write(bytes)))
            .is_some()
    })
}

/// Run `f` against a session's PTY. Returns false when the session is unknown
/// or has no terminal yet — still being cloned, or suspended — which callers
/// treat as "not delivered" rather than as an error.
fn with_terminal(
    state: &AppState,
    session_id: &str,
    cx: &Context<AppState>,
    f: impl FnOnce(&crate::terminal::pty_terminal::PtyTerminal),
) -> bool {
    let Some(session) = state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id)
    else {
        return false;
    };
    let Some(view) = session.terminal_view.as_ref() else {
        return false;
    };
    match view.read(cx).pty() {
        Some(terminal) => {
            f(terminal);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback path, and the reason the readiness wait is bounded rather
    /// than required: a terminal without the mode on must receive the payload
    /// itself, because there the markers are not markers.
    #[test]
    fn an_unbracketed_paste_is_the_payload_itself() {
        assert_eq!(paste_sequence("do the thing", false), b"do the thing");
    }

    #[test]
    fn a_bracketed_paste_is_wrapped_in_the_markers() {
        assert_eq!(
            paste_sequence("do the thing", true),
            b"\x1b[200~do the thing\x1b[201~"
        );
    }

    /// The brief has to arrive byte-for-byte either way — a delivery surface
    /// that reworded or truncated what it delivered would be unusable.
    #[test]
    fn the_payload_survives_both_forms_verbatim() {
        let payload = "line one\nline two\twith a tab";
        for bracketed in [true, false] {
            let bytes = paste_sequence(payload, bracketed);
            let sent = String::from_utf8(bytes).expect("utf8");
            assert!(sent.contains(payload), "bracketed={bracketed}");
        }
    }

    /// Strictly increasing, so the backoff actually backs off: a schedule that
    /// repeated or went backwards would hammer a booting TUI.
    #[test]
    fn the_creation_retry_schedule_backs_off() {
        for pair in CREATION_SUBMIT_RETRIES_MS.windows(2) {
            assert!(pair[1] > pair[0], "{:?} is not increasing", pair);
        }
    }

    /// The first retry has to be soon enough to fix the common case quickly
    /// and late enough that a normally-booting TUI has drawn.
    #[test]
    fn the_first_creation_retry_is_prompt_but_not_instant() {
        let first = CREATION_SUBMIT_RETRIES_MS[0];
        assert!((500..=2_000).contains(&first), "first retry at {first}ms");
    }

    /// The submit gap exists to separate the Enter from the paste; a readiness
    /// wait shorter than it would make the gap meaningless, and an unbounded
    /// one would hang delivery to a terminal that never sets the mode.
    #[test]
    fn the_readiness_wait_is_bounded_and_outlasts_the_submit_gap() {
        assert!(READY_TIMEOUT > SUBMIT_GAP);
        assert!(READY_TIMEOUT <= Duration::from_secs(30));
        assert!(READY_POLL < SUBMIT_GAP);
    }
}
