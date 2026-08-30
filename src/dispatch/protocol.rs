//! Wire types for the allele control socket (DEV-415).
//!
//! One request per line, one response per line, both newline-delimited JSON
//! over a Unix socket. Line framing rather than a length prefix because every
//! payload here is small and human-readable, and a wire a person can `nc` at
//! is worth more during development than a few saved bytes.
//!
//! These types are deliberately **separate from allele's internal types**.
//! `SessionState` mirrors [`crate::session::SessionStatus`] today, but the
//! internal enum is free to gain variants or be refactored, and an MCP client
//! on the other side of this socket is not free to keep up. The conversion is
//! one function, and it is the only place the two vocabularies meet.

// Consumers land with the socket listener in the next commit; these types are
// the contract they are written against.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::session::{Orchestration, SessionStatus};

// ── Requests ────────────────────────────────────────────────────────────

/// A call from an MCP client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Registered projects a session can be created in.
    ProjectsList,
    /// Every session this allele knows about, dispatched or not.
    SessionsList,
    /// One session, by its stable allele id.
    SessionsStatus { session_id: String },
    /// Provision a workspace and start a session in it.
    SessionsCreate(CreateRequest),
    /// Remove a dispatched session, archiving its work.
    SessionsDiscard { session_id: String },
    /// Stop whatever a dispatched session is currently doing.
    SessionsInterrupt { session_id: String },
}

/// Parameters for `sessions.create`.
///
/// Deliberately four fields. Everything an orchestrator wants to say to the
/// session it is starting belongs in `prompt`; a parameter added here is one
/// allele has to maintain and version forever. Notably absent:
///
/// - **`permission_mode`** — descoped, see DEV-413. Dispatched sessions
///   inherit `permissions.defaultMode` from the operator's `settings.json`.
/// - **`depth`** — derived by allele from the creating session, never
///   supplied. Anything a dispatched session can assert about its own depth
///   is something it can be wrong about, and the sessions doing the asserting
///   are the ones running the rule that causes the recursion.
/// - **any settings or allowlist override** — see DEV-413's note on why a
///   scalar permission comparison is only sound while the allowlist is held
///   constant across creator and child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateRequest {
    /// Project name or id, as returned by `projects.list`.
    pub project: String,
    /// Requested session name. Allele may return a *different* name — see
    /// [`CreatedSession::name`].
    pub name: String,
    /// Sent to the agent once the session is up. Keep it short and point at
    /// an artifact: one atomic paste has nothing to interleave, and the
    /// failure mode of a truncated brief is a session starting confidently
    /// on half a specification.
    pub prompt: String,
    /// How much of the project's setup to run. Defaults to `Nothing` for
    /// dispatched sessions — a worker clones a workspace, does one job and is
    /// discarded, and most of them never touch the project's services: they
    /// run the test binary directly, or against SQLite. Provisioning a
    /// database, a server, a queue worker and a bundler for that is overhead
    /// paid on every dispatch to be used by few.
    ///
    /// A dispatcher whose work genuinely needs them asks for `StartupOnly`
    /// (or `Full`) explicitly, so the cost lands on the dispatch that wanted
    /// it rather than on all of them.
    #[serde(default = "default_dispatch_orchestration")]
    pub orchestration: Orchestration,
    /// The calling session's Claude session id, taken from
    /// `CLAUDE_CODE_SESSION_ID` in the MCP server's own environment.
    ///
    /// An identity **claim**, resolved by allele against its own records. The
    /// depth it implies is read from allele's record of that session, never
    /// from the caller — so naming a session cannot grant a depth that
    /// session does not have.
    ///
    /// The claim itself is unverified: a caller can omit or alter it, and an
    /// unknown id is treated as depth 0. That bounds the *accident* case —
    /// a session recursing because its own rules told it to — and nothing
    /// more. It is not a security control; see DEV-419 for the enforceable
    /// half, which cannot live here because allele cannot see processes it
    /// did not spawn.
    #[serde(default)]
    pub caller_session_id: Option<String>,
    /// The calling session's messaging address, taken from
    /// `CLAUDE_CODE_MESSAGING_SOCKET` in the MCP server's own environment and
    /// prefixed `uds:` (DEV-440).
    ///
    /// Prepended to the dispatched session's prompt so it can reply without
    /// the dispatcher having to state an address — which it cannot do
    /// correctly, because `ListAgents` does not list self and the name in its
    /// sidebar is not the name its process answers to.
    ///
    /// Absent means "the caller has no reachable address". That is reported to
    /// the worker explicitly rather than dropped; see
    /// [`crate::dispatch::address::dispatch_preamble`].
    #[serde(default)]
    pub caller_reply_to: Option<String>,
}

/// Dispatched sessions start bare unless they ask for more (DEV-441).
///
/// Deliberately *not* [`Orchestration::default()`], which is `Full`: that is
/// the right answer for a human opening the New Session dialog in a project
/// they are about to work in, and the wrong one for an agent that wants a
/// workspace, a branch, and nothing else running behind it.
fn default_dispatch_orchestration() -> Orchestration {
    Orchestration::Nothing
}

// ── Responses ───────────────────────────────────────────────────────────

/// The reply to a [`Request`]. Exactly one is written per request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Projects { projects: Vec<ProjectSummary> },
    Sessions { sessions: Vec<SessionSummary> },
    Status { session: SessionSummary },
    Created { session: CreatedSession },
    Discarded { session: DiscardedSession },
    Interrupted { session: InterruptedSession },
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub source_path: String,
    /// Live sessions in this project, dispatched or not.
    pub session_count: usize,
}

/// What `sessions.create` returns.
///
/// **`name` is not an address.** `SendMessage` resolves names against the
/// Claude Code *process* name, which is fixed at spawn, while this one is
/// allele's mutable label — auto-naming rewrites it seconds after the first
/// prompt lands. Resolving it via `ListAgents` may find nothing, or may find
/// a different session carrying the same string.
///
/// `reply_to` is an address, and is the one to use. See DEV-440.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreatedSession {
    /// Allele's stable session id — the **only durable identity** here.
    /// Names are stable in practice; refs are not stable at all. Key on this.
    pub session_id: String,
    /// The name allele actually minted, which may differ from the requested
    /// one. Uniqueness is enforced across *every session allele has ever
    /// created*, not just live ones — a dead session is absent from
    /// `ListAgents` and its name is exactly the one that would collide,
    /// silently retargeting a caller holding an amortised bare name.
    pub name: String,
    pub project: String,
    pub state: SessionState,
    /// The new session's own messaging address, as `uds:<path>` (DEV-440).
    ///
    /// Derived from the agent process, so it survives every rename and can
    /// never collide with another session's. Use it to send follow-ups
    /// instead of resolving `name`.
    ///
    /// `None` means allele could not find a bound socket for the process —
    /// reported honestly rather than guessed, because a plausible-looking
    /// wrong address is worse than an absent one.
    pub reply_to: Option<String>,
}

/// What `sessions.discard` returns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscardedSession {
    pub session_id: String,
    pub name: String,
    pub project: String,
    /// The state the session was in when it was discarded.
    ///
    /// Reported so a caller that killed something mid-turn can notice.
    /// Discarding a `running` session is allowed — stopping a runaway is a
    /// legitimate reason to reach for this — so the response says what was
    /// stopped rather than the request refusing to stop it.
    pub was_state: SessionState,
}

/// What `sessions.interrupt` returns.
///
/// Reports the state on both sides of the interrupt rather than asserting
/// success, so a caller can see whether anything actually stopped. An
/// interrupt sent to a session that was not running is a no-op, and this
/// makes that visible instead of implying otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InterruptedSession {
    pub session_id: String,
    pub name: String,
    pub was_state: SessionState,
    pub now_state: SessionState,
}

/// A session's current state.
///
/// **`ListAgents` cannot express this.** It collapses every one of these into
/// idle/busy, which means a session blocked on a permission prompt is
/// indistinguishable there from one that has finished. Callers must key on
/// this field and never conclude a worker is done from `ListAgents`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Actively working.
    Running,
    /// Finished a response turn and is waiting for the next prompt.
    ResponseReady,
    /// **Blocked on a permission prompt. Nobody is coming unless a human
    /// acts.** Sticky by design: allele will not let a later turn-complete
    /// overwrite it, so the signal can be trusted rather than re-checked.
    AwaitingInput,
    /// Started, or ended. After a confirmed `create()` this cannot mean
    /// "never received its prompt" — creation does not report success until
    /// the prompt is observed arriving.
    Idle,
    /// Rehydrated from disk with no agent process attached.
    Suspended,
    Done,
}

impl From<SessionStatus> for SessionState {
    fn from(s: SessionStatus) -> Self {
        match s {
            SessionStatus::Running => SessionState::Running,
            SessionStatus::ResponseReady => SessionState::ResponseReady,
            SessionStatus::AwaitingInput => SessionState::AwaitingInput,
            SessionStatus::Idle => SessionState::Idle,
            SessionStatus::Suspended => SessionState::Suspended,
            SessionStatus::Done => SessionState::Done,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: String,
    pub project: String,
    pub state: SessionState,
    /// Seconds since `state` last changed.
    ///
    /// Carried because "blocked for 40 minutes" is actionable and "blocked"
    /// is only a state. A fleet orchestrator watching twenty sessions needs
    /// to tell a permission prompt that just appeared from one that has been
    /// stranded since before lunch.
    pub state_age_secs: u64,
    /// `true` when allele started this session on behalf of another session.
    pub dispatched: bool,
    /// Dispatch depth: 0 for human-started, 1 for dispatched by a human's
    /// session, and so on. Derived by allele; never accepted from a caller.
    pub depth: u8,
    /// This session's messaging address, as `uds:<path>` (DEV-440).
    ///
    /// The durable way to reach it: derived from the agent process rather than
    /// from `name`, so renaming cannot break it and two sessions sharing a
    /// name cannot share it. This is also how a session discovers **its own**
    /// address to hand out — `ListAgents` does not list self, so looking up
    /// its own `session_id` here is the only route.
    ///
    /// `None` for a suspended session, or one whose socket allele cannot find.
    #[serde(default)]
    pub reply_to: Option<String>,
}

/// Why a request failed.
///
/// The contract is **non-interactive**: allele never raises UI in response to
/// a control-socket call, and always answers with one of these. Two of
/// allele's normal creation paths are interactive — a missing source path
/// opens a Relocate modal, a dirty worktree prompts — and both must become
/// codes here rather than a dialog nobody is watching.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Reserved for the client side: the socket refused the connection.
    /// Allele itself can never send this — which is the point of using a
    /// socket. A request written to a spool always "succeeds", leaving a
    /// caller unable to tell "busy" from "not running" except by timing out.
    AlleleNotRunning,
    ProjectNotRegistered,
    SourcePathMissing,
    WorktreeDirty,
    CloneFailed,
    BranchResolutionFailed,
    AgentNotConfigured,
    /// The dispatched-session cap is full. Counts dispatched sessions only;
    /// human-started ones are uncapped, because a human can see what they
    /// are doing and an orchestrator in a loop cannot.
    CapacityExceeded,
    /// Dispatch depth limit reached. Bounds *dispatch* recursion only —
    /// a session can still shell out to other tooling that allele cannot see.
    DepthLimitExceeded,
    /// The session started, but allele never saw the prompt arrive.
    ///
    /// The session is **left alive and visible** deliberately. A visible idle
    /// session is recoverable by a human; a phantom success reported to an
    /// orchestrator is not.
    PromptDeliveryUnconfirmed,
    /// The session exists but was started by a human, not by an agent.
    ///
    /// A caller may clean up what agents created; a human's session is not
    /// the agent's to delete. Distinct from `bad_request` so a caller can
    /// tell "you may not do that" from "that does not exist".
    NotDispatched,
    /// Malformed request, unknown project field, etc.
    BadRequest,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_over_the_wire() {
        let reqs = [
            Request::ProjectsList,
            Request::SessionsList,
            Request::SessionsStatus {
                session_id: "abc".into(),
            },
            Request::SessionsCreate(CreateRequest {
                project: "allele".into(),
                name: "Fix The Thing".into(),
                prompt: "read https://example.test/brief".into(),
                orchestration: Orchestration::StartupOnly,
                caller_session_id: Some("caller".into()),
                caller_reply_to: Some("uds:/tmp/cc-socks/65452.sock".into()),
            }),
        ];
        for r in reqs {
            let line = serde_json::to_string(&r).expect("serialises");
            assert!(
                !line.contains('\n'),
                "line framing forbids embedded newlines"
            );
            assert_eq!(serde_json::from_str::<Request>(&line).expect("parses"), r);
        }
    }

    /// Discard must round-trip like every other op — it is the one that
    /// removes a workspace, so a malformed request reaching the handler is
    /// the worst case in this protocol.
    #[test]
    fn discard_round_trips_over_the_wire() {
        let r = Request::SessionsDiscard {
            session_id: "b1413d28".into(),
        };
        let line = serde_json::to_string(&r).expect("serialises");
        assert_eq!(line, r#"{"op":"sessions_discard","session_id":"b1413d28"}"#);
        assert_eq!(serde_json::from_str::<Request>(&line).expect("parses"), r);
    }

    #[test]
    fn interrupt_round_trips_over_the_wire() {
        let r = Request::SessionsInterrupt {
            session_id: "s1".into(),
        };
        let line = serde_json::to_string(&r).expect("serialises");
        assert_eq!(line, r#"{"op":"sessions_interrupt","session_id":"s1"}"#);
        assert_eq!(serde_json::from_str::<Request>(&line).expect("parses"), r);
    }

    /// `not_dispatched` has to be distinguishable on the wire from
    /// `bad_request`: "you may not do that" and "that does not exist" want
    /// different responses from a caller.
    #[test]
    fn refusing_a_human_session_is_its_own_code() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::NotDispatched).expect("serialises"),
            "\"not_dispatched\""
        );
        assert_ne!(ErrorCode::NotDispatched, ErrorCode::BadRequest);
    }

    /// A dispatched session that isn't told otherwise starts bare: a
    /// workspace and a branch, and none of the project's setup behind them.
    #[test]
    fn create_defaults_to_nothing() {
        let r: CreateRequest =
            serde_json::from_str(r#"{"project":"p","name":"n","prompt":"go"}"#).expect("parses");
        assert_eq!(r.orchestration, Orchestration::Nothing);
    }

    /// The dispatch default is a separate decision from the type's `Default`,
    /// and they deliberately disagree: `Full` is right for a human opening the
    /// New Session dialog, `Nothing` for an agent that wants a workspace. A
    /// refactor collapsing the two would silently start a project's whole
    /// stack behind every dispatched session.
    #[test]
    fn dispatch_default_is_not_the_human_default() {
        assert_ne!(default_dispatch_orchestration(), Orchestration::default());
        assert_eq!(Orchestration::default(), Orchestration::Full);
    }

    /// An MCP client from before DEV-440 sends no `caller_reply_to`. It must
    /// still parse: the field is a strict addition, and a create that arrives
    /// without one is a dispatch whose worker is told it has no way back —
    /// which is the honest answer, not a protocol error.
    #[test]
    fn create_without_a_reply_address_still_parses() {
        let r: CreateRequest =
            serde_json::from_str(r#"{"project":"p","name":"n","prompt":"go"}"#).expect("parses");
        assert_eq!(r.caller_reply_to, None);
    }

    /// The wire vocabulary is snake_case and must stay stable independently of
    /// allele's internal enum names. Asserted literally so a rename upstream
    /// breaks this test rather than a consumer.
    #[test]
    fn state_wire_values_are_stable_snake_case() {
        for (state, wire) in [
            (SessionState::Running, "\"running\""),
            (SessionState::ResponseReady, "\"response_ready\""),
            (SessionState::AwaitingInput, "\"awaiting_input\""),
            (SessionState::Idle, "\"idle\""),
            (SessionState::Suspended, "\"suspended\""),
            (SessionState::Done, "\"done\""),
        ] {
            assert_eq!(serde_json::to_string(&state).expect("serialises"), wire);
        }
    }

    #[test]
    fn error_codes_are_stable_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::PromptDeliveryUnconfirmed).expect("serialises"),
            "\"prompt_delivery_unconfirmed\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::CapacityExceeded).expect("serialises"),
            "\"capacity_exceeded\""
        );
    }

    /// Every internal status has a wire equivalent. If allele grows a variant,
    /// the `From` impl stops compiling — this test documents that the mapping
    /// is total rather than lossy.
    #[test]
    fn every_internal_status_maps_to_a_wire_state() {
        for (internal, wire) in [
            (SessionStatus::Running, SessionState::Running),
            (SessionStatus::ResponseReady, SessionState::ResponseReady),
            (SessionStatus::AwaitingInput, SessionState::AwaitingInput),
            (SessionStatus::Idle, SessionState::Idle),
            (SessionStatus::Suspended, SessionState::Suspended),
            (SessionStatus::Done, SessionState::Done),
        ] {
            assert_eq!(SessionState::from(internal), wire);
        }
    }
}
