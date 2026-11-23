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

/// What is wrong with a prompt before any session is looked at.
///
/// Control characters other than newline and tab are refused: inside a
/// bracketed paste they are noise, and outside one ESC, arrows and `\r` are
/// keystrokes that move through and select in a menu. A leading `/`, `!` or
/// `#` is refused because Claude Code reads it as a slash command, bash mode
/// or memory, none of which starts a turn Allele can confirm — and `!` runs a
/// shell command without a permission prompt.
fn check_prompt(prompt: &str) -> Result<(), Response> {
    let refuse = |message: String| {
        Err(Response::Error {
            code: ErrorCode::BadRequest,
            message,
        })
    };
    let text = prompt.trim_start();
    if text.trim_end().is_empty() {
        return refuse("prompt is empty".to_string());
    }
    let chars = prompt.chars().count();
    if chars > allele_contract::PROMPT_MAX_CHARS {
        return refuse(format!(
            "prompt is {chars} characters; the limit is {}. Point at an artifact instead",
            allele_contract::PROMPT_MAX_CHARS
        ));
    }
    if let Some(c) = prompt
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return refuse(format!(
            "prompt contains the control character U+{:04X}; only newline and tab are allowed",
            c as u32
        ));
    }
    if let Some(lead) = text.chars().next().filter(|c| matches!(c, '/' | '!' | '#')) {
        return refuse(format!(
            "prompt starts with {lead:?}, which Claude Code reads as a command, bash mode or \
             memory rather than a message; start it with text"
        ));
    }
    Ok(())
}

/// What the gate needs to know about a session's terminal and its agent
/// process, which the session record does not hold.
#[derive(Debug, Clone, Copy)]
struct Terminal {
    /// A PTY with an agent process that has not exited.
    live: bool,
    /// Allele has had a hook event from that process. Until then `idle` is
    /// only the value a session starts with, and the agent may be on a
    /// startup dialog where Enter picks an option (`away::decide`).
    heard: bool,
    /// The terminal has bracketed paste on, so the prompt goes in as one
    /// paste rather than as keystrokes — and without `pty::deliver`'s 10 s
    /// wait for the mode, which would leave this gate's answer stale.
    accepts_paste: bool,
}

/// Whether Allele may type into `session` now. Decided before anything is
/// written, so every refusal leaves the terminal untouched.
///
/// **Every `awaiting_input` is refused**, including Claude's sixty-second
/// idle nudge. That state is where permission prompts and menus show, the
/// nudge is told apart from them by its notification text alone, and a later
/// notification rewrites the kind without a status change. After a finished
/// turn the nudge is absorbed and the session stays `response_ready`, so the
/// strict reading costs only sessions whose turn never completed.
///
/// The menu check reads [`crate::catch_up::gather::wants_from_session`], the
/// source the catch-up strip draws "only the terminal can answer this" from,
/// and a blocking attention context on any status refuses too: it cannot
/// outlive `awaiting_input` today, so that costs nothing until a new status
/// path forgets to clear one.
///
/// `last_typed` is when Allele last typed into this session — a decision's
/// answer or an earlier `sessions_prompt` (`decisions.last_sent`). Until the
/// session's status has moved since, that text may still be sitting in the
/// input box, and a second paste would be submitted with it as one message.
fn gate(
    session: &Session,
    terminal: Terminal,
    last_typed: Option<SystemTime>,
) -> Result<(), Response> {
    let name = &session.label;
    let refuse = |code: ErrorCode, message: String| Err(Response::Error { code, message });
    let awaiting = |what: &str| {
        refuse(
            ErrorCode::SessionAwaitingPerson,
            format!(
                "session {name:?} is waiting on {what}; Enter there would answer it, so \
                 nothing was typed"
            ),
        )
    };
    let wants = crate::catch_up::gather::wants_from_session(session);
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
        _ if !terminal.live => {
            return refuse(
                ErrorCode::SessionNotLive,
                format!("session {name:?} has no live agent process to type into"),
            )
        }
        SessionStatus::Running => {
            return refuse(
                ErrorCode::SessionBusy,
                format!("session {name:?} is running; try again once it finishes"),
            )
        }
        SessionStatus::AwaitingInput => {
            return match (&wants, session.attention_context.as_ref().map(|c| c.kind)) {
                (WantsNow::Permission { .. }, _) => awaiting("a permission prompt"),
                (
                    WantsNow::TerminalOnly {
                        ask: TerminalAsk::Plan,
                    },
                    _,
                ) => awaiting("a plan approval menu"),
                (WantsNow::TerminalOnly { .. }, _) => awaiting("a question menu"),
                (_, Some(AttentionKind::Idle)) => awaiting(
                    "input (Claude's idle notice, which Allele cannot tell apart from a \
                     prompt with certainty)",
                ),
                _ => awaiting("input, and Allele cannot tell on what"),
            }
        }
        SessionStatus::Idle | SessionStatus::ResponseReady => {}
    }
    if !terminal.heard {
        return refuse(
            ErrorCode::SessionBusy,
            format!(
                "session {name:?} has not finished starting — Allele has not heard from its \
                 agent yet, and it may be on a startup dialog; try again shortly"
            ),
        );
    }
    if last_typed.is_some_and(|sent| session.status_since <= sent) {
        return refuse(
            ErrorCode::SessionBusy,
            format!(
                "Allele typed into session {name:?} and has not seen it start on that yet; \
                 try again once it has. If it never does, the text is sitting unsent in its \
                 input box and a person has to press Enter"
            ),
        );
    }
    match wants {
        WantsNow::Permission { .. } => return awaiting("a permission prompt"),
        WantsNow::TerminalOnly { .. } => return awaiting("a menu"),
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
    if !terminal.accepts_paste {
        return refuse(
            ErrorCode::SessionNotLive,
            format!(
                "session {name:?}'s terminal is not accepting a paste, so there is no agent \
                 input editor to type into"
            ),
        );
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
///
/// **Known limit:** Allele does not record which text a turn started on, so a
/// turn the agent starts by itself inside the window (a background task
/// finishing, say) also reads as confirmation. The `prompted` answer means
/// "the session started a turn after the paste", not "it read this text".
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

    // Gate, record and paste in one foreground update, so nothing can change
    // the session between deciding it is safe and handing the bytes over.
    // With bracketed paste already on, `pty::deliver` writes on its first
    // tick rather than after its 10 s wait for the mode.
    let delivered = update_in_main_window(this, cx, |state, _w, cx| {
        let session = claim(state, &session_id)?;
        let pid = crate::away::live_pid(session, cx);
        let view = session.terminal_view.clone();
        let terminal = Terminal {
            live: pid.is_some(),
            heard: pid.is_some() && session.remote_control.heard_from_pid == pid,
            accepts_paste: view
                .as_ref()
                .and_then(|v| v.read(cx).pty().map(pty::accepts_bracketed_paste))
                .unwrap_or(false),
        };
        let last_typed = state.decisions.last_sent.get(&session_id).copied();
        gate(session, terminal, last_typed)?;
        let before = Seen::of(session);
        let name = session.label.clone();
        let Some(view) = view else {
            return Err(internal("terminal vanished while being checked".into()));
        };
        // Shared with the decision sweep, so neither types on top of the
        // other's unsent text, and a second prompt is refused until this one
        // has visibly started — however long that takes.
        state
            .decisions
            .last_sent
            .insert(session_id.clone(), SystemTime::now());
        // No Enter retries. Creation can re-send Enter because nobody is at a
        // new session yet; here a retry that lands after the prompt has
        // started could answer the permission prompt it raised.
        pty::deliver(&view, prompt, &[], cx);
        Ok((name, before))
    });
    let (name, before) = match delivered {
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
             session will send it. Further prompts are refused with session_busy until it \
             does, so it cannot arrive twice",
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

    /// A live Claude at its prompt: process up, heard from, bracketed paste on.
    const READY: Terminal = Terminal {
        live: true,
        heard: true,
        accepts_paste: true,
    };

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

    fn gate_ready(s: &Session) -> Option<ErrorCode> {
        code(gate(s, READY, None))
    }

    #[test]
    fn a_finished_turn_or_a_fresh_session_takes_a_prompt() {
        assert_eq!(gate_ready(&session(SessionStatus::ResponseReady)), None);
        assert_eq!(gate_ready(&session(SessionStatus::Idle)), None);
    }

    /// F1 of the self-review: before the agent's first hook, `idle` is only
    /// the value a session starts with, and it may be on a startup dialog
    /// (MCP approval, trust, onboarding) where Enter picks an option.
    #[test]
    fn a_session_not_yet_heard_from_is_busy() {
        let unheard = Terminal {
            heard: false,
            ..READY
        };
        for status in [SessionStatus::Idle, SessionStatus::ResponseReady] {
            assert_eq!(
                code(gate(&session(status), unheard, None)),
                Some(ErrorCode::SessionBusy),
                "{status:?}"
            );
        }
    }

    /// The idle nudge is told apart from a permission prompt by its text
    /// alone, and a later notification can relabel an open prompt. So every
    /// `awaiting_input` is refused, as the ticket's acceptance criteria say.
    #[test]
    fn every_awaiting_input_waits_for_a_person() {
        for s in [
            waiting_on(None, AttentionKind::Idle),
            waiting_on(Some("Bash"), AttentionKind::Permission),
            waiting_on(Some("ExitPlanMode"), AttentionKind::Question),
            waiting_on(Some("AskUserQuestion"), AttentionKind::Question),
            session(SessionStatus::AwaitingInput),
        ] {
            assert_eq!(
                gate_ready(&s),
                Some(ErrorCode::SessionAwaitingPerson),
                "{:?}",
                s.attention_context.as_ref().map(|c| c.kind)
            );
        }
    }

    #[test]
    fn a_working_session_is_busy() {
        assert_eq!(
            gate_ready(&session(SessionStatus::Running)),
            Some(ErrorCode::SessionBusy)
        );
    }

    /// A blocking context outside `awaiting_input` cannot happen today; if a
    /// new status path ever leaves one behind, refusing is the safe reading.
    #[test]
    fn a_menu_left_open_on_a_finished_turn_is_refused() {
        let mut s = waiting_on(Some("AskUserQuestion"), AttentionKind::Question);
        s.status = SessionStatus::ResponseReady;
        assert_eq!(gate_ready(&s), Some(ErrorCode::SessionAwaitingPerson));
    }

    #[test]
    fn a_session_with_no_process_is_not_live() {
        for status in [SessionStatus::Suspended, SessionStatus::Done] {
            assert_eq!(
                gate_ready(&session(status)),
                Some(ErrorCode::SessionNotLive),
                "{status:?}"
            );
        }
        let dead = Terminal {
            live: false,
            heard: false,
            accepts_paste: false,
        };
        assert_eq!(
            code(gate(&session(SessionStatus::ResponseReady), dead, None)),
            Some(ErrorCode::SessionNotLive)
        );
    }

    /// F4: without bracketed paste the prompt would go in as keystrokes, and
    /// only after `pty::deliver`'s 10 s wait — by when this answer is stale.
    #[test]
    fn a_terminal_not_accepting_a_paste_is_refused() {
        let plain = Terminal {
            accepts_paste: false,
            ..READY
        };
        assert_eq!(
            code(gate(&session(SessionStatus::ResponseReady), plain, None)),
            Some(ErrorCode::SessionNotLive)
        );
    }

    /// F2: text Allele typed — a decision's answer or an earlier prompt — may
    /// still be in the input box until the session's status moves. Pasting
    /// again would submit both as one message, however long ago it was sent.
    #[test]
    fn text_typed_and_not_yet_started_on_holds_the_session() {
        let s = session(SessionStatus::ResponseReady);
        let after_status = s.status_since + Duration::from_secs(90);
        assert_eq!(
            code(gate(&s, READY, Some(after_status))),
            Some(ErrorCode::SessionBusy)
        );
        assert_eq!(
            code(gate(&s, READY, Some(s.status_since))),
            Some(ErrorCode::SessionBusy),
            "a send in the same instant is not settled"
        );
        let before_status = s.status_since - Duration::from_secs(1);
        assert_eq!(code(gate(&s, READY, Some(before_status))), None);
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

    /// F4: ESC, arrows, `\r` and Ctrl-C would be keystrokes in a menu.
    #[test]
    fn control_characters_other_than_newline_and_tab_are_refused() {
        assert_eq!(code(check_prompt("line one\nline two\tok")), None);
        for bad in ["\u{1b}[B\r", "go\r", "stop\u{3}", "x\u{7f}", "x\u{9b}"] {
            assert_eq!(
                code(check_prompt(bad)),
                Some(ErrorCode::BadRequest),
                "{bad:?}"
            );
        }
    }

    /// F5: a leading `/`, `!` or `#` is a command, bash mode or memory to
    /// Claude Code — none a message, and `!` skips the permission prompt.
    #[test]
    fn a_prompt_that_claude_code_would_read_as_a_mode_is_refused() {
        for bad in ["/exit", "!rm -rf target", "# remember this", "  /clear"] {
            assert_eq!(
                code(check_prompt(bad)),
                Some(ErrorCode::BadRequest),
                "{bad:?}"
            );
        }
        assert_eq!(code(check_prompt("Review Desk: see /tmp/q41")), None);
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
