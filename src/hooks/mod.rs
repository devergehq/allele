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

/// Canonical on-disk locations for the hook infrastructure.
pub fn base_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".allele"))
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
printf '{"ts":%s,"kind":"%s"}\n' "$ts" "$kind" >> "$out"
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
        "_allele_version": 2,
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
pub fn install_if_missing() -> anyhow::Result<PathBuf> {
    let base = base_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    fs::create_dir_all(&base)?;
    fs::create_dir_all(base.join("bin"))?;
    fs::create_dir_all(base.join("events"))?;

    // Write the receiver script every time — it's tiny and this guarantees
    // the on-disk copy matches the source in case we ship a fix.
    let receiver_path = receiver_script_path()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
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
    let hooks_path = hooks_settings_path()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;
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
}

/// The kinds of events we route into status transitions. Unknown kinds are
/// tolerated and logged, not an error (forward compatibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Notification,
    Stop,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    /// Fires right before Claude runs a tool. Critical clearing signal:
    /// once PreToolUse fires we know the permission prompt (if any) was
    /// resolved and Claude is actively executing.
    PreToolUse,
    /// Fires after a tool completes. Belt-and-suspenders clearing signal.
    PostToolUse,
    Other,
}

impl HookKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "notification" => HookKind::Notification,
            "stop" => HookKind::Stop,
            "user_prompt_submit" => HookKind::UserPromptSubmit,
            "session_start" => HookKind::SessionStart,
            "session_end" => HookKind::SessionEnd,
            "pre_tool_use" => HookKind::PreToolUse,
            "post_tool_use" => HookKind::PostToolUse,
            _ => HookKind::Other,
        }
    }
}

/// A fully-resolved hook event ready for the main thread to consume.
#[derive(Debug, Clone)]
pub struct HookEvent {
    pub session_id: String,
    pub kind: HookKind,
    /// Unix epoch seconds — reserved for the Phase C activity feed, which
    /// will display a chronological list of attention events.
    #[allow(dead_code)]
    pub ts: u64,
}

/// Tracks per-file read offsets so previously-processed lines are never
/// re-emitted. In-memory only — if the app restarts, we fast-forward each
/// file to its current end (see [`EventWatcher::initialize_offsets`]).
pub struct EventWatcher {
    offsets: std::collections::HashMap<PathBuf, u64>,
}

impl Default for EventWatcher {
    fn default() -> Self {
        Self {
            offsets: std::collections::HashMap::new(),
        }
    }
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
        let Ok(entries) = fs::read_dir(&dir) else { return };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() { continue; }
            let Ok(meta) = entry.metadata() else { continue; };
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
        let Some(dir) = events_dir() else { return Vec::new(); };
        let Ok(entries) = fs::read_dir(&dir) else { return Vec::new(); };

        let mut out = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() { continue; }

            // Derive session_id from the filename
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue; };
            let session_id = stem.to_string();

            let last_offset = self.offsets.get(&path).copied().unwrap_or(0);

            // Open and seek
            let Ok(file) = fs::File::open(&path) else { continue; };
            let Ok(meta) = file.metadata() else { continue; };
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
                let Ok(line) = line else { break; };
                bytes_read += line.len() as u64 + 1; // +1 for newline

                if line.trim().is_empty() { continue; }

                match serde_json::from_str::<HookEventLine>(&line) {
                    Ok(parsed) => {
                        out.push(HookEvent {
                            session_id: session_id.clone(),
                            kind: HookKind::parse(&parsed.kind),
                            ts: parsed.ts,
                        });
                    }
                    Err(e) => {
                        eprintln!(
                            "hooks: skipping malformed line in {}: {e}",
                            path.display()
                        );
                    }
                }
            }

            self.offsets.insert(path, bytes_read.min(current_len));
        }

        out
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
