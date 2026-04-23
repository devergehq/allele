//! Transcript tailer — read-only consumer of Claude Code's session JSONL.
//!
//! Claude Code writes every user prompt, assistant message, thinking
//! block, tool call, tool result, and subagent turn to disk as it runs
//! interactively in the PTY. This module watches those files and
//! produces `TranscriptEvent`s for the Rich Sidecar to render.
//!
//! Layout on disk (per Claude Code, macOS/Linux):
//!   ~/.claude/projects/<dashed-cwd>/<session-uuid>.jsonl        ← main turns
//!   ~/.claude/projects/<dashed-cwd>/<session-uuid>/subagents/
//!     agent-<id>.jsonl                                          ← subagent turns
//!
//! `<dashed-cwd>` is the absolute working directory with every `/` and
//! `.` replaced by `-` (see `dash_cwd`).
//!
//! The tailer is poll-based: call `poll()` on an interval (~100ms works
//! well) and it returns only events appended since the last call. No
//! programmatic interaction with the `claude` binary happens here or
//! anywhere the sidecar touches.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::stream::{RichEvent, StreamParser};

/// Emitted for every new line parsed from a transcript.
pub enum TranscriptEvent {
    /// A plain-text user prompt (top-level `type:"user"` with string
    /// content). These precede the assistant turn they kick off.
    UserPrompt(String),
    /// Any event from the stream parser (text/thinking/tool_use/
    /// tool_result/edit_diff/session_result). Already carries a
    /// `parent_agent_id` stamp for subagent records.
    Rich(RichEvent),
}

/// Convert an absolute cwd path into Claude Code's dashed form used as
/// the project directory name under `~/.claude/projects/`.
///
/// Rule (verified empirically on the active machine): any non-alphanumeric
/// character (`/`, `.`, etc.) is mapped to `-`. A leading `/` therefore
/// becomes a leading `-`, and `.allele/` becomes `--allele-`.
pub fn dash_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Return the path where Claude Code WILL write (or is writing) this
/// session's JSONL, whether or not the file exists yet. The caller can
/// construct a `TranscriptTailer` from this path immediately — the
/// tailer silently no-ops on a missing file and picks up content from
/// byte 0 the moment `claude` creates it on its first turn.
pub fn expected_session_jsonl(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let dashed = dash_cwd(cwd);
    Some(
        home.join(".claude")
            .join("projects")
            .join(dashed)
            .join(format!("{session_id}.jsonl")),
    )
}

/// State for one watched JSONL file (main or subagent).
struct TailedFile {
    path: PathBuf,
    /// Byte offset of the next unread byte.
    offset: u64,
    /// Buffer for a partial last line (incomplete write).
    leftover: String,
    /// Per-file parser — keeps init/session state.
    parser: StreamParser,
    /// Agent id to stamp on sidechain events (for subagent files), or
    /// `None` for the main transcript.
    agent_id: Option<String>,
}

impl TailedFile {
    fn new(path: PathBuf, agent_id: Option<String>) -> Self {
        Self { path, offset: 0, leftover: String::new(), parser: StreamParser::new(), agent_id }
    }

    /// Read newly-appended bytes, split into lines, and return events.
    fn read_new(&mut self) -> Vec<TranscriptEvent> {
        let mut out = Vec::new();
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return out,
        };
        // Handle rotation/truncation: if the file is now shorter than
        // our last offset, reset to 0 and start over.
        let size = match file.metadata().map(|m| m.len()) {
            Ok(s) => s,
            Err(_) => return out,
        };
        if size < self.offset {
            self.offset = 0;
            self.leftover.clear();
            self.parser = StreamParser::new();
        }
        if size == self.offset { return out; }
        if file.seek(SeekFrom::Start(self.offset)).is_err() { return out; }
        let mut reader = BufReader::new(file);
        let mut line = std::mem::take(&mut self.leftover);
        loop {
            let prev_len = line.len();
            let read = match reader.read_line(&mut line) {
                Ok(n) => n,
                Err(_) => break,
            };
            if read == 0 { break; }
            if line.ends_with('\n') {
                let complete = line[..line.len() - 1].trim_end_matches('\r');
                self.offset += (line.len() - prev_len) as u64;
                self.process_line(complete, &mut out);
                line.clear();
            } else {
                // Incomplete tail — put it back and stop.
                self.leftover = line;
                return out;
            }
        }
        out
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<TranscriptEvent>) {
        if line.is_empty() { return; }

        // Fast path: detect top-level `type:"user"` with a plain string
        // `message.content` before handing to StreamParser. Those are
        // user prompts typed by the human; StreamParser ignores them.
        if let Some(prompt) = try_parse_user_prompt(line) {
            if !prompt.is_empty() {
                out.push(TranscriptEvent::UserPrompt(prompt));
                return;
            }
        }

        // Everything else: let StreamParser do the work. The JSONL
        // records are a superset of stream-json (extra uuid, parentUuid,
        // timestamp, isSidechain, sessionId fields are silently ignored
        // by #[serde(tag="type")] + non-deny_unknown_fields).
        for mut event in self.parser.feed_line(line) {
            if let Some(agent) = &self.agent_id {
                stamp_parent_agent(&mut event, agent.clone());
            }
            out.push(TranscriptEvent::Rich(event));
        }
    }
}

/// Minimal JSON sniff: is this a `type:"user"` line with a plain string
/// `message.content`? If so, return the string.
///
/// Kept as a targeted serde probe to avoid re-parsing the whole record
/// into a heavy struct just to pick off the user-prompt case.
fn try_parse_user_prompt(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Probe<'a> {
        #[serde(rename = "type")]
        ty: &'a str,
        message: Option<MessageBody>,
    }
    #[derive(serde::Deserialize)]
    struct MessageBody {
        content: serde_json::Value,
    }
    let probe: Probe = serde_json::from_str(line).ok()?;
    if probe.ty != "user" { return None; }
    let body = probe.message?;
    body.content.as_str().map(|s| s.to_string())
}

fn stamp_parent_agent(event: &mut RichEvent, agent: String) {
    let a = Some(agent);
    match event {
        RichEvent::TextDelta { parent_agent_id, .. }
        | RichEvent::TextBlock { parent_agent_id, .. }
        | RichEvent::ThinkingBlock { parent_agent_id, .. }
        | RichEvent::ToolUse { parent_agent_id, .. }
        | RichEvent::ToolResult { parent_agent_id, .. }
        | RichEvent::EditDiff { parent_agent_id, .. } => {
            *parent_agent_id = a;
        }
        RichEvent::Init { .. } | RichEvent::SessionResult { .. } | RichEvent::HookStatus { .. } => {}
    }
}

/// Watches a main session JSONL plus every `agent-*.jsonl` found under
/// its sibling `subagents/` directory. Call `poll()` on a timer.
pub struct TranscriptTailer {
    main: TailedFile,
    /// Path to the `<session>/subagents/` directory to scan on each poll.
    subagents_dir: PathBuf,
    /// Keyed by absolute subagent file path so re-polls are idempotent.
    subagents: HashMap<PathBuf, TailedFile>,
    /// Wall-clock time when this tailer was created. Subagent files last
    /// modified before this time are historical (from a previous agent
    /// invocation) and are skipped — otherwise they get appended after
    /// all current-session events and anchor the viewport in the past.
    created_at: std::time::SystemTime,
}

impl TranscriptTailer {
    /// Create a tailer for the JSONL at `session_jsonl`. The subagents
    /// directory is derived by stripping the `.jsonl` extension.
    pub fn new(session_jsonl: PathBuf) -> Self {
        let subagents_dir = session_jsonl.with_extension("").join("subagents");
        Self {
            main: TailedFile::new(session_jsonl, None),
            subagents_dir,
            subagents: HashMap::new(),
            created_at: std::time::SystemTime::now(),
        }
    }

    /// Return new events since the last call. Empty vec if nothing
    /// changed. Scans for new subagent files each call (they appear
    /// mid-session when the agent is first spawned).
    pub fn poll(&mut self) -> Vec<TranscriptEvent> {
        let mut out = self.main.read_new();

        // Discover subagent files (each `agent-<id>.jsonl` appears when
        // the agent's first turn is written).
        if self.subagents_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&self.subagents_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() { continue; }
                    if path.extension().and_then(|s| s.to_str()) != Some("jsonl") { continue; }
                    if self.subagents.contains_key(&path) { continue; }

                    // Skip subagent files that predate this tailer — they're
                    // from a previous agent invocation. If we can't determine
                    // the mtime, we include the file (safe default).
                    let is_current = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .map(|t| t > self.created_at)
                        .unwrap_or(true);
                    if !is_current { continue; }

                    let agent_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|stem| stem.strip_prefix("agent-"))
                        .unwrap_or("unknown")
                        .to_string();
                    self.subagents.insert(path.clone(), TailedFile::new(path, Some(agent_id)));
                }
            }
        }

        // Drain each known subagent file.
        for (_, tail) in self.subagents.iter_mut() {
            out.extend(tail.read_new());
        }
        out
    }

    #[allow(dead_code)]
    pub fn main_path(&self) -> &Path {
        &self.main.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_cwd_matches_claude_code_convention() {
        let p = Path::new("/Users/patrickdorival/.allele/workspaces/allele/e95d96e2");
        let d = dash_cwd(p);
        assert_eq!(d, "-Users-patrickdorival--allele-workspaces-allele-e95d96e2");
    }

    #[test]
    fn dash_cwd_simple_root() {
        assert_eq!(dash_cwd(Path::new("/foo/bar")), "-foo-bar");
    }

    #[test]
    fn user_prompt_probe_extracts_plain_string() {
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"isSidechain":false,"message":{"role":"user","content":"hello claude"},"timestamp":"2026-04-21T00:00:00Z"}"#;
        assert_eq!(try_parse_user_prompt(line).as_deref(), Some("hello claude"));
    }

    #[test]
    fn user_prompt_probe_rejects_tool_result_array() {
        // tool_result user turns have an array content, not a string
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        assert_eq!(try_parse_user_prompt(line), None);
    }

    #[test]
    fn user_prompt_probe_rejects_assistant() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#;
        assert_eq!(try_parse_user_prompt(line), None);
    }

    #[test]
    fn tailer_reads_appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":"first"}}
"#,
        ).unwrap();
        let mut tailer = TranscriptTailer::new(path.clone());
        let events = tailer.poll();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TranscriptEvent::UserPrompt(s) => assert_eq!(s, "first"),
            _ => panic!("expected UserPrompt"),
        }

        // Second poll with no changes — empty.
        let events = tailer.poll();
        assert!(events.is_empty());

        // Append another line.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"reply"}}]}}}}"#
        ).unwrap();
        drop(f);

        let events = tailer.poll();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TranscriptEvent::Rich(RichEvent::TextBlock { text, .. }) => assert_eq!(text, "reply"),
            _ => panic!("expected Rich TextBlock"),
        }
    }
}
