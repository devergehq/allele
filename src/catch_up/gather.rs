//! Building [`CatchUpFacts`] on this Mac (DEV-1353).
//!
//! This is the only place that reads the transcript, git or a live
//! [`Session`] for the catch-up strip. The parts in `parts.rs` are handed
//! what this produces and never reach for any of it, so DEV-1272 can feed
//! them a digest from another Mac instead.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::facts::{ChangeFacts, FileChange, PromptFacts, TerminalAsk, WantsNow};
use crate::session::{AttentionKind, Session, SessionStatus};

/// How much of the end of a transcript is read to find the last prompt.
/// A long turn full of tool output can push the prompt a long way back.
const TRANSCRIPT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Longest reply line kept. The strip shows one line; the rest is cut.
const REPLY_MAX_CHARS: usize = 300;

pub fn epoch_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the session wants from you, read from its live status.
pub fn wants_from_session(session: &Session) -> WantsNow {
    match session.status {
        SessionStatus::Running => WantsNow::Working {
            activity: session
                .last_pre_tool_use
                .as_ref()
                .and_then(|c| activity_phrase(&c.tool_name, c.tool_input.as_ref())),
            since: epoch_secs(session.status_since),
        },
        SessionStatus::AwaitingInput => match session.attention_context.as_ref() {
            Some(ctx) if ctx.kind == AttentionKind::Question => WantsNow::TerminalOnly {
                ask: if ctx.tool_name.as_deref() == Some("ExitPlanMode") {
                    TerminalAsk::Plan
                } else {
                    TerminalAsk::Question
                },
            },
            Some(ctx) if ctx.kind == AttentionKind::Permission => {
                let tool = ctx.tool_name.clone().unwrap_or_else(|| "a tool".into());
                match &ctx.request {
                    Some(req) => WantsNow::Permission {
                        tool,
                        purpose: req.purpose.clone(),
                        target: req.target.clone(),
                    },
                    None => WantsNow::Permission {
                        tool,
                        purpose: "use a tool".into(),
                        target: ctx.tool_input_summary.clone(),
                    },
                }
            }
            // The idle nudge, or no context at all: nothing is blocked.
            _ => WantsNow::Nothing {
                finished_at: Some(epoch_secs(session.status_since)),
            },
        },
        SessionStatus::ResponseReady => WantsNow::Nothing {
            finished_at: Some(epoch_secs(session.status_since)),
        },
        SessionStatus::Idle | SessionStatus::Suspended | SessionStatus::Done => {
            WantsNow::Nothing { finished_at: None }
        }
    }
}

/// "Running cargo build", "Editing src/main.rs": what the current tool is
/// doing, in words.
fn activity_phrase(tool: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let field = |k: &str| {
        input
            .and_then(|i| i.get(k))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let short = |p: String| {
        let parts: Vec<&str> = p.rsplitn(3, '/').collect();
        if parts.len() == 3 {
            format!("{}/{}", parts[1], parts[0])
        } else {
            p
        }
    };
    Some(match tool {
        "Bash" => {
            let cmd = field("command")?;
            format!("Running {}", truncate(super::facts::first_line(&cmd), 60))
        }
        "Edit" | "MultiEdit" => format!("Editing {}", short(field("file_path")?)),
        "Write" => format!("Writing {}", short(field("file_path")?)),
        "Read" => format!("Reading {}", short(field("file_path")?)),
        "Grep" | "Glob" => "Searching the code".into(),
        "WebFetch" | "WebSearch" => "Looking on the web".into(),
        "Task" | "Agent" => "Running a subagent".into(),
        other => format!("Using {other}"),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

/// Read the end of a Claude transcript and return the last prompt you typed
/// with one line of the latest reply after it.
pub fn read_last_exchange(path: &Path) -> Option<PromptFacts> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // A tail that starts mid-file starts mid-line; drop the fragment.
    let text = if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        &text
    };
    last_exchange(text.lines())
}

/// The last typed prompt and the latest assistant text after it.
pub fn last_exchange<'a>(lines: impl Iterator<Item = &'a str>) -> Option<PromptFacts> {
    let mut found: Option<PromptFacts> = None;
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                if let Some(text) = typed_prompt(&v) {
                    found = Some(PromptFacts {
                        text,
                        asked_at: v
                            .get("timestamp")
                            .and_then(|t| t.as_str())
                            .and_then(parse_rfc3339),
                        reply: None,
                    });
                }
            }
            Some("assistant") => {
                if let (Some(p), Some(reply)) = (found.as_mut(), assistant_line(&v)) {
                    p.reply = Some(reply);
                }
            }
            _ => {}
        }
    }
    found
}

/// A `type:"user"` record that is something you typed: not a tool result,
/// not Claude Code's own meta record, not a slash command's echo.
fn typed_prompt(v: &serde_json::Value) -> Option<String> {
    if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
        return None;
    }
    if v.get("isSidechain").and_then(|m| m.as_bool()) == Some(true) {
        return None;
    }
    let content = v.get("message")?.get("content")?;
    let text = if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        let blocks = content.as_array()?;
        if blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
        {
            return None;
        }
        blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let trimmed = text.trim();
    // `<command-name>`, `<local-command-stdout>`, `<system-reminder>`…
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    Some(trimmed.to_string())
}

/// The first line of an assistant record's text, if it has any.
fn assistant_line(v: &serde_json::Value) -> Option<String> {
    let blocks = v.get("message")?.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let line = reply_line(&text)?.replace("**", "");
    let line = line.trim();
    (!line.is_empty()).then(|| truncate(line, REPLY_MAX_CHARS))
}

/// The line of a reply worth showing: the first one that reads as a
/// sentence. A heading, or a line that is wholly bold, is a label a hook or
/// a style asked for ("**Classification: Trivial**"), not what Claude said.
/// A reply that is only labels so far has said nothing yet.
fn reply_line(text: &str) -> Option<&str> {
    let is_label =
        |l: &str| l.starts_with('#') || (l.len() > 4 && l.starts_with("**") && l.ends_with("**"));
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| !is_label(l))
        .map(|l| l.trim_start_matches(['>', '-', '*', ' ']))
}

/// Parse `2026-11-21T04:01:27.169Z` to Unix seconds. UTC only, which is all
/// Claude Code writes.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !s.ends_with('Z') || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec).ok()
}

/// What changed in `repo` between the baseline tree and now.
///
/// Returns the facts plus the current tree id. With no baseline there is
/// nothing to compare against, so the facts are `None`.
pub fn compute_changes(
    repo: &Path,
    baseline: Option<&str>,
) -> (Option<ChangeFacts>, Option<String>) {
    use crate::git::snapshot;
    let Some(now_tree) = snapshot::snapshot_worktree(repo) else {
        return (None, None);
    };
    let Some(baseline) = baseline else {
        return (None, Some(now_tree));
    };
    let facts = snapshot::tree_changes(repo, baseline, &now_tree).map(|changes| ChangeFacts {
        files: changes
            .into_iter()
            .map(|c| FileChange {
                path: c.path,
                added: c.stat.added,
                removed: c.stat.removed,
                binary: c.stat.binary,
            })
            .collect(),
        uncommitted: snapshot::head_tree(repo).as_deref() != Some(now_tree.as_str()),
    });
    (facts, Some(now_tree))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str, ts: &str) -> String {
        serde_json::json!({"type":"user","timestamp":ts,"message":{"role":"user","content":text}})
            .to_string()
    }
    fn assistant(text: &str) -> String {
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}})
            .to_string()
    }

    #[test]
    fn finds_the_last_prompt_and_its_latest_reply() {
        let lines = [
            user("first", "2026-11-21T04:00:00.000Z"),
            assistant("old reply"),
            user("When a session is discarded, move target/ to the Trash", "2026-11-21T04:01:27.169Z"),
            assistant("Looking at it."),
            serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}).to_string(),
            assistant("Done. Discarding now moves target/ to the Trash.\n\nMore detail."),
        ];
        let got = last_exchange(lines.iter().map(String::as_str)).expect("prompt");
        assert_eq!(
            got.text,
            "When a session is discarded, move target/ to the Trash"
        );
        assert_eq!(
            got.reply.as_deref(),
            Some("Done. Discarding now moves target/ to the Trash.")
        );
        assert_eq!(got.asked_at, parse_rfc3339("2026-11-21T04:01:27.169Z"));
    }

    #[test]
    fn a_label_line_is_not_the_reply() {
        let lines = [
            user("go", "2026-11-21T04:00:00Z"),
            assistant("**Classification: Trivial**\n\nThe command passed but ran 0 tests."),
        ];
        let got = last_exchange(lines.iter().map(String::as_str)).expect("prompt");
        assert_eq!(
            got.reply.as_deref(),
            Some("The command passed but ran 0 tests.")
        );
    }

    #[test]
    fn a_reply_of_only_a_label_is_no_reply_yet() {
        let lines = [
            user("go", "2026-11-21T04:00:00Z"),
            assistant("**Classification: Trivial**"),
        ];
        let got = last_exchange(lines.iter().map(String::as_str)).expect("prompt");
        assert_eq!(got.reply, None);
    }

    #[test]
    fn a_prompt_with_no_reply_yet_has_none() {
        let lines = [assistant("before"), user("go", "2026-11-21T04:00:00Z")];
        let got = last_exchange(lines.iter().map(String::as_str)).expect("prompt");
        assert_eq!(got.reply, None);
    }

    #[test]
    fn meta_records_and_command_echoes_are_not_prompts() {
        let meta = serde_json::json!({"type":"user","isMeta":true,"message":{"content":"Caveat"}})
            .to_string();
        let cmd = user(
            "<command-name>/clear</command-name>",
            "2026-11-21T04:00:00Z",
        );
        let lines = [user("real", "2026-11-21T04:00:00Z"), meta, cmd];
        let got = last_exchange(lines.iter().map(String::as_str)).expect("prompt");
        assert_eq!(got.text, "real");
    }

    #[test]
    fn no_prompt_at_all_is_none() {
        let lines = [assistant("hello")];
        assert!(last_exchange(lines.iter().map(String::as_str)).is_none());
    }

    fn session_in(status: SessionStatus, tool: Option<&str>, kind: AttentionKind) -> Session {
        let now = SystemTime::now();
        let mut s = Session::suspended_from_persisted(
            "a".into(),
            "a".into(),
            now,
            now,
            std::time::Duration::ZERO,
            None,
        );
        s.status = status;
        let input = serde_json::json!({"command":"cargo test -p allele clone::trash"});
        s.attention_context = Some(crate::session::AttentionContext {
            kind,
            tool_name: tool.map(str::to_string),
            tool_input_summary: None,
            message: None,
            request: tool
                .map(|t| crate::rich::permissions::PermissionRequest::from_tool(t, &input)),
        });
        s
    }

    #[test]
    fn a_permission_prompt_is_a_permission_row_with_the_whole_command() {
        let s = session_in(
            SessionStatus::AwaitingInput,
            Some("Bash"),
            AttentionKind::Permission,
        );
        match wants_from_session(&s) {
            WantsNow::Permission { tool, target, .. } => {
                assert_eq!(tool, "Bash");
                assert_eq!(target.as_deref(), Some("cargo test -p allele clone::trash"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plan_approval_is_terminal_only() {
        let s = session_in(
            SessionStatus::AwaitingInput,
            Some("ExitPlanMode"),
            AttentionKind::Question,
        );
        assert_eq!(
            wants_from_session(&s),
            WantsNow::TerminalOnly {
                ask: TerminalAsk::Plan
            }
        );
    }

    #[test]
    fn a_question_is_terminal_only_too() {
        let s = session_in(
            SessionStatus::AwaitingInput,
            Some("AskUserQuestion"),
            AttentionKind::Question,
        );
        assert_eq!(
            wants_from_session(&s),
            WantsNow::TerminalOnly {
                ask: TerminalAsk::Question
            }
        );
    }

    #[test]
    fn the_idle_nudge_wants_nothing() {
        let s = session_in(SessionStatus::AwaitingInput, None, AttentionKind::Idle);
        assert!(matches!(
            wants_from_session(&s),
            WantsNow::Nothing {
                finished_at: Some(_)
            }
        ));
    }

    #[test]
    fn a_finished_turn_wants_nothing_and_says_when() {
        let s = session_in(SessionStatus::ResponseReady, None, AttentionKind::Idle);
        assert!(matches!(
            wants_from_session(&s),
            WantsNow::Nothing {
                finished_at: Some(_)
            }
        ));
    }

    #[test]
    fn a_running_session_is_working_with_its_current_tool() {
        let mut s = session_in(SessionStatus::Running, None, AttentionKind::Idle);
        s.last_pre_tool_use = Some(crate::session::PreToolUseContext {
            tool_name: "Bash".into(),
            tool_input: Some(serde_json::json!({"command":"cargo build"})),
        });
        match wants_from_session(&s) {
            WantsNow::Working { activity, .. } => {
                assert_eq!(activity.as_deref(), Some("Running cargo build"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rfc3339_parses_to_epoch() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00.000Z"), Some(951_868_800));
        assert_eq!(parse_rfc3339("not a time"), None);
    }

    #[test]
    fn activity_reads_the_command() {
        let input = serde_json::json!({"command":"cargo build --release","description":"Build"});
        assert_eq!(
            activity_phrase("Bash", Some(&input)).as_deref(),
            Some("Running cargo build --release")
        );
        let input = serde_json::json!({"file_path":"/a/b/src/clone/mod.rs"});
        assert_eq!(
            activity_phrase("Edit", Some(&input)).as_deref(),
            Some("Editing clone/mod.rs")
        );
    }

    #[test]
    fn changes_need_a_baseline() {
        let dir = tempfile::TempDir::new().expect("tmp");
        std::fs::write(dir.path().join("f"), "a\n").expect("w");
        crate::git::git_init(dir.path()).expect("init");
        let (facts, tree) = compute_changes(dir.path(), None);
        assert!(facts.is_none());
        let tree = tree.expect("tree");

        std::fs::write(dir.path().join("f"), "a\nb\n").expect("w");
        let (facts, _) = compute_changes(dir.path(), Some(&tree));
        let facts = facts.expect("facts");
        assert_eq!(facts.files.len(), 1);
        assert_eq!((facts.files[0].added, facts.files[0].removed), (1, 0));
        assert!(facts.uncommitted);
    }

    #[test]
    fn nothing_changed_is_an_empty_summary_not_none() {
        let dir = tempfile::TempDir::new().expect("tmp");
        std::fs::write(dir.path().join("f"), "a\n").expect("w");
        crate::git::git_init(dir.path()).expect("init");
        let (_, tree) = compute_changes(dir.path(), None);
        let (facts, _) = compute_changes(dir.path(), tree.as_deref());
        assert!(facts.expect("facts").is_empty());
    }
}
