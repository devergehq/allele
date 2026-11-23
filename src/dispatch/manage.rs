//! Managing a session after it exists: interrupt (DEV-430), and follow-up
//! prompt (DEV-1382).
//!
//! Both are **dispatched-only**, on the same line as discard: an agent may
//! manage what agents created. Typing into or interrupting a human's session
//! is not the agent's to do, and a prompt appearing in a session someone is
//! mid-thought in is worse than the inconvenience of refusing.
//!
//! DEV-430 cut the prompt half because `SendMessage` already reached a
//! session. It reaches one only when the caller speaks Claude Code's messaging
//! socket, which `address.rs` calls a convention rather than a contract; a desk
//! integrating with Allele needs a path Allele owns. DEV-1382 adds it, typed
//! through [`pty::deliver`] like every other delivery, and refuses wherever an
//! Enter could answer something a person was meant to.

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use gpui::{AsyncApp, WeakEntity};

use crate::app_state::AppState;
use crate::catch_up::facts::{TerminalAsk, WantsNow};
use crate::dispatch::protocol::{
    no_such_session, ErrorCode, InterruptedSession, Response, SessionState,
};
use crate::dispatch::{pty, update_in_main_window};
use crate::session::{AttentionKind, Session, SessionStatus};

/// How long to wait for an interrupted session to stop.
///
/// Short: Ctrl-C either lands quickly or the session was not doing what the
/// caller thought. A long wait would just delay reporting that.
const INTERRUPT_SETTLE: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(100);

/// How long to wait for a prompted session to start on its prompt.
///
/// Far shorter than creation's two minutes: this session is already up, its
/// input editor is drawn, and Claude reports the submit within a second. The
/// bracketed-paste wait inside [`pty::deliver`] (10 s at most) fits inside it.
const PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// Look a session up and check it is ours to manage.
///
/// Returns the session, or the response to send back.
fn claim<'a>(state: &'a AppState, session_id: &str) -> Result<&'a Session, Response> {
    let Some(session) = state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id)
    else {
        return Err(no_such_session(session_id));
    };
    managed(session)?;
    Ok(session)
}

/// Refuse a session a person started: an agent manages what agents created.
fn managed(session: &Session) -> Result<(), Response> {
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
    Ok(())
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
            Ok(s) => (s.label.clone(), s.status),
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

// ── sessions_prompt (DEV-1382) ──────────────────────────────────────────

pub(crate) fn spawn_prompt(
    session_id: String,
    prompt: String,
    reply: std::sync::mpsc::Sender<Response>,
    this: WeakEntity<AppState>,
    cx: &mut AsyncApp,
) {
    cx.spawn(async move |cx| {
        let _ = reply.send(prompt_session(session_id, prompt, &this, cx).await);
    })
    .detach();
}

/// Sessions a prompt is being delivered to right now.
///
/// Status alone cannot refuse a second prompt: a session still reads
/// `response_ready` until Claude reports the turn the first one started, and a
/// second paste in that gap would land in the same input box and submit both
/// as one message.
static IN_FLIGHT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Holds a session's place in [`IN_FLIGHT`] until the prompt is confirmed or
/// given up on, whichever way the task ends.
struct InFlight(String);

impl InFlight {
    fn take(session_id: &str) -> Option<Self> {
        let mut busy = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if busy.iter().any(|id| id == session_id) {
            return None;
        }
        busy.push(session_id.to_string());
        Some(InFlight(session_id.to_string()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut busy = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        busy.retain(|id| id != &self.0);
    }
}

/// What is wrong with a prompt before any session is looked at.
fn check_prompt(prompt: &str) -> Result<(), Response> {
    let refuse = |message: String| {
        Err(Response::Error {
            code: ErrorCode::BadRequest,
            message,
        })
    };
    if prompt.trim().is_empty() {
        return refuse("prompt is empty".to_string());
    }
    let chars = prompt.chars().count();
    if chars > allele_contract::PROMPT_MAX_CHARS {
        return refuse(format!(
            "prompt is {chars} characters; the limit is {}. Point at an artifact instead",
            allele_contract::PROMPT_MAX_CHARS
        ));
    }
    Ok(())
}

/// Whether Allele may type into `session` now. Decided before anything is
/// written, so every refusal leaves the terminal untouched.
///
/// The menu check reads [`crate::catch_up::gather::wants_from_session`], the
/// source the catch-up strip draws "only the terminal can answer this" from.
/// Two rules sit on top of it, both refusing where it would say "nothing":
///
/// - `awaiting_input` with no attention context. The catch-up strip reads
///   that as nothing blocked; here it means Allele cannot tell what is on
///   screen, and a guess that is wrong types Enter into a menu.
/// - A blocking attention context on any status. It cannot outlive
///   `awaiting_input` today, so this costs nothing until a new status path
///   forgets to clear it.
///
/// Claude's sixty-second idle nudge (`awaiting_input`, kind `Idle`) is
/// deliverable: the input box is still what has focus. After a finished turn
/// the nudge is absorbed (`absorbs_idle_nudge`), so it shows here only where
/// no turn-complete came first. `away.rs` and `decisions::target` make the
/// same call.
fn gate(session: &Session, has_terminal: bool) -> Result<(), Response> {
    let name = &session.label;
    let refuse = |code: ErrorCode, message: String| Err(Response::Error { code, message });
    match session.status {
        SessionStatus::Suspended | SessionStatus::Done => {
            return refuse(
                ErrorCode::SessionNotLive,
                format!(
                    "session {name:?} is {}; there is no agent process to type into",
                    wire_name(session.status)
                ),
            )
        }
        _ if !has_terminal => {
            return refuse(
                ErrorCode::SessionNotLive,
                format!("session {name:?} has no terminal attached to type into"),
            )
        }
        SessionStatus::Running => {
            return refuse(
                ErrorCode::SessionBusy,
                format!("session {name:?} is running; try again once it finishes"),
            )
        }
        SessionStatus::AwaitingInput if session.attention_context.is_none() => {
            return refuse(
                ErrorCode::SessionAwaitingPerson,
                format!(
                    "session {name:?} is waiting for input and Allele cannot tell on \
                     what; nothing was typed"
                ),
            )
        }
        _ => {}
    }
    let awaiting = |what: &str| {
        refuse(
            ErrorCode::SessionAwaitingPerson,
            format!(
                "session {name:?} is waiting on {what}; Enter there would answer it, so \
                 nothing was typed"
            ),
        )
    };
    match crate::catch_up::gather::wants_from_session(session) {
        WantsNow::Permission { .. } => return awaiting("a permission prompt"),
        WantsNow::TerminalOnly {
            ask: TerminalAsk::Plan,
        } => return awaiting("a plan approval menu"),
        WantsNow::TerminalOnly {
            ask: TerminalAsk::Question,
        } => return awaiting("a question menu"),
        WantsNow::Working { .. } => {
            return refuse(
                ErrorCode::SessionBusy,
                format!("session {name:?} is working; try again once it finishes"),
            )
        }
        WantsNow::Nothing { .. } => {}
    }
    if session
        .attention_context
        .as_ref()
        .is_some_and(|c| c.kind.is_blocking())
    {
        return awaiting("a prompt");
    }
    Ok(())
}

fn wire_name(status: SessionStatus) -> String {
    serde_json::to_value(SessionState::from(status))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// What the confirmation wait compares.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Seen {
    status: SessionStatus,
    turn: u64,
    since: SystemTime,
    attention: Option<AttentionKind>,
}

impl Seen {
    fn of(session: &Session) -> Self {
        Seen {
            status: session.status,
            turn: session.turn,
            since: session.status_since,
            attention: session.attention_context.as_ref().map(|c| c.kind),
        }
    }
}

/// Whether the session has visibly started on the prompt since `before`.
///
/// It must have *moved* — a new turn, or a status change — and landed
/// somewhere only a submitted prompt leads: `running`, `response_ready`, or a
/// blocking `awaiting_input`. A turn counter is checked as well as the status
/// because a quick turn can go `response_ready` → `running` →
/// `response_ready` between two looks. An idle nudge is not evidence: it is
/// what an unsent prompt sitting in the input box produces a minute later.
fn landed(before: Seen, after: Seen) -> bool {
    let moved =
        after.turn > before.turn || after.status != before.status || after.since > before.since;
    moved
        && match after.status {
            SessionStatus::Running | SessionStatus::ResponseReady => true,
            SessionStatus::AwaitingInput => after.attention.is_some_and(|k| k.is_blocking()),
            SessionStatus::Idle | SessionStatus::Suspended | SessionStatus::Done => false,
        }
}

async fn prompt_session(
    session_id: String,
    prompt: String,
    this: &WeakEntity<AppState>,
    cx: &mut AsyncApp,
) -> Response {
    if let Err(response) = check_prompt(&prompt) {
        return response;
    }

    // Gate and paste in one foreground update, so nothing can change the
    // session between deciding it is safe and handing the bytes to delivery.
    let delivered = update_in_main_window(this, cx, |state, _w, cx| {
        let session = claim(state, &session_id)?;
        let view = session.terminal_view.clone();
        let has_terminal = view.as_ref().is_some_and(|v| v.read(cx).pty().is_some());
        gate(session, has_terminal)?;
        let Some(guard) = InFlight::take(&session_id) else {
            return Err(Response::Error {
                code: ErrorCode::SessionBusy,
                message: format!(
                    "session {:?} is already being prompted; try again once it has started",
                    session.label
                ),
            });
        };
        let before = Seen::of(session);
        let name = session.label.clone();
        let Some(view) = view else {
            return Err(internal("terminal vanished while being checked".into()));
        };
        // No Enter retries. Creation can re-send Enter because nobody is at a
        // new session yet; here a retry that lands after the prompt has
        // started could answer the permission prompt it raised.
        pty::deliver(&view, prompt, &[], cx);
        Ok((name, before, guard))
    });
    let (name, before, _guard) = match delivered {
        Ok(Ok(v)) => v,
        Ok(Err(response)) => return response,
        Err(e) => return internal(e.to_string()),
    };

    for _ in 0..(PROMPT_CONFIRM_TIMEOUT.as_millis() / POLL.as_millis()) {
        cx.background_executor().timer(POLL).await;
        let seen = update_in_main_window(this, cx, |state, _w, _cx| {
            state
                .projects
                .iter()
                .flat_map(|p| p.sessions.iter())
                .find(|s| s.id == session_id)
                .map(Seen::of)
        });
        match seen {
            Ok(Some(after)) if landed(before, after) => {
                return Response::Prompted {
                    session: InterruptedSession {
                        session_id,
                        name,
                        was_state: SessionState::from(before.status),
                        now_state: SessionState::from(after.status),
                    },
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return internal(format!(
                    "session {name:?} disappeared before its prompt was confirmed"
                ))
            }
            Err(e) => return internal(e.to_string()),
        }
    }

    Response::Error {
        code: ErrorCode::PromptDeliveryUnconfirmed,
        message: format!(
            "the prompt was typed into session {name:?}, but Allele did not see it start \
             within {}s. It is most likely sitting unsent in the input box; Enter in that \
             session will send it. Do not send it again, or it will arrive twice",
            PROMPT_CONFIRM_TIMEOUT.as_secs()
        ),
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

    // ── sessions_prompt (DEV-1382) ──

    use crate::session::{AttentionContext, SessionOrigin};

    fn session(status: SessionStatus) -> Session {
        let now = SystemTime::now();
        let mut s = Session::suspended_from_persisted(
            "s1".into(),
            "Review acme/portal#514".into(),
            now,
            now,
            Duration::ZERO,
            None,
        );
        s.status = status;
        s.origin = SessionOrigin::Dispatched {
            by_session_id: "parent".into(),
            by_label: "Dispatcher".into(),
            depth: 1,
        };
        s
    }

    fn waiting_on(tool: Option<&str>, kind: AttentionKind) -> Session {
        let mut s = session(SessionStatus::AwaitingInput);
        s.attention_context = Some(AttentionContext {
            kind,
            tool_name: tool.map(str::to_string),
            tool_input_summary: None,
            message: None,
            request: None,
        });
        s
    }

    fn code(r: Result<(), Response>) -> Option<ErrorCode> {
        match r {
            Ok(()) => None,
            Err(Response::Error { code, .. }) => Some(code),
            Err(other) => panic!("refused with {other:?}"),
        }
    }

    #[test]
    fn a_finished_turn_or_a_fresh_session_takes_a_prompt() {
        assert_eq!(
            code(gate(&session(SessionStatus::ResponseReady), true)),
            None
        );
        assert_eq!(code(gate(&session(SessionStatus::Idle), true)), None);
    }

    /// Claude's sixty-second idle nudge leaves the input box focused. After a
    /// finished turn it is absorbed and the session stays `response_ready`;
    /// it shows as `awaiting_input` only where no turn-complete came first (an
    /// interrupted turn, say), and refusing there would strand the session
    /// until a person typed.
    #[test]
    fn the_idle_nudge_takes_a_prompt() {
        let s = waiting_on(None, AttentionKind::Idle);
        assert_eq!(code(gate(&s, true)), None);
    }

    #[test]
    fn a_working_session_is_busy() {
        assert_eq!(
            code(gate(&session(SessionStatus::Running), true)),
            Some(ErrorCode::SessionBusy)
        );
    }

    #[test]
    fn a_permission_prompt_waits_for_a_person() {
        let s = waiting_on(Some("Bash"), AttentionKind::Permission);
        assert_eq!(code(gate(&s, true)), Some(ErrorCode::SessionAwaitingPerson));
    }

    #[test]
    fn a_plan_or_question_menu_waits_for_a_person() {
        for tool in ["ExitPlanMode", "AskUserQuestion"] {
            let s = waiting_on(Some(tool), AttentionKind::Question);
            assert_eq!(
                code(gate(&s, true)),
                Some(ErrorCode::SessionAwaitingPerson),
                "{tool}"
            );
        }
    }

    /// The catch-up strip reads no context as "nothing blocked". Here that
    /// would be a guess, and a wrong guess types Enter into a menu.
    #[test]
    fn waiting_on_something_unreadable_is_refused() {
        let s = session(SessionStatus::AwaitingInput);
        assert!(s.attention_context.is_none());
        assert_eq!(code(gate(&s, true)), Some(ErrorCode::SessionAwaitingPerson));
    }

    /// A blocking context outside `awaiting_input` cannot happen today; if a
    /// new status path ever leaves one behind, refusing is the safe reading.
    #[test]
    fn a_menu_left_open_on_a_finished_turn_is_refused() {
        let mut s = waiting_on(Some("AskUserQuestion"), AttentionKind::Question);
        s.status = SessionStatus::ResponseReady;
        assert_eq!(code(gate(&s, true)), Some(ErrorCode::SessionAwaitingPerson));
    }

    #[test]
    fn a_session_with_no_process_is_not_live() {
        for status in [SessionStatus::Suspended, SessionStatus::Done] {
            assert_eq!(
                code(gate(&session(status), true)),
                Some(ErrorCode::SessionNotLive),
                "{status:?}"
            );
        }
        assert_eq!(
            code(gate(&session(SessionStatus::ResponseReady), false)),
            Some(ErrorCode::SessionNotLive)
        );
    }

    #[test]
    fn a_human_session_is_not_an_agents_to_prompt() {
        let mut s = session(SessionStatus::ResponseReady);
        assert_eq!(code(managed(&s)), None);
        s.origin = SessionOrigin::Human;
        assert_eq!(code(managed(&s)), Some(ErrorCode::NotDispatched));
    }

    #[test]
    fn an_empty_or_oversized_prompt_is_a_bad_request() {
        assert_eq!(code(check_prompt("")), Some(ErrorCode::BadRequest));
        assert_eq!(code(check_prompt(" \n\t")), Some(ErrorCode::BadRequest));
        let max = allele_contract::PROMPT_MAX_CHARS;
        // Characters, not bytes: 4,000 two-byte characters is within the limit.
        assert_eq!(code(check_prompt(&"é".repeat(max))), None);
        assert_eq!(
            code(check_prompt(&"a".repeat(max + 1))),
            Some(ErrorCode::BadRequest)
        );
        assert_eq!(max, 4_000);
    }

    /// Two prompts in flight to one session would land in the same input box
    /// and submit as one message, so the second is refused until the first
    /// is settled either way.
    #[test]
    fn a_second_prompt_in_flight_is_refused_until_the_first_settles() {
        let first = InFlight::take("dev-1382-inflight").expect("free");
        assert!(InFlight::take("dev-1382-inflight").is_none());
        assert!(InFlight::take("dev-1382-other").is_some());
        drop(first);
        assert!(InFlight::take("dev-1382-inflight").is_some());
    }

    fn seen(status: SessionStatus, turn: u64, since: SystemTime) -> Seen {
        Seen {
            status,
            turn,
            since,
            attention: None,
        }
    }

    #[test]
    fn starting_a_turn_confirms_the_prompt() {
        let t = SystemTime::now();
        let before = seen(SessionStatus::ResponseReady, 3, t);
        assert!(landed(before, seen(SessionStatus::Running, 4, t)));
        let from_idle = seen(SessionStatus::Idle, 0, t);
        assert!(landed(from_idle, seen(SessionStatus::Running, 1, t)));
    }

    /// A short turn can finish between two looks; the turn count catches it.
    #[test]
    fn a_turn_that_finished_between_looks_confirms_it() {
        let t = SystemTime::now();
        let before = seen(SessionStatus::ResponseReady, 3, t);
        assert!(landed(before, seen(SessionStatus::ResponseReady, 4, t)));
    }

    /// A session that never leaves the state it was prompted in has not
    /// started on anything: that is `prompt_delivery_unconfirmed`.
    #[test]
    fn a_session_that_never_moves_is_unconfirmed() {
        let t = SystemTime::now();
        let before = seen(SessionStatus::ResponseReady, 3, t);
        assert!(!landed(before, before));
        let idle = seen(SessionStatus::Idle, 0, t);
        assert!(!landed(idle, idle));
    }

    /// The idle nudge is what an unsent prompt produces a minute later, so
    /// reaching it is not evidence the prompt was read. A permission prompt
    /// is: only a turn raises one.
    #[test]
    fn only_a_blocking_wait_counts_as_having_started() {
        let t = SystemTime::now();
        let before = seen(SessionStatus::Idle, 0, t);
        let mut nudged = seen(SessionStatus::AwaitingInput, 0, t);
        nudged.attention = Some(AttentionKind::Idle);
        assert!(!landed(before, nudged));
        nudged.attention = Some(AttentionKind::Permission);
        assert!(landed(before, nudged));
    }

    /// The confirmation wait has to outlast `pty::deliver`'s own 10 s wait for
    /// bracketed paste, or a slow terminal is reported unconfirmed before the
    /// paste is even written.
    #[test]
    fn the_confirmation_wait_outlasts_the_paste_wait() {
        assert!(PROMPT_CONFIRM_TIMEOUT >= Duration::from_secs(20));
        assert!(PROMPT_CONFIRM_TIMEOUT < super::super::create::PROMPT_CONFIRM_TIMEOUT);
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
