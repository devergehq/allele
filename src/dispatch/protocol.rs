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
    /// How much of the project's setup to run. Defaults to `StartupOnly` for
    /// dispatched sessions — a session that needs a database provisioned to
    /// run tests does not need a server, a queue worker, a scheduler and a
    /// bundler it will never look at.
    #[serde(default = "default_dispatch_orchestration")]
    pub orchestration: Orchestration,
}

fn default_dispatch_orchestration() -> Orchestration {
    Orchestration::StartupOnly
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
/// **This is not an address.** `SendMessage` needs `name [ref]`, and the ref
/// is minted inside Claude Code — allele has no route to it, so none of these
/// fields can be handed straight to a send. The caller must call `ListAgents`
/// and resolve `name` to `name [ref]` itself, **fresh at every send**: refs
/// rotate, and a name amortised against an old ref re-gates when it changes.
///
/// There is deliberately no convenience field that looks like an address.
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

    /// A dispatched session that isn't told otherwise gets the startup command
    /// without the terminals — the shape dispatch exists to produce.
    #[test]
    fn create_defaults_to_startup_only() {
        let r: CreateRequest =
            serde_json::from_str(r#"{"project":"p","name":"n","prompt":"go"}"#).expect("parses");
        assert_eq!(r.orchestration, Orchestration::StartupOnly);
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
