//! Managing a session after it exists: interrupt, and follow-up prompt
//! (DEV-430).
//!
//! Both are **dispatched-only**, on the same line as discard: an agent may
//! manage what agents created. Typing into or interrupting a human's session
//! is not the agent's to do, and a prompt appearing in a session someone is
//! mid-thought in is worse than the inconvenience of refusing.

use std::time::Duration;

use gpui::{AsyncApp, WeakEntity};

use crate::app_state::AppState;
use crate::dispatch::protocol::{ErrorCode, InterruptedSession, Response, SessionState};
use crate::dispatch::{pty, update_in_main_window};
use crate::session::SessionStatus;

/// How long to wait for an interrupted session to stop.
///
/// Short: Ctrl-C either lands quickly or the session was not doing what the
/// caller thought. A long wait would just delay reporting that.
const INTERRUPT_SETTLE: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(100);

/// Look a session up and check it is ours to manage.
///
/// Returns its label and current status, or the response to send back.
fn claim(state: &AppState, session_id: &str) -> Result<(String, SessionStatus), Response> {
    let Some(session) = state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id)
    else {
        return Err(Response::Error {
            code: ErrorCode::BadRequest,
            message: format!("no session with id {session_id}"),
        });
    };
    if !session.origin.is_dispatched() {
        return Err(Response::Error {
            code: ErrorCode::NotDispatched,
            message: format!(
                "session {:?} was started by a human — interrupting or prompting it \
                 is not an agent's to do",
                session.label
            ),
        });
    }
    Ok((session.label.clone(), session.status))
}

pub(crate) fn spawn_interrupt(
    session_id: String,
    reply: std::sync::mpsc::Sender<Response>,
    this: WeakEntity<AppState>,
    cx: &mut AsyncApp,
) {
    cx.spawn(async move |cx| {
        let _ = reply.send(interrupt(session_id, &this, cx).await);
    })
    .detach();
}

async fn interrupt(session_id: String, this: &WeakEntity<AppState>, cx: &mut AsyncApp) -> Response {
    let claimed = update_in_main_window(this, cx, |state, _w, cx| {
        let (name, was) = match claim(state, &session_id) {
            Ok(v) => v,
            Err(r) => return Err(r),
        };
        // A suspended session has no PTY. Reporting "interrupted" when
        // nothing was written would be a lie a caller then acts on.
        if !pty::interrupt(state, &session_id, cx) {
            return Err(Response::Error {
                code: ErrorCode::Internal,
                message: format!("session {name:?} has no terminal attached to interrupt"),
            });
        }
        Ok((name, was))
    });
    let (name, was_state) = match claimed {
        Ok(Ok(v)) => v,
        Ok(Err(response)) => return response,
        Err(e) => return internal(e.to_string()),
    };

    // Only a session that was working has anything to stop. Waiting on an
    // idle one would report a settle that never had to happen.
    let mut now = was_state;
    if was_state == SessionStatus::Running {
        for _ in 0..(INTERRUPT_SETTLE.as_millis() / POLL.as_millis()) {
            cx.background_executor().timer(POLL).await;
            match update_in_main_window(this, cx, |state, _w, _cx| status_of(state, &session_id)) {
                Ok(Some(s)) if s != SessionStatus::Running => {
                    now = s;
                    break;
                }
                Ok(Some(s)) => now = s,
                Ok(None) => break,
                Err(e) => return internal(e.to_string()),
            }
        }
    }

    Response::Interrupted {
        session: InterruptedSession {
            session_id,
            name,
            was_state: SessionState::from(was_state),
            now_state: SessionState::from(now),
        },
    }
}

fn status_of(state: &AppState, session_id: &str) -> Option<SessionStatus> {
    state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id)
        .map(|s| s.status)
}

fn internal(message: String) -> Response {
    Response::Error {
        code: ErrorCode::Internal,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Escape, not Ctrl-C. Claude Code's UI says "esc to interrupt" while it
    /// works, and two Ctrl-Cs in quick succession exit it outright — which an
    /// automated caller retrying an interrupt would trigger, killing the
    /// session instead of stopping its turn.
    #[test]
    fn interrupt_sends_escape_not_ctrl_c() {
        assert_eq!(crate::dispatch::pty::INTERRUPT, b"\x1b");
        assert_ne!(crate::dispatch::pty::INTERRUPT, [0x03]);
    }

    /// The settle wait must be short. Escape either lands quickly or the
    /// session was not doing what the caller thought, and a long wait would
    /// only delay reporting that.
    #[test]
    fn the_settle_wait_is_short() {
        assert!(INTERRUPT_SETTLE <= Duration::from_secs(15));
        assert!(INTERRUPT_SETTLE > POLL);
    }
}
