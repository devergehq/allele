//! Conversation discovery — every Claude Code conversation belonging to one
//! Allele workspace, plus enough of each to tell them apart.
//!
//! A workspace is *not* one conversation. Claude Code rotates to a fresh
//! `<uuid>.jsonl` on `/clear` and on some compactions, and a user can start a
//! stray `claude` run in the same directory. All of those land side by side in
//! `~/.claude/projects/<dashed-cwd>/`.
//!
//! Allele caches the live conversation as [`Session::claude_session_id`], learned
//! from hook events. That cache goes stale whenever a rotation happens while
//! Allele isn't running, and resume then replays an *older* conversation with no
//! visible error — the old `.jsonl` is still on disk, so every existence check
//! passes. This module exists so resume can consult the filesystem, which is
//! ground truth, instead of trusting the cache.
//!
//! Read-only: nothing here writes to or interprets Claude Code's state beyond
//! reading the transcripts it already leaves on disk.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Bytes of transcript tail scanned for the last human prompt.
///
/// Sized against the local corpus of 248 real transcripts: a 256 KB tail finds a
/// prompt in only 90.3% of them (a long autonomous run can push the last human
/// turn far from the end), 1 MB reaches 98.8%, and reading the whole file gains
/// just 0.4 points more. 1 MB is the knee of that curve.
const TAIL_SCAN_BYTES: u64 = 1024 * 1024;

/// Leading lines scanned for sidecar metadata records (`custom-title` and
/// friends sit in the first handful of lines).
const HEAD_SCAN_LINES: usize = 64;

/// Longest preview rendered in the picker before ellipsis.
pub(crate) const PREVIEW_MAX_CHARS: usize = 180;

/// One conversation transcript belonging to a workspace.
#[derive(Debug, Clone)]
pub(crate) struct Conversation {
    /// The conversation UUID — the value to pass to `claude --resume`.
    pub id: String,
    pub path: PathBuf,
    /// Last write to the transcript. The primary recency signal.
    pub modified: SystemTime,
    /// Transcript size. Free from the directory metadata, and a useful second
    /// differentiator when two forks show similar times and previews.
    pub size_bytes: u64,
    /// `custom-title` sidecar record, when Claude Code has titled the session.
    /// Forks inherit their parent's title, so this does **not** discriminate
    /// between two branches of one lineage — see [`Conversation::last_prompt`].
    pub title: Option<String>,
    /// The last human-typed prompt. This is the field that actually tells two
    /// forks apart: they share an opening message, a uuid history and a title,
    /// and diverge only at the tail.
    pub last_prompt: Option<String>,
}

impl Conversation {
    /// Text to show the user for this conversation, best available.
    pub fn preview(&self) -> Option<&str> {
        self.last_prompt.as_deref().or(self.title.as_deref())
    }
}

/// The `~/.claude/projects/<dashed-cwd>` directory backing a workspace.
pub(crate) fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(crate::transcript::dash_cwd(cwd)),
    )
}

/// Every conversation in `cwd`'s project directory, newest write first.
///
/// Metadata only — no transcript bytes are read, so this is cheap enough to run
/// on every resume. Call [`load_previews`] to fill in `title`/`last_prompt` for
/// the few conversations actually shown.
///
/// Subagent transcripts live in a `<session>/subagents/` subdirectory and are
/// skipped for free by taking regular files only.
pub(crate) fn list(cwd: &Path) -> Vec<Conversation> {
    match project_dir(cwd) {
        Some(dir) => list_in_dir(&dir),
        None => Vec::new(),
    }
}

/// [`list`] against an explicit project directory.
pub(crate) fn list_in_dir(dir: &Path) -> Vec<Conversation> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut out: Vec<Conversation> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(Conversation {
                id: path.file_stem()?.to_str()?.to_string(),
                path,
                modified: meta.modified().ok()?,
                size_bytes: meta.len(),
                title: None,
                last_prompt: None,
            })
        })
        .collect();

    // Newest first. Ties keep a stable order by id so the picker doesn't
    // reshuffle between renders.
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Fill in `title` and `last_prompt` by reading each transcript's head and tail.
pub(crate) fn load_previews(conversations: &mut [Conversation]) {
    for c in conversations.iter_mut() {
        c.title = read_title(&c.path);
        c.last_prompt = read_last_prompt(&c.path);
    }
}

/// Repair a conversation pointer whose transcript has vanished, returning the
/// id to adopt or `None` to leave the pointer alone.
///
/// Deliberately narrow. It fires only when the conversation Allele believes it
/// owns is **gone** from disk while siblings remain — there is nothing to choose
/// between, so choosing silently is safe. When the pointed-to transcript still
/// exists, a newer sibling is a genuine ambiguity that belongs to the picker,
/// not to a silent rewrite: adopting the newest there would quietly abandon a
/// conversation the user may well have wanted.
pub(crate) fn reconcile_missing(cwd: &Path, resolved: &str) -> Option<String> {
    match project_dir(cwd) {
        Some(dir) => reconcile_missing_in_dir(&dir, resolved),
        None => None,
    }
}

/// [`reconcile_missing`] against an explicit project directory.
pub(crate) fn reconcile_missing_in_dir(dir: &Path, resolved: &str) -> Option<String> {
    let all = list_in_dir(dir);
    if all.is_empty() || all.iter().any(|c| c.id == resolved) {
        return None;
    }
    all.into_iter().next().map(|c| c.id)
}

/// Repair a rehydrated session whose Claude conversation has vanished from
/// disk, adopting the newest surviving one in its workspace.
///
/// Scoped to sessions that positively learned a conversation id: with no
/// pointer there is no way to tell "this agent writes no transcripts" from
/// "the transcript is gone", and guessing there could adopt an unrelated
/// conversation. The genuinely ambiguous case — pointer intact but superseded
/// — belongs to the picker, not to a silent rewrite.
pub(crate) fn repair_session_pointer(session: &mut crate::session::Session) {
    if session.claude_session_id.is_none() {
        return;
    }
    let Some(clone_path) = session.clone_path.clone() else {
        return;
    };
    let Some(adopted) = reconcile_missing(&clone_path, session.claude_session_id()) else {
        return;
    };
    tracing::info!(
        "startup: session {} conversation {} is gone; adopting {}",
        session.id,
        session.claude_session_id(),
        adopted
    );
    session.claude_session_id = Some(adopted).filter(|c| c != &session.id);
}

/// Human-readable transcript size, e.g. "4.0 MB".
pub(crate) fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", (b / KB).max(1.0))
    }
}

/// Whether resuming `cwd` is ambiguous: more than one conversation exists and
/// `resolved` — the id resume would otherwise use — is not the newest one.
///
/// A single conversation, or a pointer that already names the newest, resumes
/// silently as before.
pub(crate) fn resume_is_ambiguous(cwd: &Path, resolved: &str) -> bool {
    match project_dir(cwd) {
        Some(dir) => ambiguous_in_dir(&dir, resolved),
        None => false,
    }
}

/// [`resume_is_ambiguous`] against an explicit project directory.
pub(crate) fn ambiguous_in_dir(dir: &Path, resolved: &str) -> bool {
    let all = list_in_dir(dir);
    all.len() > 1 && all.first().map(|c| c.id.as_str()) != Some(resolved)
}

/// Read the leading sidecar records for a `custom-title`.
fn read_title(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(HEAD_SCAN_LINES).map_while(Result::ok) {
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("type").and_then(|t| t.as_str()) == Some("custom-title") {
            let title = value.get("customTitle")?.as_str()?.trim();
            if !title.is_empty() {
                return Some(truncate(title, PREVIEW_MAX_CHARS));
            }
        }
    }
    None
}

/// Scan the transcript tail backwards for the last human-typed prompt.
fn read_last_prompt(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_SCAN_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = String::new();
    // Transcripts are UTF-8 but a byte-offset seek can land mid-codepoint;
    // read as bytes and recover lossily rather than failing the whole preview.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    buf.push_str(&String::from_utf8_lossy(&bytes));

    let mut lines: Vec<&str> = buf.lines().collect();
    // A non-zero start almost certainly lands mid-line; that fragment isn't
    // valid JSON and would only ever be discarded, but drop it explicitly.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }

    lines.iter().rev().find_map(|line| human_prompt(line))
}

/// Extract a human-typed prompt from one transcript line, or `None` if the line
/// is anything else — an assistant turn, a tool result, or one of the synthetic
/// user turns Claude Code injects for slash commands, hooks and reminders.
fn human_prompt(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(|t| t.as_str()) != Some("user") {
        return None;
    }
    // Compaction summaries and other injected turns carry isMeta.
    if value.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
        return None;
    }
    let content = value.get("message")?.get("content")?;
    let text = content_text(content)?;
    let text = text.trim();
    if text.is_empty() || is_synthetic(text) {
        return None;
    }
    Some(truncate(text, PREVIEW_MAX_CHARS))
}

/// Human-typed text out of a `message.content`, which is either a plain string
/// or — when the user attached a file or pasted an image — an array of blocks.
///
/// A `tool_result` block anywhere means the whole record is Claude Code feeding
/// itself, not the user typing, so the record is rejected outright.
fn content_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    if blocks
        .iter()
        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
    {
        return None;
    }
    let joined = blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.trim().is_empty()).then_some(joined)
}

/// Synthetic user turns Claude Code writes on the user's behalf. These are real
/// `type:"user"` records with string content, so they have to be filtered by
/// shape or the picker shows plumbing instead of what the user said.
///
/// Enumerating known wrappers proved too brittle — a first pass missed
/// `<task-notification>` and showed it as the preview for the very conversation
/// that motivated this feature. Claude Code writes all of them as a lowercase
/// XML-ish opening tag, so match that shape instead of a fixed list.
fn is_synthetic(text: &str) -> bool {
    text.starts_with("Caveat: The messages below were generated") || starts_with_xml_tag(text)
}

/// Does `text` open with a `<lowercase-tag>`? Deliberately narrow: a real prompt
/// pasting HTML would have to start at the very first character to be caught,
/// and the only cost of a false positive is falling back to an earlier prompt.
fn starts_with_xml_tag(text: &str) -> bool {
    let mut chars = text.chars();
    if chars.next() != Some('<') {
        return false;
    }
    let mut named = false;
    for c in chars {
        match c {
            'a'..='z' => named = true,
            '0'..='9' | '-' if named => {}
            '>' => return named,
            _ => return false,
        }
    }
    false
}

/// Truncate on a character boundary, adding an ellipsis when shortened.
fn truncate(text: &str, max: usize) -> String {
    // Collapse the newlines a multi-line prompt would otherwise bring into a
    // single-line row.
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Coarse "3d ago"-style label for a conversation's last write.
pub(crate) fn relative_time(then: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(then) else {
        return "just now".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else {
        format!("{}mo ago", secs / (86_400 * 30))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    /// Push a file's mtime into the past so ordering assertions are decisive.
    /// Uses std's `FileTimes` rather than pulling in a dev-dependency.
    fn backdate(path: &Path, secs: u64) {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        let when = SystemTime::now() - std::time::Duration::from_secs(secs);
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    fn user(text: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":{}}}}}"#,
            serde_json::Value::String(text.to_string())
        )
    }

    #[test]
    fn list_returns_every_conversation_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.jsonl", &[&user("one")]);
        write(dir.path(), "b.jsonl", &[&user("two")]);
        assert_eq!(list_in_dir(dir.path()).len(), 2);
    }

    #[test]
    fn list_reports_the_conversation_id_from_the_filename() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "abc-123.jsonl", &[&user("hi")]);
        assert_eq!(list_in_dir(dir.path())[0].id, "abc-123");
    }

    #[test]
    fn list_reports_a_modified_time_and_size() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.jsonl", &[&user("hello there")]);
        let c = &list_in_dir(dir.path())[0];
        assert!(c.size_bytes > 0);
        assert!(c.modified <= SystemTime::now());
    }

    #[test]
    fn list_orders_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), "old.jsonl", &[&user("old")]);
        write(dir.path(), "new.jsonl", &[&user("new")]);
        backdate(&old, 3600);
        let ids: Vec<_> = list_in_dir(dir.path()).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["new", "old"]);
    }

    #[test]
    fn list_excludes_subagent_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.jsonl", &[&user("hi")]);
        // Subagent files live one level down, under `<session>/subagents/`.
        let sub = dir.path().join("main").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        write(&sub, "agent-1.jsonl", &[&user("subagent turn")]);
        let ids: Vec<_> = list_in_dir(dir.path()).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["main"]);
    }

    #[test]
    fn list_is_empty_for_a_missing_directory() {
        assert!(list_in_dir(Path::new("/nonexistent/allele/project/dir")).is_empty());
    }

    #[test]
    fn preview_extracts_the_final_human_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "a.jsonl",
            &[&user("first thing"), &user("last thing")],
        );
        assert_eq!(read_last_prompt(&p).as_deref(), Some("last thing"));
    }

    #[test]
    fn preview_ignores_tool_result_turns() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "a.jsonl",
            &[
                &user("the real prompt"),
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            ],
        );
        assert_eq!(read_last_prompt(&p).as_deref(), Some("the real prompt"));
    }

    #[test]
    fn preview_ignores_synthetic_command_and_notification_turns() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "a.jsonl",
            &[
                &user("what I actually said"),
                &user("<command-name>/loop</command-name>"),
                &user("<system-reminder>be careful</system-reminder>"),
                // The wrapper that a fixed prefix list missed in the field.
                &user("<task-notification><task-id>x</task-id></task-notification>"),
            ],
        );
        assert_eq!(
            read_last_prompt(&p).as_deref(),
            Some("what I actually said")
        );
    }

    #[test]
    fn preview_reads_human_prompts_sent_as_content_blocks() {
        // Attaching a file makes Claude Code write the prompt as an array of
        // blocks rather than a plain string.
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "a.jsonl",
            &[
                &user("older"),
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"look at this file"}]}}"#,
            ],
        );
        assert_eq!(read_last_prompt(&p).as_deref(), Some("look at this file"));
    }

    #[test]
    fn preview_ignores_meta_turns() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "a.jsonl",
            &[
                &user("human text"),
                r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"injected"}}"#,
            ],
        );
        assert_eq!(read_last_prompt(&p).as_deref(), Some("human text"));
    }

    #[test]
    fn preview_truncates_and_flattens_long_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let long = "word ".repeat(200);
        let p = write(dir.path(), "a.jsonl", &[&user(&long)]);
        let got = read_last_prompt(&p).unwrap();
        assert!(got.ends_with('…'));
        assert!(!got.contains('\n'));
        // At most `max` characters plus the ellipsis; trailing whitespace is
        // trimmed before the ellipsis, so it can be one shorter.
        assert!(got.chars().count() <= PREVIEW_MAX_CHARS + 1);
        assert!(got.chars().count() >= PREVIEW_MAX_CHARS);
    }

    #[test]
    fn preview_reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        // Bury an early prompt under more than TAIL_SCAN_BYTES of later records,
        // so a tail-only reader cannot see it.
        let filler = user(&"x".repeat(4096));
        let mut lines: Vec<String> = vec![user("buried far too early")];
        lines.extend(std::iter::repeat_n(filler, 400));
        lines.push(user("near the end"));
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let p = write(dir.path(), "a.jsonl", &refs);
        assert!(std::fs::metadata(&p).unwrap().len() > TAIL_SCAN_BYTES);
        assert_eq!(read_last_prompt(&p).as_deref(), Some("near the end"));
    }

    #[test]
    fn title_comes_from_the_custom_title_record() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            "a.jsonl",
            &[r#"{"type":"custom-title","customTitle":"E2E testing SBS","sessionId":"a"}"#],
        );
        assert_eq!(read_title(&p).as_deref(), Some("E2E testing SBS"));
    }

    #[test]
    fn preview_falls_back_to_title_when_no_human_prompt_exists() {
        let mut c = Conversation {
            id: "a".into(),
            path: PathBuf::from("/tmp/a.jsonl"),
            modified: SystemTime::now(),
            size_bytes: 0,
            title: Some("A title".into()),
            last_prompt: None,
        };
        assert_eq!(c.preview(), Some("A title"));
        c.last_prompt = Some("the last thing said".into());
        assert_eq!(c.preview(), Some("the last thing said"));
    }

    #[test]
    fn a_single_conversation_never_interrupts() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "only.jsonl", &[&user("hi")]);
        assert!(!ambiguous_in_dir(dir.path(), "only"));
        // Even a pointer naming nothing on disk: with one candidate there is
        // no choice to offer.
        assert!(!ambiguous_in_dir(dir.path(), "some-other-id"));
    }

    #[test]
    fn a_pointer_already_naming_the_newest_never_interrupts() {
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), "old.jsonl", &[&user("old")]);
        write(dir.path(), "new.jsonl", &[&user("new")]);
        backdate(&old, 3600);
        assert!(!ambiguous_in_dir(dir.path(), "new"));
    }

    #[test]
    fn a_stale_pointer_interrupts() {
        // The reported failure: two conversations, and the id Allele would
        // resume is the superseded one.
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), "old.jsonl", &[&user("old")]);
        write(dir.path(), "new.jsonl", &[&user("new")]);
        backdate(&old, 3600);
        assert!(ambiguous_in_dir(dir.path(), "old"));
    }

    #[test]
    fn an_absent_pointer_interrupts_when_several_conversations_exist() {
        // The exact shape on disk in the incident: claude_session_id was null,
        // so the resolved id fell back to the workspace uuid.
        let dir = tempfile::tempdir().unwrap();
        let workspace = write(
            dir.path(),
            "workspace-uuid.jsonl",
            &[&user("older lineage")],
        );
        write(dir.path(), "rotated-uuid.jsonl", &[&user("the live work")]);
        backdate(&workspace, 3600);
        assert!(ambiguous_in_dir(dir.path(), "workspace-uuid"));
    }

    #[test]
    fn an_empty_workspace_never_interrupts() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!ambiguous_in_dir(dir.path(), "anything"));
    }

    #[test]
    fn reconcile_adopts_the_newest_only_when_the_pointer_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), "old.jsonl", &[&user("old")]);
        write(dir.path(), "new.jsonl", &[&user("new")]);
        backdate(&old, 3600);
        // Pointer still on disk — ambiguity is the picker's job, not a silent
        // rewrite, so nothing is adopted even though a newer sibling exists.
        assert_eq!(reconcile_missing_in_dir(dir.path(), "old"), None);
        // Pointer gone — unambiguous repair.
        assert_eq!(
            reconcile_missing_in_dir(dir.path(), "vanished").as_deref(),
            Some("new")
        );
    }

    #[test]
    fn reconcile_does_nothing_in_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(reconcile_missing_in_dir(dir.path(), "anything"), None);
    }

    #[test]
    fn xml_tag_detection_is_narrow() {
        assert!(starts_with_xml_tag("<task-notification>"));
        assert!(starts_with_xml_tag("<system-reminder>x"));
        assert!(!starts_with_xml_tag("<NotATag>"));
        assert!(!starts_with_xml_tag("a < b"));
        assert!(!starts_with_xml_tag("<"));
        assert!(!starts_with_xml_tag("< spaced>"));
    }

    #[test]
    fn size_is_formatted_for_humans() {
        assert_eq!(format_size(4 * 1024 * 1024), "4.0 MB");
        assert_eq!(format_size(2048), "2 KB");
    }

    #[test]
    fn relative_time_buckets() {
        let now = SystemTime::now();
        assert_eq!(relative_time(now), "just now");
        assert_eq!(
            relative_time(now - std::time::Duration::from_secs(600)),
            "10m ago"
        );
        assert_eq!(
            relative_time(now - std::time::Duration::from_secs(7200)),
            "2h ago"
        );
    }
}
