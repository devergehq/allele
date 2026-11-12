//! One decision list (DEV-1267).
//!
//! A session posts a question for the developer through `allele_decisions_post`
//! and carries on with the option it assumed. Allele lists every open question
//! in one strip under the attention bar ([`crate::decisions_panel`]). The
//! developer clicks an option, and the answer is typed into the asking session
//! as a normal prompt — **only when that session is idle at its prompt**. A
//! session that is mid-turn or waiting on a permission prompt gets the answer
//! when it next goes idle; a session that has gone gets nothing, and the answer
//! stays in the history.
//!
//! This module holds the book of questions (pure, unit-tested), the rule for
//! when an answer may be sent ([`target`]), the wire types for the control
//! socket, and the `AppState` glue: the three ops, answering, and the sweep
//! that expires and delivers.
//!
//! Stored in `~/.allele/decisions.json` through
//! [`crate::repositories::DecisionsRepository`], written by the dirty-flag
//! coordinator like `state.json`. Private to Allele: nothing else is meant to
//! read it.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{Context, Entity};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::dispatch::protocol::{ErrorCode, Response};
use crate::session::SessionStatus;

// ── Limits ──────────────────────────────────────────────────────────────

pub(crate) const MAX_QUESTION_CHARS: usize = 280;
pub(crate) const MIN_OPTIONS: usize = 2;
pub(crate) const MAX_OPTIONS: usize = 4;
pub(crate) const MAX_OPTION_CHARS: usize = 80;
pub(crate) const MAX_UNBLOCKS: usize = 5;
pub(crate) const MAX_UNBLOCK_CHARS: usize = 60;
pub(crate) const MAX_NOTE_CHARS: usize = 200;
/// Open questions one session may have at once.
pub(crate) const MAX_OPEN_PER_SESSION: usize = 5;
/// Open questions across every session.
pub(crate) const MAX_OPEN_TOTAL: usize = 50;
/// An unanswered question is marked expired after this long.
pub(crate) const EXPIRE_AFTER_SECS: u64 = 24 * 60 * 60;
/// Closed questions kept in the history; the oldest beyond this are dropped so
/// the file cannot grow without bound.
pub(crate) const MAX_HISTORY: usize = 500;
/// How often the sweep runs: expiry, and delivery to sessions that went idle.
pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// The prefix every delivered answer starts with, so the session can tell it
/// from a prompt the developer typed.
pub(crate) const FRAME: &str = "[Your decision via Allele]";

// ── The book ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Open,
    Answered,
    Withdrawn,
    Expired,
}

impl DecisionState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            DecisionState::Open => "open",
            DecisionState::Answered => "answered",
            DecisionState::Withdrawn => "withdrawn",
            DecisionState::Expired => "expired",
        }
    }
}

/// Whether an answer has reached its session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Delivery {
    /// Held until the session is idle at its prompt.
    Waiting,
    /// Typed into the session at `at` (unix seconds).
    Sent { at: u64 },
    /// The session no longer exists; nothing was or will be sent.
    SessionGone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Answer {
    /// Index into the question's options.
    pub choice: usize,
    #[serde(default)]
    pub note: Option<String>,
    pub answered_at: u64,
    pub delivery: Delivery,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub id: String,
    /// Allele's id of the asking session.
    pub session_id: String,
    /// The session's name when it asked — shown once the session has gone.
    pub session_label: String,
    pub question: String,
    pub options: Vec<String>,
    /// Index of the option the session went ahead with meanwhile.
    pub assumed: usize,
    #[serde(default)]
    pub unblocks: Vec<String>,
    /// Unix seconds.
    pub posted_at: u64,
    pub state: DecisionState,
    #[serde(default)]
    pub answer: Option<Answer>,
    /// When it stopped being open (answered, withdrawn or expired).
    #[serde(default)]
    pub closed_at: Option<u64>,
}

/// Everything in `decisions.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DecisionBook {
    #[serde(default)]
    pub decisions: Vec<Decision>,
}

/// A post that passed validation, with its text cleaned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CleanPost {
    pub question: String,
    pub options: Vec<String>,
    pub assumed: usize,
    pub unblocks: Vec<String>,
}

/// Plain text only. Control characters — newlines, tabs, and above all ESC —
/// become spaces: an answer is typed into a terminal, and an escape sequence in
/// a question (a bracketed-paste end marker, say) would otherwise run there.
pub(crate) fn clean(text: &str) -> String {
    let mapped: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn check_len(what: &str, text: &str, max: usize) -> Result<(), String> {
    let n = text.chars().count();
    if n == 0 {
        return Err(format!("{what} is empty"));
    }
    if n > max {
        return Err(format!("{what} is {n} characters; the limit is {max}"));
    }
    Ok(())
}

/// Check a post against the limits. The error is the reason given to the caller.
pub(crate) fn validate(req: &PostRequest) -> Result<CleanPost, String> {
    let question = clean(&req.question);
    check_len("question", &question, MAX_QUESTION_CHARS)?;

    let n = req.options.len();
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&n) {
        return Err(format!(
            "{n} options given; a question needs {MIN_OPTIONS} to {MAX_OPTIONS}"
        ));
    }
    let options: Vec<String> = req.options.iter().map(|o| clean(o)).collect();
    for (i, option) in options.iter().enumerate() {
        check_len(&format!("option {i}"), option, MAX_OPTION_CHARS)?;
    }
    if req.assumed >= n {
        return Err(format!(
            "assumed is {}, but options are numbered 0 to {}",
            req.assumed,
            n - 1
        ));
    }

    if req.unblocks.len() > MAX_UNBLOCKS {
        return Err(format!(
            "{} unblocks labels given; the limit is {MAX_UNBLOCKS}",
            req.unblocks.len()
        ));
    }
    let unblocks: Vec<String> = req.unblocks.iter().map(|u| clean(u)).collect();
    for (i, label) in unblocks.iter().enumerate() {
        check_len(&format!("unblocks label {i}"), label, MAX_UNBLOCK_CHARS)?;
    }

    Ok(CleanPost {
        question,
        options,
        assumed: req.assumed,
        unblocks,
    })
}

impl DecisionBook {
    fn open_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|d| d.state == DecisionState::Open)
            .count()
    }

    fn open_count_for(&self, session_id: &str) -> usize {
        self.decisions
            .iter()
            .filter(|d| d.state == DecisionState::Open && d.session_id == session_id)
            .count()
    }

    /// Add a question from `session_id`. Refuses past either limit.
    pub(crate) fn post(
        &mut self,
        session_id: &str,
        session_label: &str,
        post: CleanPost,
        now: u64,
    ) -> Result<&Decision, String> {
        if self.open_count_for(session_id) >= MAX_OPEN_PER_SESSION {
            return Err(format!(
                "this session already has {MAX_OPEN_PER_SESSION} open questions; \
                 withdraw one or wait for an answer"
            ));
        }
        if self.open_count() >= MAX_OPEN_TOTAL {
            return Err(format!(
                "{MAX_OPEN_TOTAL} questions are already open across all sessions"
            ));
        }
        self.decisions.push(Decision {
            id: new_id(),
            session_id: session_id.to_string(),
            session_label: session_label.to_string(),
            question: post.question,
            options: post.options,
            assumed: post.assumed,
            unblocks: post.unblocks,
            posted_at: now,
            state: DecisionState::Open,
            answer: None,
            closed_at: None,
        });
        Ok(self.decisions.last().expect("just pushed"))
    }

    /// The caller's own questions, oldest first.
    pub(crate) fn for_session(&self, session_id: &str) -> Vec<&Decision> {
        let mut own: Vec<&Decision> = self
            .decisions
            .iter()
            .filter(|d| d.session_id == session_id)
            .collect();
        own.sort_by_key(|d| d.posted_at);
        own
    }

    /// Withdraw one of the caller's own open questions.
    pub(crate) fn withdraw(
        &mut self,
        session_id: &str,
        id: &str,
        now: u64,
    ) -> Result<&Decision, String> {
        let Some(d) = self
            .decisions
            .iter_mut()
            .find(|d| d.id == id && d.session_id == session_id)
        else {
            return Err(format!("this session has no question with id {id}"));
        };
        if d.state != DecisionState::Open {
            return Err(format!("question {id} is already {}", d.state.label()));
        }
        d.state = DecisionState::Withdrawn;
        d.closed_at = Some(now);
        Ok(d)
    }

    /// Record the developer's choice. Only an open question can be answered.
    pub(crate) fn answer(
        &mut self,
        id: &str,
        choice: usize,
        note: Option<String>,
        now: u64,
    ) -> Result<(), String> {
        let Some(d) = self.decisions.iter_mut().find(|d| d.id == id) else {
            return Err(format!("no question with id {id}"));
        };
        if d.state != DecisionState::Open {
            return Err(format!("question {id} is already {}", d.state.label()));
        }
        if choice >= d.options.len() {
            return Err(format!("question {id} has no option {choice}"));
        }
        let note = note
            .map(|n| clean(&n).chars().take(MAX_NOTE_CHARS).collect::<String>())
            .filter(|n| !n.is_empty());
        d.state = DecisionState::Answered;
        d.closed_at = Some(now);
        d.answer = Some(Answer {
            choice,
            note,
            answered_at: now,
            delivery: Delivery::Waiting,
        });
        Ok(())
    }

    /// Mark questions open for [`EXPIRE_AFTER_SECS`] as expired. True if any
    /// changed.
    pub(crate) fn expire(&mut self, now: u64) -> bool {
        let mut changed = false;
        for d in self.decisions.iter_mut() {
            if d.state == DecisionState::Open
                && now.saturating_sub(d.posted_at) >= EXPIRE_AFTER_SECS
            {
                d.state = DecisionState::Expired;
                d.closed_at = Some(now);
                changed = true;
            }
        }
        changed
    }

    /// Drop the oldest closed questions beyond [`MAX_HISTORY`]. Open ones are
    /// never dropped.
    pub(crate) fn prune(&mut self) {
        let closed = self
            .decisions
            .iter()
            .filter(|d| d.state != DecisionState::Open)
            .count();
        let mut excess = closed.saturating_sub(MAX_HISTORY);
        if excess == 0 {
            return;
        }
        // `decisions` is in posting order, so the first closed ones are oldest.
        self.decisions.retain(|d| {
            if excess > 0 && d.state != DecisionState::Open {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }

    /// Every open question, oldest first.
    pub(crate) fn open(&self) -> Vec<&Decision> {
        let mut open: Vec<&Decision> = self
            .decisions
            .iter()
            .filter(|d| d.state == DecisionState::Open)
            .collect();
        open.sort_by_key(|d| d.posted_at);
        open
    }

    /// Every closed question, most recently closed first.
    pub(crate) fn history(&self) -> Vec<&Decision> {
        let mut closed: Vec<&Decision> = self
            .decisions
            .iter()
            .filter(|d| d.state != DecisionState::Open)
            .collect();
        closed.sort_by_key(|d| std::cmp::Reverse(d.closed_at.unwrap_or(d.posted_at)));
        closed
    }

    /// Sessions with at least one answer waiting to be sent.
    pub(crate) fn sessions_waiting(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for d in &self.decisions {
            if matches!(&d.answer, Some(a) if a.delivery == Delivery::Waiting)
                && !ids.contains(&d.session_id)
            {
                ids.push(d.session_id.clone());
            }
        }
        ids
    }

    fn waiting_for<'a>(&'a self, session_id: &'a str) -> impl Iterator<Item = &'a Decision> {
        self.decisions.iter().filter(move |d| {
            d.session_id == session_id
                && matches!(&d.answer, Some(a) if a.delivery == Delivery::Waiting)
        })
    }

    /// The prompt carrying every waiting answer for `session_id`, in the order
    /// they were answered. One line: a newline would submit early on a terminal
    /// that is not in bracketed-paste mode.
    pub(crate) fn frame_waiting(&self, session_id: &str) -> Option<String> {
        let mut waiting: Vec<&Decision> = self.waiting_for(session_id).collect();
        if waiting.is_empty() {
            return None;
        }
        waiting.sort_by_key(|d| d.answer.as_ref().map(|a| a.answered_at));
        Some(
            waiting
                .iter()
                .map(|d| frame(d))
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// Set every waiting answer for `session_id` to `to`.
    pub(crate) fn settle(&mut self, session_id: &str, to: Delivery) {
        for d in self.decisions.iter_mut() {
            if d.session_id != session_id {
                continue;
            }
            if let Some(a) = d.answer.as_mut() {
                if a.delivery == Delivery::Waiting {
                    a.delivery = to;
                }
            }
        }
    }
}

/// `[Your decision via Allele] Q: … → A: …`, with the note if there is one.
pub(crate) fn frame(d: &Decision) -> String {
    let Some(answer) = &d.answer else {
        return String::new();
    };
    let chosen = d
        .options
        .get(answer.choice)
        .map(String::as_str)
        .unwrap_or("");
    let mut line = format!("{FRAME} Q: {} → A: {chosen}", d.question);
    if answer.choice != d.assumed {
        if let Some(assumed) = d.options.get(d.assumed) {
            line.push_str(&format!(" (you had assumed: {assumed})"));
        }
    }
    if let Some(note) = &answer.note {
        line.push_str(&format!(" — note: {note}"));
    }
    line
}

fn new_id() -> String {
    // Short enough to read in a list, long enough not to collide in 50.
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "just now", "5m ago", … — the formatter the conversation picker uses.
pub(crate) fn ago(then: u64) -> String {
    crate::conversations::relative_time(UNIX_EPOCH + Duration::from_secs(then))
}

// ── When an answer may be sent ──────────────────────────────────────────

/// What to do with a session's waiting answers right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// Idle at its prompt: send.
    Deliver,
    /// Mid-turn, blocked on a prompt, suspended, or not yet idle again since
    /// the last send: keep holding.
    Hold,
    /// No such session: record that and send nothing.
    Gone,
}

/// What Allele can see of a session, for [`target`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionView {
    pub status: SessionStatus,
    /// The session has a live agent process, and Allele has had a hook event
    /// from that process. Before the first one, `Idle` is only the initial
    /// value and the agent may be on a startup dialog where typing picks an
    /// option — the same guard Away uses (`away::decide`).
    pub heard: bool,
    /// The kind of attention the session is in, when it is `AwaitingInput`.
    pub attention: Option<crate::session::AttentionKind>,
    pub status_since: SystemTime,
}

/// The delivery rule. **Never types over a running turn**: only a session
/// that is idle at its prompt gets an answer. That is `ResponseReady` or
/// `Idle`, or `AwaitingInput` from Claude's sixty-second idle nudge — which is
/// still the prompt, and which every session left idle for a minute ends up
/// in. A permission prompt or a question is held: typing there answers it.
///
/// `last_sent` closes the gap after a send: the status still reads idle until
/// the agent's hook reports the turn it just started. A second sweep in that
/// gap must not type again, so after a send the session is held until its
/// status has changed since.
pub(crate) fn target(session: Option<SessionView>, last_sent: Option<SystemTime>) -> Target {
    let Some(s) = session else {
        return Target::Gone;
    };
    let idle = match s.status {
        SessionStatus::ResponseReady | SessionStatus::Idle => true,
        SessionStatus::AwaitingInput => s.attention == Some(crate::session::AttentionKind::Idle),
        SessionStatus::Running | SessionStatus::Suspended | SessionStatus::Done => false,
    };
    let settled = last_sent.is_none_or(|sent| s.status_since > sent);
    if idle && s.heard && settled {
        Target::Deliver
    } else {
        Target::Hold
    }
}

// ── Wire types (control socket) ─────────────────────────────────────────

/// Parameters for `decisions_post`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostRequest {
    pub question: String,
    pub options: Vec<String>,
    pub assumed: usize,
    #[serde(default)]
    pub unblocks: Vec<String>,
    /// Who is asking, from `CLAUDE_CODE_SESSION_ID` in the MCP server's own
    /// environment — written after the caller's arguments, so a caller cannot
    /// post as another session. Stripped for paired machines.
    #[serde(default)]
    pub caller_session_id: Option<String>,
}

/// What the decision ops return for one question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionSummary {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub assumed: usize,
    pub unblocks: Vec<String>,
    /// `open`, `answered`, `withdrawn` or `expired`.
    pub state: DecisionState,
    /// Unix seconds.
    pub posted_at: u64,
    /// `null` until the developer answers.
    pub answer: Option<AnswerSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerSummary {
    pub choice: usize,
    /// The chosen option's text.
    pub option: String,
    pub note: Option<String>,
    pub answered_at: u64,
    /// `waiting`, `sent` or `session_gone`.
    pub delivery: String,
}

pub(crate) fn summarise(d: &Decision) -> DecisionSummary {
    DecisionSummary {
        id: d.id.clone(),
        question: d.question.clone(),
        options: d.options.clone(),
        assumed: d.assumed,
        unblocks: d.unblocks.clone(),
        state: d.state,
        posted_at: d.posted_at,
        answer: d.answer.as_ref().map(|a| AnswerSummary {
            choice: a.choice,
            option: d.options.get(a.choice).cloned().unwrap_or_default(),
            note: a.note.clone(),
            answered_at: a.answered_at,
            delivery: match a.delivery {
                Delivery::Waiting => "waiting",
                Delivery::Sent { .. } => "sent",
                Delivery::SessionGone => "session_gone",
            }
            .to_string(),
        }),
    }
}

// ── AppState glue ───────────────────────────────────────────────────────

/// The decision list's state on `AppState`.
#[derive(Default)]
pub(crate) struct Decisions {
    pub book: DecisionBook,
    /// Set by [`AppState::mark_decisions_dirty`]; drained by
    /// `checkpoint_persistence` like `state_dirty`.
    pub dirty: bool,
    pub gate: crate::app_state::PersistGate,
    /// Rows hidden; the header with the count stays.
    pub collapsed: bool,
    /// Show answered, withdrawn and expired questions.
    pub show_history: bool,
    /// The optional note that goes with the next answer clicked.
    pub note: Option<Entity<crate::text_input::TextInput>>,
    /// `decisions.json` exists but could not be read at launch. Nothing is
    /// written over it, so a passing read error cannot cost the history.
    pub save_blocked: bool,
    /// When an answer was last typed into each session. In memory only: after
    /// a restart every session is past its last send.
    pub last_sent: HashMap<String, SystemTime>,
}

impl Decisions {
    /// Load `decisions.json` at launch.
    pub(crate) fn load(repo: &dyn crate::repositories::DecisionsRepository) -> Self {
        match repo.load() {
            Ok(book) => Self {
                book,
                ..Self::default()
            },
            Err(e) => {
                // A corrupt file has already been moved aside, so writing is
                // safe; any other failure leaves the file where it was, and it
                // is not overwritten this run.
                let save_blocked = repo.exists();
                tracing::warn!(
                    "decisions: starting with an empty list{}: {e}",
                    if save_blocked {
                        "; decisions.json is left untouched until the next launch"
                    } else {
                        ""
                    }
                );
                Self {
                    save_blocked,
                    ..Self::default()
                }
            }
        }
    }
}

fn bad_request(message: String) -> Response {
    Response::Error {
        code: ErrorCode::BadRequest,
        message,
    }
}

/// The allele session a caller id names, by allele id or Claude session id.
fn resolve_caller(state: &AppState, caller: Option<&str>) -> Result<(String, String), Response> {
    let found = caller.filter(|c| !c.is_empty()).and_then(|caller| {
        state
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter())
            .find(|s| s.id == caller || s.claude_session_id() == caller)
    });
    match found {
        Some(s) => Ok((s.id.clone(), s.label.clone())),
        None => Err(bad_request(
            "decisions can only be posted and read from a session Allele is running \
             (no calling session was found)"
                .to_string(),
        )),
    }
}

impl AppState {
    /// Flag `decisions.json` for the next `checkpoint_persistence()`.
    pub(crate) fn mark_decisions_dirty(&mut self) {
        self.decisions.dirty = true;
    }

    /// `decisions_post`: record the question and return at once.
    pub(crate) fn decisions_post(&mut self, req: PostRequest, cx: &mut Context<Self>) -> Response {
        let (session_id, label) = match resolve_caller(self, req.caller_session_id.as_deref()) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let post = match validate(&req) {
            Ok(p) => p,
            Err(reason) => return bad_request(reason),
        };
        let summary = match self
            .decisions
            .book
            .post(&session_id, &label, post, now_secs())
        {
            Ok(d) => Box::new(summarise(d)),
            Err(reason) => return bad_request(reason),
        };
        self.mark_decisions_dirty();
        cx.notify();
        Response::DecisionPosted { decision: summary }
    }

    /// `decisions_list`: the caller's own questions.
    pub(crate) fn decisions_list(&self, caller: Option<&str>) -> Response {
        let (session_id, _) = match resolve_caller(self, caller) {
            Ok(v) => v,
            Err(r) => return r,
        };
        Response::Decisions {
            decisions: self
                .decisions
                .book
                .for_session(&session_id)
                .into_iter()
                .map(summarise)
                .collect(),
        }
    }

    /// `decisions_withdraw`: close one of the caller's own open questions.
    pub(crate) fn decisions_withdraw(
        &mut self,
        id: &str,
        caller: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Response {
        let (session_id, _) = match resolve_caller(self, caller) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let summary = match self.decisions.book.withdraw(&session_id, id, now_secs()) {
            Ok(d) => Box::new(summarise(d)),
            Err(reason) => return bad_request(reason),
        };
        self.mark_decisions_dirty();
        cx.notify();
        Response::DecisionWithdrawn { decision: summary }
    }

    /// The developer clicked an option. Takes the note field's text with it,
    /// then tries to deliver straight away.
    pub(crate) fn answer_decision(&mut self, id: &str, choice: usize, cx: &mut Context<Self>) {
        let note = self
            .decisions
            .note
            .as_ref()
            .map(|input| input.read(cx).text().to_string());
        match self.decisions.book.answer(id, choice, note, now_secs()) {
            Ok(()) => {
                // Cleared only once the answer is taken, so a refused click
                // (the question expired a moment ago) keeps what was typed.
                if let Some(input) = self.decisions.note.clone() {
                    input.update(cx, |input, cx| input.set_text_silent("", cx));
                }
                self.mark_decisions_dirty();
                self.sweep_decisions(cx);
            }
            Err(e) => tracing::warn!("decisions: {e}"),
        }
        cx.notify();
    }

    /// Expire stale questions and send waiting answers to sessions that are
    /// idle at their prompt. Runs every [`SWEEP_EVERY`] and after each answer.
    pub(crate) fn sweep_decisions(&mut self, cx: &mut Context<Self>) {
        let now = now_secs();
        let mut changed = self.decisions.book.expire(now);

        for session_id in self.decisions.book.sessions_waiting() {
            let session = self
                .projects
                .iter()
                .flat_map(|p| p.sessions.iter())
                .find(|s| s.id == session_id);
            let view = session.map(|s| {
                let pid = crate::away::live_pid(s, cx);
                SessionView {
                    status: s.status,
                    heard: pid.is_some() && s.remote_control.heard_from_pid == pid,
                    attention: s.attention_context.as_ref().map(|c| c.kind),
                    status_since: s.status_since,
                }
            });
            match target(view, self.decisions.last_sent.get(&session_id).copied()) {
                Target::Hold => {}
                Target::Gone => {
                    self.decisions
                        .book
                        .settle(&session_id, Delivery::SessionGone);
                    changed = true;
                }
                Target::Deliver => {
                    let (Some(tv), Some(text)) = (
                        session.and_then(|s| s.terminal_view.clone()),
                        self.decisions.book.frame_waiting(&session_id),
                    ) else {
                        continue;
                    };
                    crate::dispatch::pty::deliver(&tv, text, &[], cx);
                    self.decisions
                        .last_sent
                        .insert(session_id.clone(), SystemTime::now());
                    self.decisions
                        .book
                        .settle(&session_id, Delivery::Sent { at: now });
                    changed = true;
                }
            }
        }

        if changed {
            self.decisions.book.prune();
            self.mark_decisions_dirty();
            cx.notify();
        }
    }

    /// Write `decisions.json` off the foreground, on the same debounce as
    /// `state.json`.
    pub(crate) fn flush_decisions(&mut self, cx: &mut Context<Self>) {
        if self.decisions.save_blocked {
            self.decisions.dirty = false;
            return;
        }
        if self.decisions.gate.in_flight {
            return;
        }
        if !crate::app_state::persist_is_due(
            self.decisions.gate.written_at,
            std::time::Instant::now(),
        ) {
            self.schedule_persist_flush(cx);
            return;
        }
        self.decisions.dirty = false;
        self.decisions.gate.written_at = Some(std::time::Instant::now());
        self.decisions.gate.in_flight = true;

        let snapshot = self.decisions.book.clone();
        let repo = self.repos.decisions.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = repo.save(&snapshot) {
                        tracing::warn!("Failed to save decisions.json: {e}");
                    }
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.decisions.gate.in_flight = false;
                if this.decisions.dirty {
                    this.schedule_persist_flush(cx);
                }
            });
        })
        .detach();
    }
}

/// Run [`AppState::sweep_decisions`] for the life of the app.
pub(crate) fn spawn_sweeper(cx: &mut Context<AppState>) {
    cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(SWEEP_EVERY).await;
        if this
            .update(cx, |state, cx| state.sweep_decisions(cx))
            .is_err()
        {
            return;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::AttentionKind;

    fn req(question: &str, options: &[&str], assumed: usize) -> PostRequest {
        PostRequest {
            question: question.into(),
            options: options.iter().map(|s| s.to_string()).collect(),
            assumed,
            unblocks: vec![],
            caller_session_id: Some("s1".into()),
        }
    }

    fn post(book: &mut DecisionBook, session: &str, now: u64) -> Result<String, String> {
        let clean = validate(&req("Which one?", &["A", "B"], 0))?;
        book.post(session, "Session", clean, now)
            .map(|d| d.id.clone())
    }

    #[test]
    fn one_option_is_refused_with_a_reason() {
        let err = validate(&req("Q?", &["only"], 0)).unwrap_err();
        assert!(err.contains("1 options given"), "{err}");
    }

    #[test]
    fn five_options_are_refused_with_a_reason() {
        let err = validate(&req("Q?", &["a", "b", "c", "d", "e"], 0)).unwrap_err();
        assert!(err.contains("5 options given"), "{err}");
    }

    #[test]
    fn a_281_character_question_is_refused_with_a_reason() {
        let q = "x".repeat(281);
        let err = validate(&req(&q, &["a", "b"], 0)).unwrap_err();
        assert!(err.contains("281 characters"), "{err}");
        assert!(validate(&req(&"x".repeat(280), &["a", "b"], 0)).is_ok());
    }

    #[test]
    fn limits_count_characters_not_bytes() {
        // 280 three-byte characters is within the limit.
        assert!(validate(&req(&"é".repeat(280), &["a", "b"], 0)).is_ok());
    }

    #[test]
    fn long_options_and_bad_assumed_are_refused() {
        assert!(validate(&req("Q?", &[&"o".repeat(81), "b"], 0)).is_err());
        assert!(validate(&req("Q?", &["a", "b"], 2))
            .unwrap_err()
            .contains("assumed"));
        assert!(validate(&req("Q?", &["a", "  "], 0))
            .unwrap_err()
            .contains("empty"));
    }

    #[test]
    fn unblocks_are_limited() {
        let mut r = req("Q?", &["a", "b"], 0);
        r.unblocks = (0..6).map(|i| format!("u{i}")).collect();
        assert!(validate(&r).is_err());
        r.unblocks = vec!["u".repeat(61)];
        assert!(validate(&r).is_err());
        r.unblocks = vec!["merge PR".into()];
        assert!(validate(&r).is_ok());
    }

    /// An answer is typed into a terminal. ESC and newlines in a question must
    /// never reach it.
    #[test]
    fn control_characters_become_spaces() {
        let r = req("Ship\x1b[201~ it?\nreally", &["yes\r", "no"], 0);
        let clean = validate(&r).unwrap();
        assert_eq!(clean.question, "Ship [201~ it? really");
        assert_eq!(clean.options[0], "yes");
        assert!(!clean.question.chars().any(char::is_control));
    }

    #[test]
    fn a_sixth_open_question_from_one_session_is_refused() {
        let mut book = DecisionBook::default();
        for i in 0..5 {
            post(&mut book, "s1", i).unwrap();
        }
        let err = post(&mut book, "s1", 6).unwrap_err();
        assert!(err.contains("5 open questions"), "{err}");
        // Another session is unaffected.
        assert!(post(&mut book, "s2", 6).is_ok());
    }

    #[test]
    fn the_fifty_first_open_question_overall_is_refused() {
        let mut book = DecisionBook::default();
        for i in 0..50 {
            post(&mut book, &format!("s{}", i / 5), i).unwrap();
        }
        assert!(post(&mut book, "fresh", 99)
            .unwrap_err()
            .contains("50 questions"));
    }

    #[test]
    fn a_withdrawn_question_frees_its_slot() {
        let mut book = DecisionBook::default();
        let ids: Vec<String> = (0..5).map(|i| post(&mut book, "s1", i).unwrap()).collect();
        book.withdraw("s1", &ids[0], 10).unwrap();
        assert!(post(&mut book, "s1", 11).is_ok());
    }

    #[test]
    fn a_session_cannot_withdraw_another_sessions_question() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        assert!(book.withdraw("s2", &id, 1).is_err());
        assert_eq!(book.open().len(), 1);
    }

    #[test]
    fn open_questions_list_oldest_first_across_sessions() {
        let mut book = DecisionBook::default();
        post(&mut book, "s2", 20).unwrap();
        post(&mut book, "s1", 10).unwrap();
        let open = book.open();
        assert_eq!(open[0].session_id, "s1");
        assert_eq!(open[1].session_id, "s2");
    }

    #[test]
    fn list_returns_the_chosen_option_after_an_answer() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        book.answer(&id, 1, Some("because".into()), 5).unwrap();
        let mine = book.for_session("s1");
        let summary = summarise(mine[0]);
        assert_eq!(summary.state, DecisionState::Answered);
        let answer = summary.answer.unwrap();
        assert_eq!(answer.option, "B");
        assert_eq!(answer.note.as_deref(), Some("because"));
        assert_eq!(answer.delivery, "waiting");
    }

    #[test]
    fn an_answered_question_cannot_be_answered_again() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        book.answer(&id, 0, None, 1).unwrap();
        assert!(book.answer(&id, 1, None, 2).is_err());
    }

    #[test]
    fn an_unanswered_question_expires_after_24_hours() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 1_000).unwrap();
        assert!(!book.expire(1_000 + EXPIRE_AFTER_SECS - 1));
        assert!(book.expire(1_000 + EXPIRE_AFTER_SECS));
        let d = book.decisions.iter().find(|d| d.id == id).unwrap();
        assert_eq!(d.state, DecisionState::Expired);
        assert!(book.answer(&id, 0, None, 0).is_err());
        assert_eq!(book.history().len(), 1);
    }

    #[test]
    fn the_frame_names_question_answer_and_note() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        book.answer(&id, 1, Some("ship it".into()), 1).unwrap();
        let text = book.frame_waiting("s1").unwrap();
        assert_eq!(
            text,
            "[Your decision via Allele] Q: Which one? → A: B (you had assumed: A) — note: ship it"
        );
        assert!(!text.contains('\n'));
    }

    #[test]
    fn several_waiting_answers_go_in_one_prompt() {
        let mut book = DecisionBook::default();
        let a = post(&mut book, "s1", 0).unwrap();
        let b = post(&mut book, "s1", 1).unwrap();
        book.answer(&b, 0, None, 5).unwrap();
        book.answer(&a, 0, None, 6).unwrap();
        let text = book.frame_waiting("s1").unwrap();
        assert_eq!(text.matches(FRAME).count(), 2);
        book.settle("s1", Delivery::Sent { at: 7 });
        assert!(book.frame_waiting("s1").is_none());
        assert!(book.sessions_waiting().is_empty());
    }

    fn view(status: SessionStatus, since: SystemTime) -> Option<SessionView> {
        Some(SessionView {
            status,
            heard: true,
            attention: None,
            status_since: since,
        })
    }

    #[test]
    fn an_idle_session_gets_its_answer() {
        let t = SystemTime::now();
        assert_eq!(
            target(view(SessionStatus::ResponseReady, t), None),
            Target::Deliver
        );
        assert_eq!(target(view(SessionStatus::Idle, t), None), Target::Deliver);
    }

    /// The owner's rule: never type an answer over a running turn.
    #[test]
    fn a_busy_session_is_held() {
        let t = SystemTime::now();
        for status in [
            SessionStatus::Running,
            SessionStatus::AwaitingInput,
            SessionStatus::Suspended,
            SessionStatus::Done,
        ] {
            assert_eq!(target(view(status, t), None), Target::Hold, "{status:?}");
        }
        // Not yet heard from: may be on a startup dialog.
        let unheard = Some(SessionView {
            heard: false,
            ..view(SessionStatus::ResponseReady, t).unwrap()
        });
        assert_eq!(target(unheard, None), Target::Hold);
        // A permission prompt or a question is not the prompt.
        for kind in [AttentionKind::Permission, AttentionKind::Question] {
            let blocked = Some(SessionView {
                attention: Some(kind),
                ..view(SessionStatus::AwaitingInput, t).unwrap()
            });
            assert_eq!(target(blocked, None), Target::Hold, "{kind:?}");
        }
    }

    /// A session left at its prompt for a minute gets Claude's idle nudge and
    /// reads `AwaitingInput`. It is still at its prompt, and must still get
    /// its answer — otherwise every answer to a quiet session waits forever.
    #[test]
    fn the_idle_nudge_still_counts_as_idle() {
        let nudged = Some(SessionView {
            attention: Some(AttentionKind::Idle),
            ..view(SessionStatus::AwaitingInput, SystemTime::now()).unwrap()
        });
        assert_eq!(target(nudged, None), Target::Deliver);
    }

    #[test]
    fn a_gone_session_is_gone() {
        assert_eq!(target(None, None), Target::Gone);
    }

    /// After a send the status still reads idle until the agent reports the
    /// turn it started. The next sweep must not type again in that gap.
    #[test]
    fn nothing_more_is_sent_until_the_session_has_moved_on() {
        let idle_since = SystemTime::now() - Duration::from_secs(30);
        let sent = SystemTime::now() - Duration::from_secs(1);
        assert_eq!(
            target(view(SessionStatus::ResponseReady, idle_since), Some(sent)),
            Target::Hold
        );
        // It ran a turn and is idle again.
        let idle_again = SystemTime::now();
        assert_eq!(
            target(view(SessionStatus::ResponseReady, idle_again), Some(sent)),
            Target::Deliver
        );
    }

    /// Mid-turn when answered: held, then sent exactly once when idle.
    #[test]
    fn a_mid_turn_session_gets_the_answer_once_it_goes_idle() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        book.answer(&id, 0, None, 1).unwrap();
        let t = SystemTime::now();

        assert_eq!(target(view(SessionStatus::Running, t), None), Target::Hold);
        assert_eq!(book.sessions_waiting(), vec!["s1".to_string()]);

        assert_eq!(
            target(view(SessionStatus::ResponseReady, t), None),
            Target::Deliver
        );
        book.settle("s1", Delivery::Sent { at: 2 });
        assert!(book.sessions_waiting().is_empty(), "sent once, not again");
    }

    #[test]
    fn a_gone_sessions_answer_is_kept_and_not_sent() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        book.answer(&id, 1, None, 1).unwrap();
        book.settle("s1", Delivery::SessionGone);
        let d = &book.history()[0];
        assert_eq!(d.answer.as_ref().unwrap().choice, 1);
        assert_eq!(d.answer.as_ref().unwrap().delivery, Delivery::SessionGone);
        assert!(book.sessions_waiting().is_empty());
    }

    #[test]
    fn history_is_capped_and_never_drops_open_questions() {
        let mut book = DecisionBook::default();
        for i in 0..(MAX_HISTORY as u64 + 3) {
            let id = post(&mut book, "s", i).unwrap();
            book.withdraw("s", &id, i).unwrap();
        }
        post(&mut book, "s", 9_999).unwrap();
        book.prune();
        assert_eq!(book.history().len(), MAX_HISTORY);
        assert_eq!(book.open().len(), 1);
    }

    #[test]
    fn the_book_round_trips_through_json() {
        let mut book = DecisionBook::default();
        let id = post(&mut book, "s1", 0).unwrap();
        book.answer(&id, 0, None, 1).unwrap();
        book.settle("s1", Delivery::Sent { at: 2 });
        post(&mut book, "s2", 3).unwrap();
        let json = serde_json::to_string(&book).unwrap();
        assert_eq!(serde_json::from_str::<DecisionBook>(&json).unwrap(), book);
    }

    // ── Through AppState and the control-socket handler ────────────────

    fn fixture(cx: &mut gpui::TestAppContext) -> crate::app_state::fixture::Fixture {
        let mut project =
            crate::project::Project::new("alpha".into(), std::path::PathBuf::from("/tmp/alpha"));
        let now = SystemTime::now();
        for id in ["s-1", "s-2"] {
            project
                .sessions
                .push(crate::session::Session::suspended_from_persisted(
                    id.into(),
                    id.into(),
                    now,
                    now,
                    Duration::ZERO,
                    None,
                ));
        }
        crate::app_state::fixture::Fixture::with_projects(vec![project], cx)
    }

    fn call(
        fx: &crate::app_state::fixture::Fixture,
        request: crate::dispatch::protocol::Request,
        cx: &mut gpui::TestAppContext,
    ) -> Response {
        fx.update(cx, |state, window, cx| {
            crate::dispatch::handler::handle(request, state, window, cx)
        })
    }

    fn post_as(caller: Option<&str>, question: &str) -> crate::dispatch::protocol::Request {
        let mut r = req(question, &["A", "B"], 0);
        r.caller_session_id = caller.map(str::to_string);
        crate::dispatch::protocol::Request::DecisionsPost(r)
    }

    #[gpui::test]
    fn posting_returns_an_id_and_only_a_known_session_may_post(cx: &mut gpui::TestAppContext) {
        let fx = fixture(cx);
        let Response::DecisionPosted { decision } = call(&fx, post_as(Some("s-1"), "Q1?"), cx)
        else {
            panic!("expected a posted decision");
        };
        assert!(!decision.id.is_empty());
        assert_eq!(decision.state, DecisionState::Open);

        for caller in [None, Some("nobody")] {
            match call(&fx, post_as(caller, "Q?"), cx) {
                Response::Error { code, .. } => assert_eq!(code, ErrorCode::BadRequest),
                other => panic!("{caller:?} posted: {other:?}"),
            }
        }
        let open = fx.update(cx, |state, _w, _cx| state.decisions.book.open().len());
        assert_eq!(open, 1);
    }

    #[gpui::test]
    fn the_list_is_per_session_and_shows_the_answer(cx: &mut gpui::TestAppContext) {
        let fx = fixture(cx);
        call(&fx, post_as(Some("s-1"), "Mine?"), cx);
        call(&fx, post_as(Some("s-2"), "Theirs?"), cx);
        let id = fx.update(cx, |state, _w, _cx| {
            state.decisions.book.open()[0].id.clone()
        });
        fx.update(cx, |state, _w, cx| state.answer_decision(&id, 1, cx));

        let list = crate::dispatch::protocol::Request::DecisionsList {
            caller_session_id: Some("s-1".into()),
        };
        let Response::Decisions { decisions } = call(&fx, list, cx) else {
            panic!("expected a list");
        };
        assert_eq!(decisions.len(), 1, "only the caller's own questions");
        assert_eq!(decisions[0].question, "Mine?");
        let answer = decisions[0].answer.as_ref().expect("answered");
        assert_eq!(answer.option, "B");
        // s-1 is suspended: no terminal, so the answer is held, not sent.
        assert_eq!(answer.delivery, "waiting");
    }

    #[gpui::test]
    fn withdraw_is_limited_to_the_callers_own(cx: &mut gpui::TestAppContext) {
        let fx = fixture(cx);
        call(&fx, post_as(Some("s-1"), "Q?"), cx);
        let id = fx.update(cx, |state, _w, _cx| {
            state.decisions.book.open()[0].id.clone()
        });
        let as_other = crate::dispatch::protocol::Request::DecisionsWithdraw {
            id: id.clone(),
            caller_session_id: Some("s-2".into()),
        };
        assert!(matches!(call(&fx, as_other, cx), Response::Error { .. }));
        let as_owner = crate::dispatch::protocol::Request::DecisionsWithdraw {
            id,
            caller_session_id: Some("s-1".into()),
        };
        assert!(matches!(
            call(&fx, as_owner, cx),
            Response::DecisionWithdrawn { .. }
        ));
    }

    #[gpui::test]
    fn an_answer_for_a_discarded_session_is_recorded_and_not_sent(cx: &mut gpui::TestAppContext) {
        let fx = fixture(cx);
        call(&fx, post_as(Some("s-2"), "Q?"), cx);
        fx.update(cx, |state, _w, _cx| {
            state.projects[0].sessions.retain(|s| s.id != "s-2");
        });
        let id = fx.update(cx, |state, _w, _cx| {
            state.decisions.book.open()[0].id.clone()
        });
        fx.update(cx, |state, _w, cx| state.answer_decision(&id, 0, cx));
        let d = fx.update(cx, |state, _w, _cx| {
            state.decisions.book.history()[0].clone()
        });
        let answer = d.answer.expect("recorded");
        assert_eq!(answer.choice, 0);
        assert_eq!(answer.delivery, Delivery::SessionGone);
    }

    /// Open and answered questions survive quitting and reopening: the quit
    /// flush writes them, and a fresh load reads them back.
    #[gpui::test]
    fn decisions_survive_a_restart(cx: &mut gpui::TestAppContext) {
        let fx = fixture(cx);
        call(&fx, post_as(Some("s-1"), "Open one?"), cx);
        call(&fx, post_as(Some("s-1"), "Answered one?"), cx);
        let id = fx.update(cx, |state, _w, _cx| {
            state.decisions.book.open()[1].id.clone()
        });
        fx.update(cx, |state, _w, cx| state.answer_decision(&id, 1, cx));

        let (before, repo) = fx.update(cx, |state, _w, _cx| {
            assert!(state.decisions.dirty, "posting and answering mark dirty");
            state.flush_persistence_blocking();
            (state.decisions.book.clone(), state.repos.decisions.clone())
        });
        let reopened = Decisions::load(&*repo);
        assert_eq!(reopened.book, before);
        assert_eq!(reopened.book.open().len(), 1);
        assert_eq!(reopened.book.history().len(), 1);
    }
}
