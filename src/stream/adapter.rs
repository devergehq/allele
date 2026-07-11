//! Normalizing adapters (DEV-32).
//!
//! Different coding agents emit different transcript formats. The *spawn*
//! side of this is handled by [`crate::agents::AgentAdapter`]; this module is
//! the *parse* side — turning each agent's raw output into Allele's shared
//! normalized event model ([`RichEvent`]) so the renderer is agent-agnostic.
//!
//! Every adapter also **declares its capabilities**: which concepts its
//! format can express (thinking, tools, diffs, permissions, usage). This lets
//! the UI render an unsupported concept *explicitly* ("this agent doesn't
//! report token usage") rather than silently omitting it — and, combined with
//! the lossless [`super::SessionLedger`], guarantees that even formats we only
//! partially understand keep their raw lines inspectable via `Fallback`.
//!
//! Coverage notes:
//!   * `claude` — full support, backed by the battle-tested [`StreamParser`].
//!   * `terminal` — generic fallback for agents with no structured transcript;
//!     every line is surfaced verbatim, no rich concepts claimed.
//!   * `opencode` — best-effort over OpenCode's JSON event lines. Only shapes
//!     we can map unambiguously are normalized; everything else falls back
//!     explicitly. This adapter is intended to be tightened against real
//!     OpenCode fixtures (tracked in DEV-32's DoD).

use super::parser::{Coverage, ParsedLine, StreamParser};
use super::types::RichEvent;
use crate::settings::AgentKind;

/// Which transcript concepts an agent's format can express. A `false` here is
/// a promise the UI can surface as "unsupported by this agent" instead of an
/// ambiguous absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub thinking: bool,
    pub tools: bool,
    pub diffs: bool,
    pub permissions: bool,
    pub usage: bool,
}

impl Capabilities {
    /// Nothing structured — a raw terminal stream.
    pub const NONE: Capabilities = Capabilities {
        thinking: false,
        tools: false,
        diffs: false,
        permissions: false,
        usage: false,
    };
    /// Everything Allele models.
    pub const ALL: Capabilities = Capabilities {
        thinking: true,
        tools: true,
        diffs: true,
        permissions: true,
        usage: true,
    };
}

/// Parse-side adapter: normalizes one agent format into `RichEvent`s.
///
/// Stateful across lines (mirrors [`StreamParser`]) so an adapter can carry
/// session/init state. One adapter instance per transcript file.
pub trait NormalizingAdapter: Send {
    /// The agent kind this adapter normalizes.
    fn agent_kind(&self) -> AgentKind;
    /// A stable, human-readable format label (for diagnostics/UI).
    fn format_label(&self) -> &'static str;
    /// What this format can express.
    fn capabilities(&self) -> Capabilities;
    /// Normalize a single raw line into events + coverage + diagnostics.
    fn feed_line(&mut self, line: &str) -> ParsedLine;
}

/// Build the parse-side adapter for an agent kind. `Generic` agents have no
/// structured transcript, so they use the terminal fallback.
pub fn normalizer_for(kind: AgentKind) -> Box<dyn NormalizingAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeNormalizer::new()),
        AgentKind::Opencode => Box::new(OpencodeNormalizer::new()),
        AgentKind::Generic => Box::new(TerminalNormalizer),
    }
}

// ── Claude ────────────────────────────────────────────────────────────────

/// Full-fidelity Claude Code JSONL / stream-json adapter.
pub struct ClaudeNormalizer {
    parser: StreamParser,
}

impl ClaudeNormalizer {
    pub fn new() -> Self {
        Self { parser: StreamParser::new() }
    }
}

impl NormalizingAdapter for ClaudeNormalizer {
    fn agent_kind(&self) -> AgentKind { AgentKind::Claude }
    fn format_label(&self) -> &'static str { "claude-jsonl" }
    fn capabilities(&self) -> Capabilities { Capabilities::ALL }
    fn feed_line(&mut self, line: &str) -> ParsedLine {
        self.parser.feed_line_detailed(line)
    }
}

// ── Terminal (generic fallback) ─────────────────────────────────────────────

/// Generic fallback for agents that write plain text to the terminal with no
/// structured transcript. Every non-empty line becomes a `TextBlock` so the
/// output is never lost; no rich concepts are claimed.
pub struct TerminalNormalizer;

impl NormalizingAdapter for TerminalNormalizer {
    fn agent_kind(&self) -> AgentKind { AgentKind::Generic }
    fn format_label(&self) -> &'static str { "terminal-text" }
    fn capabilities(&self) -> Capabilities { Capabilities::NONE }
    fn feed_line(&mut self, line: &str) -> ParsedLine {
        if line.trim().is_empty() {
            return ParsedLine { events: Vec::new(), coverage: Coverage::Ignored, diagnostics: Vec::new() };
        }
        ParsedLine {
            events: vec![RichEvent::TextBlock { text: line.to_string(), parent_agent_id: None }],
            coverage: Coverage::Full,
            diagnostics: Vec::new(),
        }
    }
}

// ── OpenCode (best-effort) ──────────────────────────────────────────────────

/// Best-effort adapter for OpenCode's JSON event lines.
///
/// OpenCode emits newline-delimited JSON. We normalize the shapes we can map
/// unambiguously (text, reasoning, tool invocations) and fall back explicitly
/// for anything else — the raw payload is preserved so a later, fixture-backed
/// revision can widen coverage without data loss. Non-JSON lines are treated
/// as plain terminal text (OpenCode interleaves human-readable logs).
pub struct OpencodeNormalizer {
    _private: (),
}

impl OpencodeNormalizer {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl NormalizingAdapter for OpencodeNormalizer {
    fn agent_kind(&self) -> AgentKind { AgentKind::Opencode }
    fn format_label(&self) -> &'static str { "opencode-json" }
    fn capabilities(&self) -> Capabilities {
        // OpenCode reports reasoning, tool calls, and usage; it does not (yet,
        // as modeled here) expose Allele-style structured diffs or permission
        // prompts. Declaring these false lets the UI say so explicitly.
        Capabilities { thinking: true, tools: true, diffs: false, permissions: false, usage: true }
    }
    fn feed_line(&mut self, line: &str) -> ParsedLine {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return ParsedLine { events: Vec::new(), coverage: Coverage::Ignored, diagnostics: Vec::new() };
        }
        // Non-JSON → plain terminal text (OpenCode interleaves logs).
        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => {
                return ParsedLine {
                    events: vec![RichEvent::TextBlock { text: line.to_string(), parent_agent_id: None }],
                    coverage: Coverage::Full,
                    diagnostics: Vec::new(),
                };
            }
        };

        // Discriminator: OpenCode part/event objects carry a `type`.
        let ty = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "text" | "message" => {
                if let Some(text) = extract_text(&value) {
                    return ParsedLine::full(vec![RichEvent::TextBlock { text, parent_agent_id: None }]);
                }
                fallback(line, "opencode text part missing text field")
            }
            "reasoning" | "thinking" => {
                if let Some(text) = extract_text(&value) {
                    return ParsedLine::full(vec![RichEvent::ThinkingBlock { thinking: text, parent_agent_id: None }]);
                }
                fallback(line, "opencode reasoning part missing text field")
            }
            "tool" | "tool_use" | "tool-invocation" => {
                let name = value
                    .get("tool")
                    .or_else(|| value.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let id = value
                    .get("id")
                    .or_else(|| value.get("callID"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = value.get("input").or_else(|| value.get("args")).cloned().unwrap_or(serde_json::Value::Null);
                ParsedLine::full(vec![RichEvent::ToolUse {
                    tool_use_id: id,
                    tool_name: name,
                    input,
                    parent_agent_id: None,
                }])
            }
            // Structural/no-op events we intentionally don't render, but whose
            // raw form the ledger still retains.
            "step-start" | "step-finish" | "start" | "finish" => {
                ParsedLine { events: Vec::new(), coverage: Coverage::Ignored, diagnostics: Vec::new() }
            }
            _ => fallback(line, &format!("unrecognised opencode event type: {ty}")),
        }
    }
}

fn extract_text(value: &serde_json::Value) -> Option<String> {
    value
        .get("text")
        .or_else(|| value.get("content"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn fallback(line: &str, reason: &str) -> ParsedLine {
    ParsedLine {
        events: vec![RichEvent::Fallback {
            raw: line.to_string(),
            reason: reason.to_string(),
            parent_agent_id: None,
        }],
        coverage: Coverage::Fallback,
        diagnostics: vec![reason.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_adapter_declares_full_capabilities() {
        let a = ClaudeNormalizer::new();
        assert_eq!(a.capabilities(), Capabilities::ALL);
        assert_eq!(a.agent_kind(), AgentKind::Claude);
    }

    #[test]
    fn claude_adapter_normalizes_via_stream_parser() {
        let mut a = normalizer_for(AgentKind::Claude);
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"stop_reason":null}}"#;
        let parsed = a.feed_line(line);
        assert!(matches!(parsed.events.first(), Some(RichEvent::TextBlock { .. })));
    }

    #[test]
    fn terminal_adapter_wraps_lines_and_claims_nothing() {
        let mut a = normalizer_for(AgentKind::Generic);
        assert_eq!(a.capabilities(), Capabilities::NONE);
        let parsed = a.feed_line("building project...");
        match parsed.events.first() {
            Some(RichEvent::TextBlock { text, .. }) => assert_eq!(text, "building project..."),
            other => panic!("expected TextBlock, got {other:?}"),
        }
    }

    #[test]
    fn opencode_adapter_normalizes_known_shapes() {
        let mut a = OpencodeNormalizer::new();
        let text = a.feed_line(r#"{"type":"text","text":"hello"}"#);
        assert!(matches!(text.events.first(), Some(RichEvent::TextBlock { .. })));

        let reasoning = a.feed_line(r#"{"type":"reasoning","text":"let me think"}"#);
        assert!(matches!(reasoning.events.first(), Some(RichEvent::ThinkingBlock { .. })));

        let tool = a.feed_line(r#"{"type":"tool","tool":"bash","id":"c1","input":{"cmd":"ls"}}"#);
        match tool.events.first() {
            Some(RichEvent::ToolUse { tool_name, .. }) => assert_eq!(tool_name, "bash"),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn opencode_adapter_falls_back_on_unknown_shapes() {
        let mut a = OpencodeNormalizer::new();
        let parsed = a.feed_line(r#"{"type":"quantum_event","data":1}"#);
        assert_eq!(parsed.coverage, Coverage::Fallback);
        match parsed.events.first() {
            Some(RichEvent::Fallback { raw, .. }) => assert!(raw.contains("quantum_event")),
            other => panic!("expected Fallback, got {other:?}"),
        }
    }

    #[test]
    fn opencode_adapter_declares_unsupported_diffs_and_permissions() {
        let a = OpencodeNormalizer::new();
        let caps = a.capabilities();
        assert!(caps.thinking && caps.tools && caps.usage);
        assert!(!caps.diffs, "diffs must be declared unsupported, not silently absent");
        assert!(!caps.permissions);
    }
}
