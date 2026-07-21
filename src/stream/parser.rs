//! Two-layer parser: StreamLine (wire) → RichEvent (internal).
//!
//! The parser is stateless between lines — each NDJSON line produces
//! zero or more `RichEvent`s. Tool inputs arrive complete (not chunked)
//! when using stream-json without `--include-partial-messages`.

use super::types::*;

/// How completely a single source line was normalised. Recorded per line so
/// the ledger can report parser coverage without inspecting event contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Line fully recognised; every part produced a normalised event.
    Full,
    /// Line recognised, but one or more parts fell back to raw.
    Partial,
    /// Line recognised as a known type we intentionally emit nothing for
    /// (e.g. rate-limit, pings). The raw line is still retained by the ledger.
    Ignored,
    /// Line's top-level `type` was unknown — emitted wholesale as `Fallback`.
    Fallback,
    /// Line was not valid JSON — emitted as `Fallback` with the parse error.
    Unparsed,
}

/// Result of parsing one NDJSON line: the normalised events plus the
/// coverage classification and any diagnostics gathered along the way.
#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub events: Vec<RichEvent>,
    pub coverage: Coverage,
    pub diagnostics: Vec<String>,
}

impl ParsedLine {
    fn new(events: Vec<RichEvent>, coverage: Coverage) -> Self {
        Self {
            events,
            coverage,
            diagnostics: Vec::new(),
        }
    }
}

/// Transforms wire-format `StreamLine`s into Allele's `RichEvent`s.
pub struct StreamParser {
    /// Session ID extracted from the init event.
    session_id: Option<String>,
}

impl StreamParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }

    /// Parse a single NDJSON line. Returns events to emit (may be empty).
    ///
    /// Back-compatible thin wrapper over [`feed_line_detailed`]. Note that
    /// unknown/unparseable lines now yield a `RichEvent::Fallback` rather than
    /// an empty vec — nothing is silently dropped.
    pub fn feed_line(&mut self, line: &str) -> Vec<RichEvent> {
        self.feed_line_detailed(line).events
    }

    /// Parse a single NDJSON line, returning normalised events together with
    /// the coverage classification and diagnostics. This is the lossless
    /// entry point: every input line maps to exactly one `ParsedLine`, and no
    /// recognised-but-unsupported shape is ever discarded without a trace.
    pub fn feed_line_detailed(&mut self, line: &str) -> ParsedLine {
        let parsed: StreamLine = match serde_json::from_str(line) {
            Ok(p) => p,
            Err(e) => {
                let reason = format!("invalid JSON: {e}");
                let mut pl = ParsedLine::new(
                    vec![RichEvent::Fallback {
                        raw: line.to_string(),
                        reason: reason.clone(),
                        parent_agent_id: None,
                    }],
                    Coverage::Unparsed,
                );
                pl.diagnostics.push(reason);
                return pl;
            }
        };

        match parsed {
            StreamLine::System(sys) => self.handle_system(sys),
            StreamLine::Assistant(msg) => self.handle_assistant(msg),
            StreamLine::User(msg) => self.handle_user(msg),
            StreamLine::StreamEvent(wrapper) => self.handle_stream_event(wrapper),
            StreamLine::Result(result) => self.handle_result(result),
            StreamLine::RateLimit(_) => ParsedLine::new(Vec::new(), Coverage::Ignored),
            StreamLine::Unknown => {
                let reason = "unknown top-level event type".to_string();
                let mut pl = ParsedLine::new(
                    vec![RichEvent::Fallback {
                        raw: line.to_string(),
                        reason: reason.clone(),
                        parent_agent_id: None,
                    }],
                    Coverage::Fallback,
                );
                pl.diagnostics.push(reason);
                pl
            }
        }
    }

    fn handle_system(&mut self, sys: SystemEvent) -> ParsedLine {
        match sys.subtype.as_str() {
            "init" => {
                if let Some(sid) = &sys.session_id {
                    self.session_id = Some(sid.clone());
                }
                ParsedLine::new(
                    vec![RichEvent::Init {
                        session_id: sys.session_id.unwrap_or_default(),
                        model: sys.model.unwrap_or_default(),
                        tools: sys.tools.unwrap_or_default(),
                    }],
                    Coverage::Full,
                )
            }
            "hook_response" => {
                // Surface hook events that indicate status changes
                if let (Some(event), Some(name)) = (sys.hook_event, sys.hook_name) {
                    match event.as_str() {
                        "PreToolUse" | "PostToolUse" | "Notification" | "Stop" => ParsedLine::new(
                            vec![RichEvent::HookStatus {
                                hook_event: event,
                                hook_name: name,
                            }],
                            Coverage::Full,
                        ),
                        _ => ParsedLine::new(Vec::new(), Coverage::Ignored),
                    }
                } else {
                    ParsedLine::new(Vec::new(), Coverage::Ignored)
                }
            }
            _ => ParsedLine::new(Vec::new(), Coverage::Ignored),
        }
    }

    fn handle_assistant(&mut self, msg: AssistantMessage) -> ParsedLine {
        let parent = msg.parent_tool_use_id;
        let mut events = Vec::new();
        let mut diagnostics = Vec::new();
        let mut fell_back = false;

        for block in msg.message.content {
            let block = match block {
                MaybeBlock::Known(b) => b,
                MaybeBlock::Raw(value) => {
                    // Unrecognised content-block type — preserve it verbatim.
                    let ty = value
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or("<no type>")
                        .to_string();
                    diagnostics.push(format!("unrecognised content block type: {ty}"));
                    fell_back = true;
                    events.push(RichEvent::Fallback {
                        raw: value.to_string(),
                        reason: format!("unrecognised content block type: {ty}"),
                        parent_agent_id: parent.clone(),
                    });
                    continue;
                }
            };
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        events.push(RichEvent::TextBlock {
                            text,
                            parent_agent_id: parent.clone(),
                        });
                    }
                }
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => {
                    // Check if this is an Edit tool — extract diff data
                    if name == "Edit" || name == "edit_file" {
                        if let Some(diff) = extract_edit_diff(&id, &input, &parent) {
                            events.push(diff);
                            continue;
                        }
                    }
                    events.push(RichEvent::ToolUse {
                        tool_use_id: id,
                        tool_name: name,
                        input,
                        parent_agent_id: parent.clone(),
                    });
                }
                ContentBlock::Thinking { thinking, .. } => {
                    if !thinking.is_empty() {
                        events.push(RichEvent::ThinkingBlock {
                            thinking,
                            parent_agent_id: parent.clone(),
                        });
                    }
                }
            }
        }

        let coverage = if fell_back {
            Coverage::Partial
        } else {
            Coverage::Full
        };
        ParsedLine {
            events,
            coverage,
            diagnostics,
        }
    }

    fn handle_user(&mut self, msg: UserMessage) -> ParsedLine {
        let parent = msg.parent_tool_use_id;
        let mut events = Vec::new();

        // Extract tool results from the user message content
        if let Some(content_arr) = msg.message.content.as_array() {
            for item in content_arr {
                if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    let tool_use_id = item
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content = item
                        .get("content")
                        .map(|v| {
                            if let Some(s) = v.as_str() {
                                s.to_string()
                            } else {
                                v.to_string()
                            }
                        })
                        .unwrap_or_default();
                    let is_error = item
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    events.push(RichEvent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        parent_agent_id: parent.clone(),
                    });
                }
            }
        }

        ParsedLine::new(events, Coverage::Full)
    }

    fn handle_stream_event(&mut self, wrapper: StreamEventWrapper) -> ParsedLine {
        match wrapper.event {
            StreamEventInner::ContentBlockDelta { delta, .. } => match delta {
                Delta::Text { text } => ParsedLine::new(
                    vec![RichEvent::TextDelta {
                        text,
                        parent_agent_id: None,
                    }],
                    Coverage::Full,
                ),
                Delta::Thinking { thinking } => {
                    if !thinking.is_empty() {
                        ParsedLine::new(
                            vec![RichEvent::ThinkingBlock {
                                thinking,
                                parent_agent_id: None,
                            }],
                            Coverage::Full,
                        )
                    } else {
                        ParsedLine::new(Vec::new(), Coverage::Ignored)
                    }
                }
                _ => ParsedLine::new(Vec::new(), Coverage::Ignored),
            },
            _ => ParsedLine::new(Vec::new(), Coverage::Ignored),
        }
    }

    fn handle_result(&self, result: ResultEvent) -> ParsedLine {
        ParsedLine::new(
            vec![RichEvent::SessionResult {
                duration_ms: result.duration_ms.unwrap_or(0),
                cost_usd: result.total_cost_usd.unwrap_or(0.0),
                num_turns: result.num_turns.unwrap_or(0),
                is_error: result.is_error.unwrap_or(false),
                result_text: result.result,
            }],
            Coverage::Full,
        )
    }
}

/// Extract structured diff data from an Edit tool_use input.
fn extract_edit_diff(
    tool_use_id: &str,
    input: &serde_json::Value,
    parent: &Option<String>,
) -> Option<RichEvent> {
    let file_path = input.get("file_path")?.as_str()?.to_string();
    let old_string = input.get("old_string")?.as_str()?.to_string();
    let new_string = input.get("new_string")?.as_str()?.to_string();

    Some(RichEvent::EditDiff {
        tool_use_id: tool_use_id.to_string(),
        file_path,
        old_string,
        new_string,
        parent_agent_id: parent.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_assistant_with_edit() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01","name":"Edit","input":{"file_path":"src/main.rs","old_string":"fn old()","new_string":"fn new()","replace_all":false}}],"stop_reason":null},"parent_tool_use_id":null,"session_id":"abc"}"#;
        let mut parser = StreamParser::new();
        let events = parser.feed_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            RichEvent::EditDiff {
                file_path,
                old_string,
                new_string,
                ..
            } => {
                assert_eq!(file_path, "src/main.rs");
                assert_eq!(old_string, "fn old()");
                assert_eq!(new_string, "fn new()");
            }
            other => panic!("Expected EditDiff, got: {:?}", other),
        }
    }

    #[test]
    fn parse_subagent_tool_use() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_02","name":"Grep","input":{"pattern":"TODO","path":"/tmp"}}],"stop_reason":null},"parent_tool_use_id":"toolu_parent","session_id":"abc"}"#;
        let mut parser = StreamParser::new();
        let events = parser.feed_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            RichEvent::ToolUse {
                tool_name,
                parent_agent_id,
                ..
            } => {
                assert_eq!(tool_name, "Grep");
                assert_eq!(parent_agent_id.as_deref(), Some("toolu_parent"));
            }
            other => panic!("Expected ToolUse, got: {:?}", other),
        }
    }

    #[test]
    fn parse_result() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":5000,"num_turns":3,"total_cost_usd":0.05,"session_id":"abc","stop_reason":"end_turn"}"#;
        let mut parser = StreamParser::new();
        let events = parser.feed_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            RichEvent::SessionResult {
                duration_ms,
                cost_usd,
                num_turns,
                is_error,
                ..
            } => {
                assert_eq!(*duration_ms, 5000);
                assert!((cost_usd - 0.05).abs() < 0.001);
                assert_eq!(*num_turns, 3);
                assert!(!is_error);
            }
            other => panic!("Expected SessionResult, got: {:?}", other),
        }
    }

    #[test]
    fn unknown_type_is_captured_as_fallback() {
        let line = r#"{"type":"future_event_type","data":"whatever"}"#;
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        assert_eq!(parsed.coverage, Coverage::Fallback);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            RichEvent::Fallback { raw, reason, .. } => {
                assert!(raw.contains("future_event_type"));
                assert!(reason.contains("unknown top-level"));
            }
            other => panic!("Expected Fallback, got: {other:?}"),
        }
    }

    #[test]
    fn invalid_json_is_captured_as_fallback() {
        let line = "{not valid json";
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        assert_eq!(parsed.coverage, Coverage::Unparsed);
        assert_eq!(parsed.events.len(), 1);
        match &parsed.events[0] {
            RichEvent::Fallback { raw, .. } => assert_eq!(raw, line),
            other => panic!("Expected Fallback, got: {other:?}"),
        }
    }

    #[test]
    fn unrecognised_content_block_is_captured() {
        // A known assistant line carrying an unknown content-block type.
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"redacted_thinking","data":"xyz"}],"stop_reason":null},"parent_tool_use_id":null,"session_id":"abc"}"#;
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        assert_eq!(parsed.coverage, Coverage::Partial);
        // one text block + one fallback block
        assert_eq!(parsed.events.len(), 2);
        let has_fallback = parsed.events.iter().any(|e| {
            matches!(
                e,
                RichEvent::Fallback { raw, .. } if raw.contains("redacted_thinking")
            )
        });
        assert!(
            has_fallback,
            "expected the unknown block preserved as Fallback"
        );
    }

    #[test]
    fn rate_limit_is_ignored_not_dropped_silently() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"remaining":10}}"#;
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        assert_eq!(parsed.coverage, Coverage::Ignored);
        assert!(parsed.events.is_empty());
    }
}
