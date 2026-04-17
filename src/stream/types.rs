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

    /// Catch-all for unknown/future event types.
    #[serde(other)]
    Unknown,
}

// ── System events ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
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
pub struct AssistantMessage {
    pub message: AssistantMessageBody,
    pub parent_tool_use_id: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessageBody {
    pub id: Option<String>,
    pub model: Option<String>,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
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

    #[serde(other)]
    Other,
}

// ── User message (tool results) ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UserMessage {
    pub message: UserMessageBody,
    pub parent_tool_use_id: Option<String>,
    pub session_id: Option<String>,
    /// Structured tool result metadata (file content, etc.).
    pub tool_use_result: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
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
#[serde(tag = "type")]
pub enum StreamEventInner {
    #[serde(rename = "message_start")]
    MessageStart {
        message: Option<serde_json::Value>,
    },
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
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

// ── Rate limit ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RateLimitEvent {
    pub rate_limit_info: Option<serde_json::Value>,
}

// ── Layer 2: Rich events (Allele internal) ────────────────────────

/// High-level events consumed by the GPUI rendering layer.
/// These are the "spans" in the trace model.
#[derive(Debug, Clone)]
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
    },

    /// Status change from hooks (awaiting input, response ready, etc.).
    HookStatus {
        hook_event: String,
        hook_name: String,
    },
}
