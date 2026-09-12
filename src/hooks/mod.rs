// Attention routing via Claude Code's hook system.
//
// Allele injects its own settings file at claude spawn time via
// `claude --settings <path>`. That settings file declares hooks for
// Notification, Stop, UserPromptSubmit, SessionStart, and SessionEnd —
// all pointing at a tiny shell receiver script that appends one JSONL
// line per event to `~/.allele/events/<session_id>.jsonl`.
//
// A background polling task in main.rs reads those files every 250ms,
// parses new lines, and updates the matching session's status. The
// priority rule is enforced on the rust side: AwaitingInput (from
// Notification) can never be stomped by ResponseReady (from Stop) —
// the user has to actually submit a new prompt to clear attention.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use tracing::{info, warn};

use crate::errors::AlleleError;

/// Canonical on-disk locations for the hook infrastructure.
pub fn base_dir() -> Option<PathBuf> {
    crate::paths::allele_dir()
}

pub fn hooks_settings_path() -> Option<PathBuf> {
    Some(base_dir()?.join("hooks.json"))
}

pub fn receiver_script_path() -> Option<PathBuf> {
    Some(base_dir()?.join("bin").join("hook-receiver.sh"))
}

pub fn events_dir() -> Option<PathBuf> {
    Some(base_dir()?.join("events"))
}

/// Shell body for the receiver script. Written verbatim to disk on startup.
///
/// Deliberately minimal:
/// - reads JSON from stdin
/// - extracts session_id (jq preferred, sed fallback)
/// - appends one JSONL line (ts + kind) to the per-session events file
/// - exits 0 on any error so hooks never block claude
const RECEIVER_SCRIPT: &str = r#"#!/bin/bash
# allele hook receiver — forwards Claude Code hook events to
# per-session JSONL files under ~/.allele/events/.
# Managed by the Allele app. Do not edit by hand — it will be
# regenerated on next launch.

set -u
kind="${1:-unknown}"
events_dir="$HOME/.allele/events"
mkdir -p "$events_dir" 2>/dev/null || exit 0

# Read the hook payload from stdin (non-blocking; claude always sends JSON)
payload=$(cat)

# Extract session_id — jq preferred, sed fallback
if command -v jq >/dev/null 2>&1; then
    session_id=$(printf '%s' "$payload" | jq -r '.session_id // empty' 2>/dev/null)
else
    session_id=$(printf '%s' "$payload" | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
fi

[ -z "$session_id" ] && exit 0

ts=$(date +%s)
out="$events_dir/$session_id.jsonl"

# Build the event line with optional rich fields extracted from the payload.
# jq is required for rich fields — without it we fall back to ts+kind only.
if command -v jq >/dev/null 2>&1; then
    tool_name=$(printf '%s' "$payload" | jq -r '.tool_name // empty' 2>/dev/null)
    tool_input=$(printf '%s' "$payload" | jq -c '.tool_input // empty' 2>/dev/null)
    message=$(printf '%s' "$payload" | jq -r '.message // empty' 2>/dev/null)
    title=$(printf '%s' "$payload" | jq -r '.title // empty' 2>/dev/null)
    # cwd lets Allele re-associate a session whose id rotated (/clear or
    # /compact spawns a fresh Claude session id). The workspace directory
    # is stable across a rotation, so it's the anchor Allele matches on.
    cwd=$(printf '%s' "$payload" | jq -r '.cwd // empty' 2>/dev/null)

    # Construct JSON with only non-empty fields to keep lines compact.
    # All string values go through jq -Rs for proper JSON escaping.
    line=$(printf '{"ts":%s,"kind":"%s"' "$ts" "$kind")
    [ -n "$tool_name" ] && line="$line,\"tool_name\":$(printf '%s' "$tool_name" | jq -Rs .)"
    [ -n "$tool_input" ] && [ "$tool_input" != '""' ] && line="$line,\"tool_input\":$tool_input"
    [ -n "$message" ] && line="$line,\"message\":$(printf '%s' "$message" | jq -Rs .)"
    [ -n "$title" ] && line="$line,\"title\":$(printf '%s' "$title" | jq -Rs .)"
    [ -n "$cwd" ] && line="$line,\"cwd\":$(printf '%s' "$cwd" | jq -Rs .)"
    line="$line}"

    printf '%s\n' "$line" >> "$out"
else
    printf '{"ts":%s,"kind":"%s"}\n' "$ts" "$kind" >> "$out"
fi

# Capture the first user prompt for session auto-naming.
# Writes to a .prompt sidecar file (first prompt only — skip if exists).
if [ "$kind" = "user_prompt_submit" ]; then
    prompt_file="$events_dir/$session_id.prompt"
    if [ ! -f "$prompt_file" ]; then
        if command -v jq >/dev/null 2>&1; then
            prompt=$(printf '%s' "$payload" | jq -r '.prompt // empty' 2>/dev/null)
        else
            prompt=$(printf '%s' "$payload" | sed -n 's/.*"prompt"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
        fi
        [ -n "$prompt" ] && printf '%s' "$prompt" > "$prompt_file"
    fi
fi

exit 0
"#;

/// Generate the settings JSON that Allele passes to `claude --settings`.
/// Uses an absolute receiver-script path so the hook works regardless of the
/// session's cwd (each session runs in its own APFS clone).
fn build_hooks_json(receiver: &str) -> serde_json::Value {
    let make_hook = |arg: &str| {
        serde_json::json!({
            "hooks": [
                {
                    "type": "command",
                    "command": format!("{receiver} {arg}")
                }
            ]
        })
    };

    serde_json::json!({
        "_allele_version": 5,
        "hooks": {
            "Notification":        [make_hook("notification")],
            "Stop":                [make_hook("stop")],
            "UserPromptSubmit":    [make_hook("user_prompt_submit")],
            "SessionStart":        [make_hook("session_start")],
            "SessionEnd":          [make_hook("session_end")],
            // PreToolUse / PostToolUse are the clearing signals for
            // AwaitingInput: when Claude actually executes a tool after a
            // permission prompt, we know the block was resolved and the
            // session is back to Running. Without these, an approved
            // permission prompt leaves the ⚠ icon stuck on the sidebar.
            "PreToolUse":          [make_hook("pre_tool_use")],
            "PostToolUse":         [make_hook("post_tool_use")],
        }
    })
}

/// Install the receiver script and hooks.json on disk if they're missing
/// (or if the version marker in hooks.json is stale). Idempotent — safe to
/// call on every app startup.
///
/// Returns the absolute path to hooks.json so the caller can pass it to
/// `claude --settings`.
pub fn install_if_missing() -> crate::errors::Result<PathBuf> {
    let base = base_dir().ok_or_else(|| AlleleError::Hooks("no home directory".to_string()))?;
    fs::create_dir_all(&base)?;
    fs::create_dir_all(base.join("bin"))?;
    fs::create_dir_all(base.join("events"))?;

    // Write the receiver script every time — it's tiny and this guarantees
    // the on-disk copy matches the source in case we ship a fix.
    let receiver_path = receiver_script_path()
        .ok_or_else(|| AlleleError::Hooks("no home directory".to_string()))?;
    fs::write(&receiver_path, RECEIVER_SCRIPT)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&receiver_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&receiver_path, perms)?;
    }

    // Write (or rewrite) hooks.json. We always rewrite so the receiver path
    // is current and the version marker is up-to-date.
    let hooks_path =
        hooks_settings_path().ok_or_else(|| AlleleError::Hooks("no home directory".to_string()))?;
    let receiver_abs = receiver_path.to_string_lossy().to_string();
    let hooks_json = build_hooks_json(&receiver_abs);
    fs::write(&hooks_path, serde_json::to_string_pretty(&hooks_json)?)?;

    Ok(hooks_path)
}

// --- event polling -----------------------------------------------------------

/// A single hook event parsed from the receiver's JSONL output.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookEventLine {
    pub ts: u64,
    pub kind: String,
    /// Tool name from the hook payload (PreToolUse, Notification).
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Tool input from the hook payload (PreToolUse, Notification).
    /// Kept as raw JSON so we don't have to model every tool's schema.
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Notification message text (Notification hook).
    #[serde(default)]
    pub message: Option<String>,
    /// Notification title text (Notification hook).
    #[serde(default)]
    pub title: Option<String>,
    /// Working directory from the hook payload — present on every event when
    /// jq is available. Used to re-associate a session whose Claude id rotated
    /// after a `/clear`: the cwd (= the session's clone dir) is stable across
    /// the rotation, so it anchors the new id back to the right workspace.
    #[serde(default)]
    pub cwd: Option<String>,
}

/// A fully-resolved agent event ready for the main thread to consume.
///
/// `kind` is the raw native event name as written to the events file
/// (Claude hook name, or a canonical name emitted by the opencode plugin).
/// Interpretation into a status transition is the responsibility of the
/// session's agent adapter (`AgentAdapter::interpret_event`) — this struct
/// is a dumb transport carrier and knows nothing about any specific agent.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub session_id: String,
    pub kind: String,
    /// Working directory the event fired in, when the receiver captured it.
    /// Anchors session-id re-association after a `/clear` rotation.
    pub cwd: Option<String>,
    /// Rich payload data from the event producer — populated when the event
    /// carries tool/notification context.
    pub payload: Option<HookPayload>,
}

/// Rich data extracted from the hook receiver's full payload capture.
/// Attached to Notification and PreToolUse events so the attention bar
/// can show what each session wants without the user switching to it.
#[derive(Debug, Clone)]
pub struct HookPayload {
    pub message: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
}

/// How often the events directory is swept. The poller ticks four times a
/// second; sweeping at that rate would cost more than the growth it prevents.
const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a sweep is due. Pure so the cadence can be tested without a clock
/// or a directory — see [`EventWatcher::maybe_prune`].
fn prune_is_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last {
        None => true,
        Some(last) => now.duration_since(last) >= PRUNE_INTERVAL,
    }
}

/// Tracks per-file read offsets so previously-processed lines are never
/// re-emitted. In-memory only — if the app restarts, we fast-forward each
/// file to its current end (see [`EventWatcher::initialize_offsets`]).
#[derive(Default)]
pub struct EventWatcher {
    offsets: std::collections::HashMap<PathBuf, u64>,
    /// When the events directory was last swept. `None` until the first
    /// sweep, which is why pruning is rate-limited rather than scheduled —
    /// see [`EventWatcher::maybe_prune`].
    last_prune: Option<std::time::Instant>,
}

impl EventWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fast-forward every existing events file to its current end. Called
    /// once at startup so we don't flood the user with pre-existing events
    /// from before the app was running.
    pub fn initialize_offsets(&mut self) {
        let Some(dir) = events_dir() else { return };
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            self.offsets.insert(path, meta.len());
        }
    }

    /// Read every events file in `~/.allele/events/`, parse new lines
    /// since the last poll, and return the list of fresh events.
    ///
    /// The session_id is extracted from the filename (`<session_id>.jsonl`),
    /// not from inside the JSON payload — the receiver script puts the ID
    /// in the path, which is cheaper than parsing it out of every line.
    pub fn poll(&mut self) -> Vec<HookEvent> {
        let Some(dir) = events_dir() else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };

        let mut out = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // Only process .jsonl event files — skip .prompt sidecars and
            // any other non-JSONL files in the events directory.
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            // Derive session_id from the filename
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let session_id = stem.to_string();

            let last_offset = self.offsets.get(&path).copied().unwrap_or(0);

            // Open and seek
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let Ok(meta) = file.metadata() else {
                continue;
            };
            let current_len = meta.len();

            if current_len < last_offset {
                // File was truncated or replaced — reset offset to 0
                self.offsets.insert(path.clone(), 0);
            }

            if current_len == last_offset {
                continue; // nothing new
            }

            let mut reader = BufReader::new(file);
            if reader.seek(SeekFrom::Start(last_offset)).is_err() {
                continue;
            }

            let mut bytes_read = last_offset;
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                bytes_read += line.len() as u64 + 1; // +1 for newline

                if line.trim().is_empty() {
                    continue;
                }

                match serde_json::from_str::<HookEventLine>(&line) {
                    Ok(parsed) => {
                        let has_data = parsed.message.is_some()
                            || parsed.tool_name.is_some()
                            || parsed.tool_input.is_some();
                        let payload = if has_data {
                            Some(HookPayload {
                                message: parsed.message,
                                tool_name: parsed.tool_name,
                                tool_input: parsed.tool_input,
                            })
                        } else {
                            None
                        };
                        out.push(HookEvent {
                            session_id: session_id.clone(),
                            kind: parsed.kind,
                            cwd: parsed.cwd,
                            payload,
                        });
                    }
                    Err(e) => {
                        warn!("hooks: skipping malformed line in {}: {e}", path.display());
                    }
                }
            }

            self.offsets.insert(path, bytes_read.min(current_len));
        }

        out
    }

    /// Delete event files belonging to sessions allele no longer has, at most
    /// once a minute. Returns how many files were removed.
    ///
    /// The directory is otherwise append-only: every session that has ever run
    /// leaves a `<id>.jsonl` and a `<id>.prompt` behind for good, and
    /// [`poll`](Self::poll) stats every one of them four times a second. On
    /// the machine this was diagnosed on that had reached 830 files and 133MB,
    /// costing ~13% of the foreground thread before anything was even read.
    ///
    /// Rate-limited here rather than at the call site so the cadence lives
    /// with the thing it governs, and the poller stays a poller (DEV-602).
    pub fn maybe_prune(&mut self, live: &std::collections::HashSet<String>) -> usize {
        let now = std::time::Instant::now();
        if !prune_is_due(self.last_prune, now) {
            return 0;
        }
        self.last_prune = Some(now);

        let Some(dir) = events_dir() else {
            return 0;
        };
        self.prune_in(&dir, live)
    }

    /// The sweep itself, against an explicit directory.
    ///
    /// Takes the directory rather than resolving it so the sweep can be
    /// exercised against a temp dir. The resolved path is the user's real
    /// `~/.allele/events`, and a test that swept that would be deleting the
    /// status of sessions someone is running.
    ///
    /// Two guards, both load-bearing:
    ///
    /// 1. `live` must carry a session's workspace id *and* its current Claude
    ///    conversation id — `/clear` rotates the latter, and the events file is
    ///    named after whichever one the hook fired under. See
    ///    `AppState::live_event_ids`.
    /// 2. A file is only removed once it has gone untouched for `MIN_AGE`, so
    ///    a session whose `Session` has not materialised yet cannot have its
    ///    events deleted out from under it.
    fn prune_in(
        &mut self,
        dir: &std::path::Path,
        live: &std::collections::HashSet<String>,
    ) -> usize {
        /// Deliberately generous. The cost of keeping a dead session's events
        /// for another hour is a few stats; the cost of deleting a live
        /// session's is losing the status of a session someone is watching.
        const MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };

        let mut removed = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("jsonl") | Some("prompt")
            ) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if live.contains(stem) {
                continue;
            }
            let recently_touched = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.elapsed().ok())
                .is_none_or(|age| age < MIN_AGE);
            if recently_touched {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                self.offsets.remove(&path);
                removed += 1;
            }
        }

        if removed > 0 {
            info!("hooks: pruned {removed} event files for sessions that no longer exist");
        }
        removed
    }
}

// --- attention affordances ---------------------------------------------------

/// Play a macOS system sound asynchronously via `afplay`. Spawns as a
/// fully detached background process — never blocks the UI thread.
/// Silently does nothing on non-macOS.
pub fn play_sound(path: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        let _ = Command::new("afplay")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path; // avoid unused warning
    }
}

/// Fire a macOS notification via `osascript -e 'display notification ...'`.
/// Spawns detached — never blocks. Silently no-ops on non-macOS.
pub fn show_notification(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        // Escape double quotes in title/body to survive AppleScript quoting
        let escape = |s: &str| s.replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape(body),
            escape(title)
        );
        let _ = Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body);
    }
}

/// Show a blocking modal dialog via `osascript -e 'display dialog ...'`
/// with a stop icon and a single OK button. Used for fatal startup errors
/// that must block before the caller exits the process. Silently no-ops
/// on non-macOS.
pub fn show_fatal_dialog(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        // Escape double quotes for AppleScript, then convert real newlines
        // into AppleScript's `\n` escape sequence so they render as line
        // breaks inside `display dialog`.
        let escape = |s: &str| s.replace('"', "\\\"").replace('\n', "\\n");
        let script = format!(
            "display dialog \"{}\" with title \"{}\" with icon stop \
             buttons {{\"OK\"}} default button 1",
            escape(body),
            escape(title)
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (title, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An events directory holding `<id>.jsonl` and `<id>.prompt` per id, each
    /// backdated by `age` so the sweep's minimum-age guard can be exercised.
    fn events_fixture(tag: &str, ids: &[&str], age: std::time::Duration) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("allele-prune-{}-{tag}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("temp dir");

        let when = std::time::SystemTime::now() - age;
        for id in ids {
            for ext in ["jsonl", "prompt"] {
                let file = fs::File::create(dir.join(format!("{id}.{ext}"))).expect("create");
                file.set_modified(when).expect("backdate");
            }
        }
        dir
    }

    fn live(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    const TWO_HOURS: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

    /// The guard that matters most: a session someone is watching must never
    /// lose its events, however old the file is.
    #[test]
    fn a_live_sessions_events_are_never_pruned() {
        let dir = events_fixture("live", &["alive"], TWO_HOURS);

        let removed = EventWatcher::default().prune_in(&dir, &live(&["alive"]));

        assert_eq!(removed, 0);
        assert!(dir.join("alive.jsonl").exists());
        assert!(dir.join("alive.prompt").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// A `/clear` rotates the Claude conversation id, and the events file is
    /// named after whichever id the hook fired under. Pruning on the workspace
    /// id alone would delete the live events of every cleared session — which
    /// is why `live_event_ids` contributes both.
    #[test]
    fn a_rotated_conversation_id_still_protects_its_events() {
        let dir = events_fixture("rotated", &["rotated-convo"], TWO_HOURS);

        // The workspace id is "workspace"; the file is named after the id the
        // hook fired under, which the set also carries.
        let removed =
            EventWatcher::default().prune_in(&dir, &live(&["workspace", "rotated-convo"]));

        assert_eq!(removed, 0);
        assert!(dir.join("rotated-convo.jsonl").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dead_sessions_events_are_pruned() {
        let dir = events_fixture("dead", &["gone"], TWO_HOURS);

        let removed = EventWatcher::default().prune_in(&dir, &live(&["someone-else"]));

        assert_eq!(removed, 2, "both the jsonl and its prompt sidecar");
        assert!(!dir.join("gone.jsonl").exists());
        assert!(!dir.join("gone.prompt").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// A session whose `Session` has not materialised yet — still cloning —
    /// is not in the live set, but its agent may already be writing events.
    /// The age guard is what stops the sweep deleting them underneath it.
    #[test]
    fn a_recently_touched_file_survives_even_when_unknown() {
        let dir = events_fixture(
            "fresh",
            &["just-started"],
            std::time::Duration::from_secs(5),
        );

        let removed = EventWatcher::default().prune_in(&dir, &live(&[]));

        assert_eq!(removed, 0);
        assert!(dir.join("just-started.jsonl").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// Pruned files must also leave the offset map, or it grows without bound
    /// holding paths that no longer exist.
    #[test]
    fn pruning_forgets_the_offsets_of_deleted_files() {
        let dir = events_fixture("offsets", &["gone"], TWO_HOURS);
        let mut watcher = EventWatcher::default();
        watcher.offsets.insert(dir.join("gone.jsonl"), 128);

        watcher.prune_in(&dir, &live(&[]));

        assert!(!watcher.offsets.contains_key(&dir.join("gone.jsonl")));
        fs::remove_dir_all(&dir).ok();
    }

    /// The poller ticks four times a second; sweeping every tick would cost
    /// more than the growth it prevents.
    #[test]
    fn a_sweep_is_due_once_per_interval() {
        let now = std::time::Instant::now();

        assert!(prune_is_due(None, now), "the first sweep is always due");
        assert!(!prune_is_due(Some(now), now), "not twice in a row");

        let long_ago = now
            .checked_sub(PRUNE_INTERVAL + std::time::Duration::from_secs(1))
            .expect("representable");
        assert!(prune_is_due(Some(long_ago), now));
    }

    #[test]
    fn hook_event_line_parses_cwd_when_present() {
        // A SessionStart line as emitted by the receiver after a /clear.
        let line = r#"{"ts":1751000000,"kind":"session_start","cwd":"/Users/x/.allele/workspaces/allele/e084e960"}"#;
        let parsed: HookEventLine = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.kind, "session_start");
        assert_eq!(
            parsed.cwd.as_deref(),
            Some("/Users/x/.allele/workspaces/allele/e084e960")
        );
    }

    #[test]
    fn hook_event_line_cwd_defaults_to_none() {
        // Older lines (pre-cwd receiver, or the jq-less fallback) omit cwd.
        let line = r#"{"ts":1751000000,"kind":"stop"}"#;
        let parsed: HookEventLine = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.cwd, None);
    }
}
