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
    pub fn new(events: Vec<RichEvent>, coverage: Coverage) -> Self {
        Self {
            events,
            coverage,
            diagnostics: Vec::new(),
        }
    }

    /// Fully-covered line: every part normalised, no diagnostics.
    pub fn full(events: Vec<RichEvent>) -> Self {
        Self::new(events, Coverage::Full)
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
    #[allow(dead_code)]
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
            StreamLine::SessionMetadata => ParsedLine::new(Vec::new(), Coverage::Ignored),
            StreamLine::PrLink(pr) => self.handle_pr_link(pr),
            StreamLine::FrameLink(frame) => self.handle_frame_link(frame),
            StreamLine::QueueOperation(op) => self.handle_queue_operation(op),
            StreamLine::Attachment(att) => self.handle_attachment(att),
            StreamLine::Unknown => {
                // An event type we don't model. Emit nothing: the ledger has
                // already retained the exact raw line, so rendering it buys no
                // information and costs readability — unmodelled types are ~40%
                // of a session file (DEV-321). Coverage stays `Fallback` and the
                // diagnostic is still recorded, so parser gaps remain reportable
                // (DEV-322) without polluting the reading view.
                let reason = "unknown top-level event type".to_string();
                let mut pl = ParsedLine::new(Vec::new(), Coverage::Fallback);
                pl.diagnostics.push(reason);
                pl
            }
        }
    }

    fn handle_pr_link(&self, pr: PrLinkEvent) -> ParsedLine {
        let label = match (pr.pr_number, &pr.pr_repository) {
            (Some(n), Some(repo)) => format!("pull request {repo}#{n}"),
            (Some(n), None) => format!("pull request #{n}"),
            (None, Some(repo)) => format!("pull request on {repo}"),
            (None, None) => "pull request".to_string(),
        };
        ParsedLine::full(vec![RichEvent::Notice {
            kind: NoticeKind::PullRequest,
            text: label,
            link: pr.pr_url,
            parent_agent_id: None,
        }])
    }

    fn handle_frame_link(&self, frame: FrameLinkEvent) -> ParsedLine {
        let title = frame.title.unwrap_or_else(|| "artifact".to_string());
        ParsedLine::full(vec![RichEvent::Notice {
            kind: NoticeKind::Artifact,
            text: title,
            link: frame.frame_url.or(frame.path),
            parent_agent_id: None,
        }])
    }

    fn handle_queue_operation(&self, op: QueueOperationEvent) -> ParsedLine {
        // Only enqueues are interesting; dequeue/remove are the bookkeeping
        // half of the same action and would double every entry.
        if op.operation.as_deref() != Some("enqueue") {
            return ParsedLine::new(Vec::new(), Coverage::Ignored);
        }
        let content = op.content.unwrap_or_default();
        let summary = first_line_truncated(&content, 120);
        if summary.is_empty() {
            return ParsedLine::new(Vec::new(), Coverage::Ignored);
        }
        ParsedLine::full(vec![RichEvent::Notice {
            kind: NoticeKind::Queued,
            text: summary,
            link: None,
            parent_agent_id: None,
        }])
    }

    fn handle_attachment(&self, att: AttachmentEvent) -> ParsedLine {
        let Some(attachment) = att.attachment else {
            return ParsedLine::new(Vec::new(), Coverage::Ignored);
        };
        let notice = match attachment {
            Attachment::EditedTextFile { filename } => Some((
                NoticeKind::FileEdited,
                format!("edited outside Claude: {}", basename(filename.as_deref())),
                filename,
            )),
            Attachment::File {
                filename,
                display_path,
            } => {
                let shown = display_path
                    .clone()
                    .unwrap_or_else(|| basename(filename.as_deref()));
                Some((
                    NoticeKind::FileAttached,
                    format!("attached {shown}"),
                    filename,
                ))
            }
            Attachment::PlanModeExit { plan_file_path } => Some((
                NoticeKind::PlanAccepted,
                "plan accepted".to_string(),
                plan_file_path,
            )),
            Attachment::HookBlockingError {
                hook_name,
                blocking_error,
            }
            | Attachment::HookNonBlockingError {
                hook_name,
                blocking_error,
            } => {
                let hook = hook_name.unwrap_or_else(|| "hook".to_string());
                let detail = hook_error_detail(blocking_error.as_ref());
                let text = match detail {
                    Some(d) => format!("{hook} hook error: {d}"),
                    None => format!("{hook} hook error"),
                };
                Some((NoticeKind::HookError, text, None))
            }
            // Context plumbing — task reminders, skill/agent/MCP listings, hook
            // success, date changes. Retained by the ledger, rendered nowhere.
            Attachment::Other => None,
        };

        match notice {
            Some((kind, text, link)) => ParsedLine::full(vec![RichEvent::Notice {
                kind,
                text,
                link,
                parent_agent_id: None,
            }]),
            None => ParsedLine::new(Vec::new(), Coverage::Ignored),
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
                // A hook response may carry a producer status frame as an
                // `agent.status` key alongside its own payload (DEV-514). That
                // typed channel is what replaced parsing phase names out of
                // prose, so it is read before the coarse status classification.
                let mut events = Vec::new();
                if let Some(outcome) = sys
                    .stdout
                    .as_deref()
                    .and_then(crate::rich::locus_status::parse_stdout)
                {
                    events.push(RichEvent::AgentStatus { outcome });
                }

                // Surface hook events that indicate status changes
                if let (Some(event), Some(name)) = (sys.hook_event, sys.hook_name) {
                    match event.as_str() {
                        "PreToolUse" | "PostToolUse" | "Notification" | "Stop" => {
                            events.push(RichEvent::HookStatus {
                                hook_event: event,
                                hook_name: name,
                            });
                            ParsedLine::new(events, Coverage::Full)
                        }
                        _ => coverage_for_status_only(events),
                    }
                } else {
                    coverage_for_status_only(events)
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

/// Final path component, for a compact notice label.
fn basename(path: Option<&str>) -> String {
    let Some(path) = path else {
        return "file".to_string();
    };
    path.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// First non-empty line of `text`, truncated to `max` chars.
fn first_line_truncated(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    let Some(line) = line else {
        return String::new();
    };
    if line.chars().count() <= max {
        return line.to_string();
    }
    let head: String = line.chars().take(max).collect();
    format!("{head}…")
}

/// Pull a human-readable message out of a hook error payload, which is an
/// object in the samples we have but may also be a bare string.
fn hook_error_detail(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    let raw = match value {
        serde_json::Value::String(s) => s.as_str(),
        other => other
            .get("blockingError")
            .or_else(|| other.get("nonBlockingError"))
            .or_else(|| other.get("message"))
            .and_then(|v| v.as_str())?,
    };
    let summary = first_line_truncated(raw, 120);
    (!summary.is_empty()).then_some(summary)
}

/// Extract structured diff data from an Edit tool_use input.
/// A hook response that carried nothing but a status frame is still covered —
/// returning `Ignored` would count the line as unhandled.
fn coverage_for_status_only(events: Vec<RichEvent>) -> ParsedLine {
    if events.is_empty() {
        ParsedLine::new(Vec::new(), Coverage::Ignored)
    } else {
        ParsedLine::new(events, Coverage::Full)
    }
}

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
    use crate::rich::locus_status::{FrameOutcome, RejectReason};

    #[test]
    fn hook_stdout_yields_a_typed_status_frame() {
        // The end-to-end seam: a Locus PreToolUse response carrying its
        // delegation-guardrail `decision` object AND an `agent.status` key in
        // the same JSON object, as the contract requires (DEV-514).
        let stdout = r#"{"decision":{"permissionDecision":"deny"},"agent.status":{"contract":"agent.status/1","run":{"stage":{"label":"BUILD","ordinal":4,"total":7}},"usage":{"cost_usd":0.42}}}"#;
        let line = serde_json::json!({
            "type": "system",
            "subtype": "hook_response",
            "hook_event": "PreToolUse",
            "hook_name": "locus-guardrail",
            "stdout": stdout,
        })
        .to_string();

        let mut parser = StreamParser::new();
        let events = parser.feed_line(&line);
        let frame = events
            .iter()
            .find_map(|e| match e {
                RichEvent::AgentStatus {
                    outcome: FrameOutcome::Accepted(f),
                } => Some(f),
                _ => None,
            })
            .expect("a status frame among the emitted events");
        assert_eq!(
            frame
                .run
                .stage
                .as_ref()
                .and_then(|s| s.display_label())
                .as_deref(),
            Some("BUILD")
        );
        assert_eq!(frame.usage.cost_usd, Some(0.42));
        // The hook's own status classification is not displaced by the frame.
        assert!(events
            .iter()
            .any(|e| matches!(e, RichEvent::HookStatus { .. })));
    }

    #[test]
    fn hook_stdout_with_a_future_contract_degrades_visibly() {
        let stdout = r#"{"agent.status":{"contract":"agent.status/2"}}"#;
        let line = serde_json::json!({
            "type": "system",
            "subtype": "hook_response",
            "hook_event": "SessionStart",
            "hook_name": "locus",
            "stdout": stdout,
        })
        .to_string();

        let mut parser = StreamParser::new();
        let events = parser.feed_line(&line);
        // SessionStart isn't a status-change hook, so the frame is the ONLY
        // reason this line produces anything — it must still get through.
        assert!(matches!(
            events.as_slice(),
            [RichEvent::AgentStatus {
                outcome: FrameOutcome::Unsupported(RejectReason::UnsupportedMajor { .. })
            }]
        ));
    }

    #[test]
    fn hook_response_without_a_frame_is_unaffected() {
        let line = serde_json::json!({
            "type": "system",
            "subtype": "hook_response",
            "hook_event": "PostToolUse",
            "hook_name": "locus",
            "stdout": r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse"}}"#,
        })
        .to_string();
        let mut parser = StreamParser::new();
        let events = parser.feed_line(&line);
        assert!(events
            .iter()
            .all(|e| !matches!(e, RichEvent::AgentStatus { .. })));
    }

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
    fn unknown_type_renders_nothing_but_is_still_counted() {
        // DEV-321: unmodelled types must not reach the reading view — the
        // ledger already retains the raw line — but coverage stays `Fallback`
        // and the diagnostic survives so parser gaps remain reportable.
        let line = r#"{"type":"future_event_type","data":"whatever"}"#;
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        assert_eq!(parsed.coverage, Coverage::Fallback);
        assert!(
            parsed.events.is_empty(),
            "unknown types must emit no renderable event, got {:?}",
            parsed.events
        );
        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].contains("unknown top-level"));
    }

    #[test]
    fn session_metadata_types_are_ignored() {
        // The bulk of a JSONL session file: titles, modes, agent names,
        // file-history bookkeeping. None of it belongs in a reading view.
        let lines = [
            r#"{"type":"custom-title","customTitle":"Claude 22","sessionId":"s1"}"#,
            r#"{"type":"agent-name","agentName":"Claude 22","sessionId":"s1"}"#,
            r#"{"type":"permission-mode","permissionMode":"acceptEdits","sessionId":"s1"}"#,
            r#"{"type":"mode","mode":"normal","sessionId":"s1"}"#,
            r#"{"type":"last-prompt","leafUuid":"u1","sessionId":"s1"}"#,
            r#"{"type":"ai-title","aiTitle":"Some title","sessionId":"s1"}"#,
            r#"{"type":"bridge-session","sessionId":"s1","bridgeSessionId":"cse_1"}"#,
            r#"{"type":"fork-context-ref","agentId":"a1","parentSessionId":"s0"}"#,
            r#"{"type":"file-history-snapshot","messageId":"m1","snapshot":{}}"#,
            r#"{"type":"file-history-delta","messageId":"m1","trackingPath":"/tmp/x"}"#,
        ];
        let mut parser = StreamParser::new();
        for line in lines {
            let parsed = parser.feed_line_detailed(line);
            assert_eq!(parsed.coverage, Coverage::Ignored, "line: {line}");
            assert!(parsed.events.is_empty(), "line: {line}");
        }
    }

    #[test]
    fn pr_link_becomes_a_notice() {
        let line = r#"{"type":"pr-link","sessionId":"s1","prNumber":8687,"prUrl":"https://github.com/o/r/pull/8687","prRepository":"o/r","timestamp":"2026-07-26T00:41:28.086Z"}"#;
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        assert_eq!(parsed.coverage, Coverage::Full);
        match &parsed.events[..] {
            [RichEvent::Notice {
                kind, text, link, ..
            }] => {
                assert_eq!(*kind, NoticeKind::PullRequest);
                assert_eq!(text, "pull request o/r#8687");
                assert_eq!(link.as_deref(), Some("https://github.com/o/r/pull/8687"));
            }
            other => panic!("expected one PullRequest notice, got {other:?}"),
        }
    }

    /// Replays every JSONL session file on this machine through the parser and
    /// prints a coverage census. Ignored by default because it depends on local
    /// data; run it after a Claude Code upgrade to find newly-shipped event
    /// types before they reach users:
    ///
    ///   cargo test -- --ignored --nocapture replay_local_corpus
    ///
    /// Superseded as a shipped feature by DEV-322.
    #[test]
    #[ignore = "reads the developer's local ~/.claude/projects corpus"]
    fn replay_local_corpus() {
        use std::collections::BTreeMap;
        use std::io::BufRead;

        let Some(root) = dirs::home_dir().map(|h| h.join(".claude").join("projects")) else {
            eprintln!("no home directory; skipping");
            return;
        };
        let mut files = Vec::new();
        collect_jsonl(&root, &mut files);
        if files.is_empty() {
            eprintln!("no JSONL files under {}; skipping", root.display());
            return;
        }

        let (mut total, mut rendered, mut unknown) = (0u64, 0u64, 0u64);
        let mut unknown_types: BTreeMap<String, u64> = BTreeMap::new();
        let mut notices: BTreeMap<String, u64> = BTreeMap::new();
        for path in &files {
            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            let mut parser = StreamParser::new();
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                total += 1;
                let parsed = parser.feed_line_detailed(&line);
                rendered += parsed.events.len() as u64;
                for event in &parsed.events {
                    if let RichEvent::Notice { kind, .. } = event {
                        *notices.entry(format!("{kind:?}")).or_default() += 1;
                    }
                }
                if parsed.coverage == Coverage::Fallback {
                    unknown += 1;
                    let ty = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                        .unwrap_or_else(|| "<none>".into());
                    *unknown_types.entry(ty).or_default() += 1;
                }
            }
        }

        eprintln!("files={}  lines={total}  events={rendered}", files.len());
        eprintln!(
            "unmodelled lines: {unknown} ({:.2}%)",
            100.0 * unknown as f64 / total.max(1) as f64
        );
        for (ty, n) in &unknown_types {
            eprintln!("  {ty}: {n}");
        }
        eprintln!("notices emitted:");
        for (kind, n) in &notices {
            eprintln!("  {kind}: {n}");
        }

        // The whole point of DEV-321: no unmodelled line may reach the view.
        let fallbacks_rendered = unknown_types.is_empty();
        assert!(
            fallbacks_rendered || unknown < total,
            "sanity: not every line can be unmodelled"
        );
    }

    fn collect_jsonl(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_jsonl(&path, out);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }

    #[test]
    fn frame_link_becomes_an_artifact_notice() {
        let line = r#"{"type":"frame-link","sessionId":"s1","path":"/tmp/a.html","frameUrl":"https://claude.ai/code/artifact/x","title":"RN vs Capacitor"}"#;
        let mut parser = StreamParser::new();
        let parsed = parser.feed_line_detailed(line);
        match &parsed.events[..] {
            [RichEvent::Notice {
                kind, text, link, ..
            }] => {
                assert_eq!(*kind, NoticeKind::Artifact);
                assert_eq!(text, "RN vs Capacitor");
                assert_eq!(link.as_deref(), Some("https://claude.ai/code/artifact/x"));
            }
            other => panic!("expected one Artifact notice, got {other:?}"),
        }
    }

    #[test]
    fn only_enqueue_queue_operations_surface() {
        let mut parser = StreamParser::new();
        let enqueue = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"s1","content":"run the tests\nthen report"}"#;
        match &parser.feed_line_detailed(enqueue).events[..] {
            [RichEvent::Notice { kind, text, .. }] => {
                assert_eq!(*kind, NoticeKind::Queued);
                assert_eq!(text, "run the tests");
            }
            other => panic!("expected a Queued notice, got {other:?}"),
        }
        // The dequeue half of the same action would double every entry.
        let dequeue = r#"{"type":"queue-operation","operation":"dequeue","sessionId":"s1","content":"run the tests"}"#;
        let parsed = parser.feed_line_detailed(dequeue);
        assert_eq!(parsed.coverage, Coverage::Ignored);
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn signal_bearing_attachments_become_notices() {
        let cases: [(&str, NoticeKind, &str); 4] = [
            (
                r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"/a/b/Foo.php","snippet":"1\t<?php"}}"#,
                NoticeKind::FileEdited,
                "edited outside Claude: Foo.php",
            ),
            (
                r#"{"type":"attachment","attachment":{"type":"file","filename":"/a/.storybook/preview.js","content":"x","displayPath":".storybook/preview.js"}}"#,
                NoticeKind::FileAttached,
                "attached .storybook/preview.js",
            ),
            (
                r#"{"type":"attachment","attachment":{"type":"plan_mode_exit","planFilePath":"/p/plan.md","planExists":false}}"#,
                NoticeKind::PlanAccepted,
                "plan accepted",
            ),
            (
                r#"{"type":"attachment","attachment":{"type":"hook_blocking_error","hookName":"Stop","hookEvent":"Stop","blockingError":{"blockingError":"I want to interrogate an idea.","command":"/bin/hook"}}}"#,
                NoticeKind::HookError,
                "Stop hook error: I want to interrogate an idea.",
            ),
        ];
        let mut parser = StreamParser::new();
        for (line, want_kind, want_text) in cases {
            match &parser.feed_line_detailed(line).events[..] {
                [RichEvent::Notice { kind, text, .. }] => {
                    assert_eq!(*kind, want_kind, "line: {line}");
                    assert_eq!(text, want_text, "line: {line}");
                }
                other => panic!("expected one notice for {line}, got {other:?}"),
            }
        }
    }

    #[test]
    fn plumbing_attachments_are_ignored_not_fallback() {
        // Unrecognised subtypes must degrade to Ignored — never fail the whole
        // line into the unknown-type path.
        let lines = [
            r#"{"type":"attachment","attachment":{"type":"task_reminder","content":"..."}}"#,
            r#"{"type":"attachment","attachment":{"type":"skill_listing","skills":[]}}"#,
            r#"{"type":"attachment","attachment":{"type":"some_future_subtype","x":1}}"#,
            r#"{"type":"attachment"}"#,
        ];
        let mut parser = StreamParser::new();
        for line in lines {
            let parsed = parser.feed_line_detailed(line);
            assert_eq!(parsed.coverage, Coverage::Ignored, "line: {line}");
            assert!(parsed.events.is_empty(), "line: {line}");
        }
    }

    #[test]
    fn hook_error_accepts_a_bare_string_payload() {
        // Guards the shape assumption: our samples nest the message in an
        // object, but a bare string must not break parsing.
        let line = r#"{"type":"attachment","attachment":{"type":"hook_blocking_error","hookName":"PreToolUse","blockingError":"denied by policy"}}"#;
        let mut parser = StreamParser::new();
        match &parser.feed_line_detailed(line).events[..] {
            [RichEvent::Notice { kind, text, .. }] => {
                assert_eq!(*kind, NoticeKind::HookError);
                assert_eq!(text, "PreToolUse hook error: denied by policy");
            }
            other => panic!("expected a HookError notice, got {other:?}"),
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
