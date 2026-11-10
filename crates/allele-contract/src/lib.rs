//! Wire types for `allele.ext/1`, the extension contract on Allele's control
//! socket (DEV-1254).
//!
//! **This crate is the contract.** Allele's `~/.allele/state.json` and
//! `~/.allele/events/*.jsonl` are private and may change shape in any release;
//! the types here may not. A client in another repository (Review Desk, the
//! Locus dispatcher) can depend on this crate, or read the JSON fixtures in
//! `fixtures/` if it is not written in Rust.
//!
//! ## Rules
//!
//! - **Additive within a major version.** No field or variant is removed or
//!   renamed in `ext/1`. Every struct field has a default, so an older client
//!   parses a newer Allele's frames.
//! - **Unknown enum values parse as `Unknown`.** A client must treat them as
//!   "something this version does not know about", never as an error.
//! - **Status is source-agnostic.** `state` is one normalised value. Clients
//!   must not try to tell which of Allele's sources (hooks, transcript tail,
//!   OSC 7501) produced it.
//!
//! ## The connection, in one paragraph
//!
//! A connection that never sends `hello` is the *legacy tier*: the six ops of
//! the original control socket, answered exactly as before. Sending
//! [`Hello`] first upgrades the connection to `allele.ext/1`. Allele answers
//! with a [`HelloResult`] naming the capabilities it [granted](Capability).
//! `subscribe` (needs `observe`) then turns the connection into a one-way
//! stream of [`Frame`]s: a [`Frame::Snapshot`] first, then [`Frame::Delta`]s
//! whose `seq` counts up by exactly one, until Allele sends [`Frame::Lagged`]
//! and closes the connection, or the client hangs up.

use serde::{Deserialize, Serialize};

/// The contract this crate describes, as it travels in `hello.contract`.
pub const CONTRACT: &str = "allele.ext/1";

/// The `client.id` of the MCP server that ships inside Allele. The only id
/// granted `dispatch` without a manifest (manifests arrive in a later rung).
pub const BUILTIN_MCP_CLIENT_ID: &str = "allele-mcp";

// ── Requests ────────────────────────────────────────────────────────────

/// A request on an `ext/1` connection, tagged by `op`.
///
/// Only the ops this contract adds are modelled here. After `hello`, the
/// legacy ops (`projects_list`, `sessions_list`, `sessions_status`,
/// `sessions_create`, `sessions_discard`, `sessions_interrupt`) keep their
/// legacy request shapes, gated by capability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Hello(Hello),
    Subscribe(Subscribe),
}

/// `{"op":"hello", ...}`: the first line of an `ext/1` connection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hello {
    /// The contract the client speaks, e.g. `allele.ext/1`. A major version
    /// Allele does not serve is refused with
    /// [`ErrorCode::UnsupportedContract`].
    pub contract: String,
    pub client: ClientInfo,
    /// What the client asks for. Allele grants a subset; see
    /// [`HelloResult::granted`].
    pub capabilities: Vec<Capability>,
}

/// Who the client says it is. A claim, not an identity: every client runs as
/// the same user on a socket with mode 0600. Capabilities contain accidents;
/// they are not a security boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClientInfo {
    pub id: String,
    pub version: String,
}

/// What a connection may do. Ops outside the granted set are answered with
/// [`ErrorCode::CapabilityNotGranted`].
///
/// Only `observe` and `dispatch` are granted in this rung. The others are
/// named so a client can ask for them today and be told no.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `sessions_list`, `sessions_status`, `projects_list`, `subscribe{sessions}`.
    Observe,
    /// `subscribe{activity}`. Not served yet.
    Activity,
    /// `badge.set/clear`. Not served yet.
    Badge,
    /// `compose.stage`. Not served yet.
    Stage,
    /// `decisions.post/withdraw`. Not served yet.
    Decisions,
    /// Desk presence. Not served yet.
    Presence,
    /// `sessions_create`, `sessions_discard`, `sessions_interrupt`. Granted
    /// only to [`BUILTIN_MCP_CLIENT_ID`] until manifests exist.
    Dispatch,
    #[serde(other)]
    Unknown,
}

/// `{"op":"subscribe","topics":["sessions"],"filter":{...}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Subscribe {
    pub topics: Vec<Topic>,
    /// Narrows the `sessions` topic. Absent, or every field absent, means
    /// every session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// Session lifecycle and status. The only topic served in this rung.
    Sessions,
    /// Per-step activity. Needs the `activity` capability, never granted yet.
    Activity,
    PeerDesks,
    Decisions,
    #[serde(other)]
    Unknown,
}

/// Which sessions a subscription hears about. The fields are **OR-combined**:
/// a session matching any one is included.
///
/// **A convenience, never a boundary.** A filter grants nothing and hides
/// nothing a client could not read by subscribing without one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Filter {
    /// Sessions whose `parent` chain reaches this session id: children,
    /// grandchildren, and so on. Not the session itself. The chain is walked
    /// through sessions Allele still has, so it breaks where an ancestor has
    /// been discarded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descendants_of: Option<String>,
    /// Sessions in this sidebar group, compared ignoring case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// These exact session ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_ids: Option<Vec<String>>,
}

// ── Replies ─────────────────────────────────────────────────────────────

/// A reply on an `ext/1` connection, tagged by `result`.
///
/// Replies to the legacy ops that are not reshaped (`projects`, `created`,
/// `discarded`, `interrupted`) keep their legacy bodies and are not modelled
/// here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Hello(HelloResult),
    /// `sessions_list` after `hello`: the widened [`SessionEntry`].
    Sessions {
        sessions: Vec<SessionEntry>,
    },
    /// `sessions_status` after `hello`. An archived session is answered with
    /// `state: archived` rather than `session_not_found`.
    Status {
        session: SessionEntry,
    },
    Error(ErrorReply),
}

/// The answer to [`Hello`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HelloResult {
    /// Every contract this Allele serves.
    pub contract_versions: Vec<String>,
    /// Identifies this run of Allele. `turn` and `seq` are only comparable
    /// within one epoch; a new epoch means Allele restarted.
    pub epoch: String,
    /// The capabilities this connection holds.
    pub granted: Vec<Capability>,
    pub dispatch_limits: DispatchLimits,
}

/// Read-only dispatch limits, from the user's `settings.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DispatchLimits {
    pub max_depth: u8,
    pub max_sessions: u64,
    /// Dispatched sessions that exist right now, against `max_sessions`.
    pub live_dispatched: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorReply {
    pub code: ErrorCode,
    #[serde(default)]
    pub message: String,
}

/// Why a request failed. The legacy codes, plus the two `ext/1` adds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// `hello.contract` names a major version this Allele does not serve.
    UnsupportedContract,
    /// The op needs a capability this connection was not granted.
    CapabilityNotGranted,
    AlleleNotRunning,
    ProjectNotRegistered,
    SourcePathMissing,
    WorktreeDirty,
    CloneFailed,
    BranchResolutionFailed,
    AgentNotConfigured,
    CapacityExceeded,
    DepthLimitExceeded,
    PromptDeliveryUnconfirmed,
    NotDispatched,
    SessionNotFound,
    PermissionDenied,
    BadRequest,
    Internal,
    #[serde(other)]
    Unknown,
}

// ── Session entry ───────────────────────────────────────────────────────

/// One session, as the snapshot, the deltas, and `sessions_status` carry it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SessionEntry {
    /// Allele's stable session id. The only durable identity; key on it.
    pub session_id: String,
    /// Allele's mutable label. Not an address.
    pub name: String,
    pub project: String,
    /// Sidebar group in the project's spelling, `null` when ungrouped.
    pub group: Option<String>,
    pub state: State,
    pub input: Input,
    /// Seconds since `state` last changed. A delta is **not** sent when only
    /// this moves; compute it forward from the frame's arrival if needed.
    pub state_age_secs: u64,
    /// Counts the session's turns within one epoch. It moves when the session
    /// enters `running` from any state except `awaiting_input` (answering a
    /// permission prompt continues the turn). **It does not prove a message
    /// was read**: text delivered mid-turn lands inside the current turn.
    pub turn: u64,
    pub dispatched: bool,
    /// 0 for a human's session, 1 for one it dispatched, and so on.
    pub depth: u8,
    /// The id of the session that dispatched this one, `null` for a human's
    /// session or when the dispatcher did not identify itself.
    pub parent: Option<String>,
    /// The dispatcher's label at dispatch time, for display.
    pub dispatched_by: Option<String>,
    /// The session's messaging address (`uds:<path>`), when Allele can find
    /// one bound.
    pub reply_to: Option<String>,
    /// When the session was created, in Unix seconds. Set once.
    pub started_at: u64,
    /// When the session was archived, in Unix seconds. Only with
    /// `state: archived`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
    /// The git ref holding the archived work, e.g. `refs/allele/archive/<id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_ref: Option<String>,
}

/// A session's normalised status.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    ResponseReady,
    /// Blocked on a prompt. Nobody is coming unless a human acts.
    AwaitingInput,
    Idle,
    /// No agent process attached.
    Suspended,
    Done,
    /// Removed from the sidebar, its work kept under `archive_ref`.
    Archived,
    #[default]
    #[serde(other)]
    Unknown,
}

/// Whether text sent to the session now would reach the agent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    /// The agent is between turns and would read it now.
    DeliverableNow,
    /// The agent is working; it would read it at its next turn boundary.
    DeliverableAtNextBoundary,
    /// A prompt is on screen; typed text would answer the prompt.
    BlockedOnPrompt,
    /// There is no agent process to deliver to.
    NoProcess,
    #[default]
    #[serde(other)]
    Unknown,
}

// ── Stream ──────────────────────────────────────────────────────────────

/// One line on a subscribed connection, tagged by `frame`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// Always first. Every session the filter matches, at `seq`.
    Snapshot {
        seq: u64,
        sessions: Vec<SessionEntry>,
    },
    /// One change. `seq` is exactly one more than the previous frame's.
    Delta {
        seq: u64,
        #[serde(flatten)]
        event: Event,
    },
    /// This connection fell behind and is being closed. Drop your state and
    /// subscribe again.
    Lagged,
    #[serde(other)]
    Unknown,
}

/// What a [`Frame::Delta`] reports, tagged by `event`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event")]
pub enum Event {
    /// Entries are boxed in memory only; serde writes `Box<T>` as `T`.
    #[serde(rename = "session.added")]
    SessionAdded { session: Box<SessionEntry> },
    /// Anything in the entry changed except `state_age_secs`. Also sent when
    /// only `turn` moved: a turn that went `running` → `response_ready` →
    /// `running` between two looks is still a new turn.
    #[serde(rename = "session.status_changed")]
    SessionStatusChanged { session: Box<SessionEntry> },
    #[serde(rename = "session.removed")]
    SessionRemoved {
        session_id: String,
        reason: RemovalReason,
    },
    /// A `sessions_create` was refused by a dispatch limit.
    #[serde(rename = "dispatch.refused")]
    DispatchRefused(DispatchRefused),
    /// The dispatch limits or the live dispatched count changed.
    #[serde(rename = "limits_changed")]
    LimitsChanged { dispatch_limits: DispatchLimits },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemovalReason {
    /// Removed through `sessions_discard`.
    Discarded,
    /// Removed in Allele's UI; the work is archived.
    Archived,
    /// Gone for any other reason.
    Closed,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DispatchRefused {
    /// The session id the refused caller claimed, `null` if it named none.
    pub caller: Option<String>,
    /// `depth_limit_exceeded` or `capacity_exceeded`.
    pub code: Option<ErrorCode>,
    /// The depth the refused child would have had.
    pub depth: u8,
    /// The limit that refused it: `max_depth` or `max_sessions`.
    pub limit: u64,
    /// Unix seconds.
    pub at: u64,
    /// Refusals for this caller so far in this epoch, this one included.
    pub count: u64,
}

/// Whether `contract` names a major version this crate describes.
///
/// `allele.ext/1` and any `allele.ext/1.<minor>` are supported: minors are
/// additive by rule. Anything else, including a malformed string, is not.
pub fn is_supported(contract: &str) -> bool {
    contract
        .strip_prefix(CONTRACT)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}
