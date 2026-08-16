//! Writing to a session's terminal (DEV-430).
//!
//! One place for the byte sequences allele sends to a session on a caller's
//! behalf. Delivery is the fragile part of this system — it has been lost
//! twice in production — so the writes live together rather than being
//! rediscovered at each call site.

use gpui::Context;

use crate::app_state::AppState;

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
