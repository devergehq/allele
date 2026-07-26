//! Wire-format types for Claude Code's stream-json output.
//!
//! Each line of stdout is a JSON object. These types deserialise every
//! known variant. Unknown fields and variants are silently ignored to
//! tolerate Claude Code updates.

use serde::Deserialize;
use std::collections::HashMap;

// ── Layer 1: Wire format (NDJSON lines) ───────────────────────────

/// Top-level discriminator. Every NDJSON line has a `type` field.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type")]
pub enum StreamLine {
    /// System events: init, hooks, plugins.
    #[serde(rename = "system")]
    System(SystemEvent),

    /// Complete assistant message (one or more content blocks).
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),

    /// Tool result fed back to the model.
    #[serde(rename = "user")]
    User(UserMessage),

    /// Token-level streaming event (only with `--include-partial-messages`).
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEventWrapper),

    /// Final result of the `-p` invocation.
    #[serde(rename = "result")]
    Result(ResultEvent),

    /// Rate-limit status update.
    #[serde(rename = "rate_limit_event")]
    RateLimit(RateLimitEvent),

    /// Session bookkeeping Claude Code interleaves with the conversation in the
    /// on-disk JSONL: titles, modes, agent names, file-history backups, bridge
    /// and fork pointers. None of it belongs in a reading view — it is recorded
    /// by the ledger and rendered nowhere (DEV-321). Together these account for
    /// roughly a fifth of every session file.
    #[serde(
        rename = "last-prompt",
        alias = "custom-title",
        alias = "ai-title",
        alias = "agent-name",
        alias = "permission-mode",
        alias = "mode",
        alias = "file-history-snapshot",
        alias = "file-history-delta",
        alias = "bridge-session",
        alias = "fork-context-ref",
        alias = "summary"
    )]
    SessionMetadata,

    /// A pull request was opened during the session.
    #[serde(rename = "pr-link")]
    PrLink(PrLinkEvent),

    /// An artifact was published to claude.ai.
    #[serde(rename = "frame-link")]
    FrameLink(FrameLinkEvent),

    /// A message or task notification was queued.
    #[serde(rename = "queue-operation")]
    QueueOperation(QueueOperationEvent),

    /// Out-of-band content attached to the conversation. Multiplexes ~20
    /// subtypes under `attachment.type`, most of them context plumbing.
    #[serde(rename = "attachment")]
    Attachment(AttachmentEvent),

    /// Catch-all for unknown/future event types.
    #[serde(other)]
    Unknown,
}

// ── JSONL session records (DEV-321) ───────────────────────────────
//
// Claude Code's on-disk session JSONL is a superset of stream-json. These
// records appear only there, and unlike the stream-json types above they use
// camelCase keys — hence `rename_all` on each struct.

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
pub struct PrLinkEvent {
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub pr_repository: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
pub struct FrameLinkEvent {
    pub title: Option<String>,
    pub frame_url: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(rename_all = "camelCase")]
pub struct QueueOperationEvent {
    pub operation: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AttachmentEvent {
    /// Absent or unrecognised subtypes deserialise to `Attachment::Other`.
    pub attachment: Option<Attachment>,
}

/// The subtypes of `attachment` we render. Everything else — task reminders,
/// skill listings, hook success, MCP/agent/tool listing deltas, date changes —
/// is context plumbing and lands in `Other`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type")]
pub enum Attachment {
    /// The user edited a file outside Claude and it was re-read.
    #[serde(rename = "edited_text_file", rename_all = "camelCase")]
    EditedTextFile { filename: Option<String> },

    /// The user attached a file or image to the conversation.
    #[serde(rename = "file", rename_all = "camelCase")]
    File {
        filename: Option<String>,
        display_path: Option<String>,
    },

    /// A plan was accepted and plan mode exited.
    #[serde(rename = "plan_mode_exit", rename_all = "camelCase")]
    PlanModeExit { plan_file_path: Option<String> },

    /// A hook returned an error. `blocking_error` is an object in the samples
    /// we have but is modelled as a free value so a bare string also parses.
    #[serde(rename = "hook_blocking_error", rename_all = "camelCase")]
    HookBlockingError {
        hook_name: Option<String>,
        blocking_error: Option<serde_json::Value>,
    },

    #[serde(rename = "hook_non_blocking_error", rename_all = "camelCase")]
    HookNonBlockingError {
        hook_name: Option<String>,
        #[serde(alias = "nonBlockingError")]
        blocking_error: Option<serde_json::Value>,
    },

    /// Every other subtype — recorded by the ledger, rendered nowhere.
    #[serde(other)]
    Other,
}

// ── System events ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SystemEvent {
    pub subtype: String,
    pub session_id: Option<String>,
    /// Present on `subtype: "init"` — lists available tools.
    pub tools: Option<Vec<String>>,
    /// Present on `subtype: "init"`.
    pub model: Option<String>,
    /// Hook stdout (for hook_response events).
    pub stdout: Option<String>,
    /// Hook event type (e.g. "SessionStart", "PreToolUse").
    pub hook_event: Option<String>,
    pub hook_name: Option<String>,
    /// Remaining fields we don't need but shouldn't reject.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ── Assistant message ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AssistantMessage {
    pub message: AssistantMessageBody,
    pub parent_tool_use_id: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AssistantMessageBody {
    pub id: Option<String>,
    pub model: Option<String>,
    /// Content blocks. Wrapped in `MaybeBlock` so any block whose `type`
    /// we don't recognise is preserved as raw JSON rather than discarded.
    pub content: Vec<MaybeBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// A content block that is either a recognised [`ContentBlock`] or, when the
/// block's `type` is unknown, its raw JSON value. This is what makes the
/// assistant-message path lossless: unrecognised blocks survive as `Raw`
/// instead of collapsing into a data-free catch-all.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MaybeBlock {
    Known(ContentBlock),
    Raw(serde_json::Value),
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// Present when the tool was called by the agent directly vs. subagent.
        caller: Option<serde_json::Value>,
    },

    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    // NOTE: intentionally NO `#[serde(other)]` catch-all. Unknown block
    // types must fail to deserialise here so the enclosing `MaybeBlock`
    // untagged wrapper preserves them as `Raw` (lossless ingestion).
}

// ── User message (tool results) ───────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UserMessage {
    pub message: UserMessageBody,
    pub parent_tool_use_id: Option<String>,
    pub session_id: Option<String>,
    /// Structured tool result metadata (file content, etc.).
    pub tool_use_result: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UserMessageBody {
    pub role: Option<String>,
    pub content: serde_json::Value,
}

// ── Stream event (token-level, only with --include-partial-messages) ──

#[derive(Debug, Deserialize)]
pub struct StreamEventWrapper {
    pub event: StreamEventInner,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type")]
pub enum StreamEventInner {
    #[serde(rename = "message_start")]
    MessageStart { message: Option<serde_json::Value> },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockHeader,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: Delta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: Option<serde_json::Value>,
        usage: Option<Usage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type")]
pub enum ContentBlockHeader {
    #[serde(rename = "text")]
    Text { text: Option<String> },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: Option<String> },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type")]
pub enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(other)]
    Other,
}

// ── Result event ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ResultEvent {
    pub subtype: Option<String>,
    pub is_error: Option<bool>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u32>,
    pub result: Option<String>,
    pub session_id: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub usage: Option<serde_json::Value>,
    pub stop_reason: Option<String>,
}

// ── Shared types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

// ── Rate limit ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct RateLimitEvent {
    pub rate_limit_info: Option<serde_json::Value>,
}

// ── Layer 2: Rich events (Allele internal) ────────────────────────

/// High-level events consumed by the GPUI rendering layer.
/// These are the "spans" in the trace model.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RichEvent {
    /// Session initialised — tools, model, etc.
    Init {
        session_id: String,
        model: String,
        tools: Vec<String>,
    },

    /// Streaming text token.
    TextDelta {
        text: String,
        parent_agent_id: Option<String>,
    },

    /// Complete text block (from non-streaming mode).
    TextBlock {
        text: String,
        parent_agent_id: Option<String>,
    },

    /// Thinking/reasoning content.
    ThinkingBlock {
        thinking: String,
        parent_agent_id: Option<String>,
    },

    /// A tool call was made (complete input available).
    ToolUse {
        tool_use_id: String,
        tool_name: String,
        input: serde_json::Value,
        parent_agent_id: Option<String>,
    },

    /// Tool execution result.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        parent_agent_id: Option<String>,
    },

    /// Specialised: an Edit tool call with structured diff data.
    EditDiff {
        tool_use_id: String,
        file_path: String,
        old_string: String,
        new_string: String,
        parent_agent_id: Option<String>,
    },

    /// Session completed.
    SessionResult {
        duration_ms: u64,
        cost_usd: f64,
        num_turns: u32,
        is_error: bool,
        /// Error/result text from the CLI (if any).
        result_text: Option<String>,
    },

    /// Status change from hooks (awaiting input, response ready, etc.).
    HookStatus {
        hook_event: String,
        hook_name: String,
    },

    /// A compact one-line annotation for a session event that carries real
    /// narrative signal but isn't conversation — a PR opened, an artifact
    /// published, a file edited outside Claude (DEV-321).
    Notice {
        kind: NoticeKind,
        text: String,
        /// URL or file path the notice points at, when there is one.
        link: Option<String>,
        parent_agent_id: Option<String>,
    },

    /// An event or content block that could not be normalised. Carries the
    /// exact raw payload plus a human-readable reason so unsupported states
    /// remain inspectable and are never silently dropped. This is the
    /// in-band representation of "fallback rendering" (DEV-33).
    Fallback {
        /// The exact raw JSON (whole line, or the unrecognised block).
        raw: String,
        /// Why normalisation was unsupported (unknown type, invalid JSON…).
        reason: String,
        parent_agent_id: Option<String>,
    },
}

/// What a [`RichEvent::Notice`] is about. Drives the glyph and colour the
/// renderer picks; the accompanying text carries the detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// A pull request was opened.
    PullRequest,
    /// An artifact was published.
    Artifact,
    /// The user edited a file outside Claude.
    FileEdited,
    /// The user attached a file or image.
    FileAttached,
    /// A plan was accepted and plan mode exited.
    PlanAccepted,
    /// A message or task notification was queued.
    Queued,
    /// A hook reported an error.
    HookError,
}

impl NoticeKind {
    /// Leading glyph for this notice in the transcript.
    pub fn glyph(self) -> &'static str {
        match self {
            NoticeKind::PullRequest => "⑂",
            NoticeKind::Artifact => "◈",
            NoticeKind::FileEdited => "✎",
            NoticeKind::FileAttached => "⎘",
            NoticeKind::PlanAccepted => "✓",
            NoticeKind::Queued => "⋯",
            NoticeKind::HookError => "⚠",
        }
    }

    /// True when this notice reports a failure and should be coloured as one.
    pub fn is_error(self) -> bool {
        matches!(self, NoticeKind::HookError)
    }
}
