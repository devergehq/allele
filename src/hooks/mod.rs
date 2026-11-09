// Attention routing via Claude Code's hook system.
//
// Allele injects its own settings file at claude spawn time via
// `claude --settings <path>`. That settings file declares hooks for
// Notification, Stop, UserPromptSubmit, SessionStart, and SessionEnd —
// all pointing at a tiny shell receiver script that appends one JSONL
// line per event to `<root>/.allele/events/<session_id>.jsonl`.
//
// A background polling task in main.rs reads those files every 250ms,
// parses new lines, and updates the matching session's status. The
// priority rule is enforced on the rust side: AwaitingInput (from
// Notification) can never be stomped by ResponseReady (from Stop) —
// the user has to actually submit a new prompt to clear attention.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::errors::AlleleError;

/// Canonical on-disk locations for the hook infrastructure.
pub fn base_dir() -> Option<PathBuf> {
    crate::paths::allele_dir()
}

/// The events directory of *this* instance — what the watcher reads, and what
/// [`install_in`] substitutes into the receiver so the two cannot disagree.
pub fn events_dir() -> Option<PathBuf> {
    Some(events_dir_in(&base_dir()?))
}

// The three paths below take an explicit `<root>/.allele` rather than reading
// the process-global root, so [`install_in`] can be driven against a base that
// is not this process's. Same split, and for the same reason, as
// `paths::allele_dir_in`: `paths::init` fixes a `OnceLock` that one test in this
// binary already sets, so a test that needed to redirect the root to see what
// the installer writes could not exist. That is precisely the test DEV-951 was
// missing, and why the hard-coded `$HOME` survived a root-awareness pass.
//
// `hooks.json` and the receiver script have no caller outside the installer, so
// they have no process-global accessor at all — one derivation each, used once.

fn hooks_settings_path_in(base: &Path) -> PathBuf {
    base.join("hooks.json")
}

fn receiver_script_path_in(base: &Path) -> PathBuf {
    base.join("bin").join("hook-receiver.sh")
}

fn events_dir_in(base: &Path) -> PathBuf {
    base.join("events")
}

fn status_line_script_path_in(base: &Path) -> PathBuf {
    base.join("bin").join("status-line.sh")
}

fn status_line_payload_path_in(base: &Path) -> PathBuf {
    base.join("status-line.json")
}

/// Where the status-line wrapper leaves the latest payload Claude Code handed
/// it (DEV-1259). Read by `context_watch` for the account's `rate_limits`.
pub fn status_line_payload_path() -> Option<PathBuf> {
    Some(status_line_payload_path_in(&base_dir()?))
}

/// Restrict the events directory and everything already in it to the owner,
/// returning how many files were tightened.
///
/// The receiver writes new files `0600` via its umask, but that does nothing
/// for an install that already exists: the directory and its history predate
/// it. On the machine this was found on that meant 81 world-readable briefs,
/// the oldest four months old.
///
/// This module already chmods the receiver script `0755` on purpose, and
/// `dispatch::server` binds the control socket `0600` with a note about
/// privilege. The events directory was the one gap in that pattern (DEV-605).
#[cfg(unix)]
fn restrict_events_dir(dir: &Path) -> usize {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));

    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };

    let mut tightened = 0usize;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mode = meta.permissions().mode();
        if mode & 0o077 == 0 {
            continue; // already owner-only
        }
        // Keep the owner bits, drop group and other.
        if fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode & 0o700)).is_ok() {
            tightened += 1;
        }
    }
    tightened
}

#[cfg(not(unix))]
fn restrict_events_dir(_dir: &Path) -> usize {
    0
}

/// The literal [`RECEIVER_SCRIPT_TEMPLATE`] substitutes the instance's events
/// directory for. Chosen so it cannot contain `"#` and therefore cannot
/// terminate the raw string the template is written as.
const EVENTS_DIR_PLACEHOLDER: &str = "@@ALLELE_EVENTS_DIR@@";

/// Shell body for the receiver script, with [`EVENTS_DIR_PLACEHOLDER`] standing
/// in for the events directory. Rendered by [`build_receiver_script`] and
/// written to disk on every startup.
///
/// Deliberately minimal:
/// - reads JSON from stdin
/// - extracts session_id (jq preferred, sed fallback)
/// - appends one JSONL line (ts + kind) to the per-session events file
/// - exits 0 on any error so hooks never block claude
const RECEIVER_SCRIPT_TEMPLATE: &str = r#"#!/bin/bash
# allele hook receiver — forwards Claude Code hook events to per-session
# JSONL files under the events directory of the instance that installed it.
# Managed by the Allele app. Do not edit by hand — it will be
# regenerated on next launch.

set -u
# Event files carry the briefs sessions are dispatched with, which routinely
# include client context. Owner-only, so a dependency postinstall or any tool
# an agent runs cannot read another session's brief (DEV-605).
umask 077
kind="${1:-unknown}"
# The events directory of the instance that installed this copy, substituted
# at install time. A redirected instance — `--sandbox`, `--home <dir>` or
# `ALLELE_HOME=<dir>` — installs its own copy under its own root, so there is
# exactly one correct value per copy and nothing to resolve at run time.
#
# This read `$HOME/.allele/events` until DEV-951. Every redirected instance
# therefore wrote its sessions' events into the *live* tree while watching its
# own, which stayed empty — so a status change could never be observed there
# and every `allele_sessions_create` into one spent its whole 120s budget and
# returned `prompt_delivery_unconfirmed`, while the agent was in fact running.
events_dir=@@ALLELE_EVENTS_DIR@@
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

/// Placeholders [`STATUS_LINE_SCRIPT_TEMPLATE`] substitutes, for the file the
/// payload is written to and the script's own path.
const STATUS_LINE_FILE_PLACEHOLDER: &str = "@@ALLELE_STATUS_LINE_FILE@@";
const STATUS_LINE_SELF_PLACEHOLDER: &str = "@@ALLELE_STATUS_LINE_SELF@@";

/// Shell body for the status-line wrapper (DEV-1259).
///
/// Claude Code hands its status-line command a JSON payload that, for an
/// interactive session, carries the account's `rate_limits` — the only place
/// an interactive session reports them, since its on-disk transcript has no
/// `rate_limit_event`. Allele's `--settings` therefore names this wrapper as
/// the status line, and the wrapper does two things:
///
/// 1. leaves the payload where Allele reads it, replacing the previous one
///    atomically (temp file, then rename), and
/// 2. hands the *same* stdin to the status line the user configured, so what
///    the user sees is exactly what their own command prints.
///
/// `--settings` outranks the user's and the project's settings, so without
/// step 2 Allele would silently replace a user's status line. The user's
/// command is looked up at run time, in Claude Code's own precedence order
/// (project local, project shared, user), so editing it needs no relaunch.
/// JSON is read with `plutil`, which every Mac has, and `jq` when it does not.
/// Every failure exits 0 with no output: a broken wrapper must never cost the
/// user their status line's slot or block Claude.
const STATUS_LINE_SCRIPT_TEMPLATE: &str = r#"#!/bin/bash
# allele status-line wrapper — records Claude Code's status-line payload for
# Allele, then runs the user's own status line on the same input.
# Managed by the Allele app. Do not edit by hand — it will be
# regenerated on next launch.

set -u
umask 077
out=@@ALLELE_STATUS_LINE_FILE@@
self=@@ALLELE_STATUS_LINE_SELF@@

payload=$(cat)

tmp="$out.$$"
if printf '%s' "$payload" > "$tmp" 2>/dev/null; then
    mv -f "$tmp" "$out" 2>/dev/null || rm -f "$tmp"
fi

# json_get KEY.PATH FILE — a string at KEY.PATH, or nothing. FILE `-` is stdin.
json_get() {
    if command -v plutil >/dev/null 2>&1; then
        v=$(plutil -extract "$1" raw -o - "$2" 2>/dev/null) && printf '%s' "$v"
    elif command -v jq >/dev/null 2>&1; then
        if [ "$2" = "-" ]; then
            jq -r ".$1 // empty" 2>/dev/null
        else
            jq -r ".$1 // empty" "$2" 2>/dev/null
        fi
    fi
}

project_dir=$(printf '%s' "$payload" | json_get workspace.project_dir -)
[ -n "$project_dir" ] || project_dir=$(printf '%s' "$payload" | json_get cwd -)
config_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"

cmd=""
for f in \
    "$project_dir/.claude/settings.local.json" \
    "$project_dir/.claude/settings.json" \
    "$config_dir/settings.json"; do
    [ -n "$project_dir" ] || [ "$f" = "$config_dir/settings.json" ] || continue
    [ -f "$f" ] || continue
    c=$(json_get statusLine.command "$f")
    [ -n "$c" ] || continue
    t=$(json_get statusLine.type "$f")
    [ -z "$t" ] || [ "$t" = "command" ] || continue
    cmd="$c"
    break
done

[ -n "$cmd" ] || exit 0
# Never run ourselves: a settings file naming this wrapper would recurse.
case "$cmd" in *"$self"*) exit 0 ;; esac

printf '%s' "$payload" | /bin/bash -c "$cmd"
exit 0
"#;

/// Render the status-line wrapper for one instance's root.
fn build_status_line_script(payload_file: &Path, script: &Path) -> String {
    STATUS_LINE_SCRIPT_TEMPLATE
        .replace(STATUS_LINE_FILE_PLACEHOLDER, &shell_quote(payload_file))
        .replace(STATUS_LINE_SELF_PLACEHOLDER, &shell_quote(script))
}

/// The user's own `statusLine` object, from Claude Code's user settings.
/// Its display options (padding and the like) are carried onto the wrapper's
/// entry so the user's line renders as it did; the command itself is looked
/// up by the wrapper at run time.
fn user_status_line() -> Option<serde_json::Value> {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))?;
    let text = fs::read_to_string(config_dir.join("settings.json")).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
    settings.get("statusLine").cloned()
}

/// Quote a path for interpolation into a POSIX shell script.
///
/// The root arrives from `--home` or `ALLELE_HOME`, so it is arbitrary user
/// input: unquoted, a space would split the assignment and a `$` would expand
/// against the agent's environment. Single quotes suppress every expansion,
/// and an embedded single quote is closed, escaped and reopened — the standard
/// `'\''` dance, which is the only character single-quoting cannot carry.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

/// Render the receiver script for one instance's events directory.
fn build_receiver_script(events_dir: &Path) -> String {
    RECEIVER_SCRIPT_TEMPLATE.replace(EVENTS_DIR_PLACEHOLDER, &shell_quote(events_dir))
}

/// Generate the settings JSON that Allele passes to `claude --settings`.
/// Uses an absolute receiver-script path so the hook works regardless of the
/// session's cwd (each session runs in its own APFS clone).
///
/// Takes the path rather than a string, and quotes it here, so there is one
/// place that can get it wrong. Claude Code evaluates a hook `command` through
/// a shell — verified by running `claude -p` against a hand-written settings
/// file on a directory with a space in it: the quoted form fired, the bare form
/// never fired at all. So an unquoted receiver under a root like
/// `--home "/Users/me/My Roots/alpha"` word-splits into a path that does not
/// exist, no hook ever runs, and the symptom is indistinguishable from DEV-951
/// itself: no status transitions, and every create burning its 120s budget.
fn build_hooks_json(
    receiver: &Path,
    status_line: &Path,
    user_status_line: Option<&serde_json::Value>,
) -> serde_json::Value {
    let receiver = shell_quote(receiver);
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
        },
        "statusLine": status_line_entry(status_line, user_status_line),
    })
}

/// The `statusLine` entry naming the wrapper (DEV-1259): the user's own entry
/// with its display options intact and only the command swapped, or a bare
/// command entry when the user has none.
fn status_line_entry(
    status_line: &Path,
    user_status_line: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut entry = user_status_line
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    entry.insert("type".into(), "command".into());
    entry.insert("command".into(), shell_quote(status_line).into());
    serde_json::Value::Object(entry)
}

/// Install the receiver script and hooks.json on disk if they're missing
/// (or if the version marker in hooks.json is stale). Idempotent — safe to
/// call on every app startup.
///
/// Returns the absolute path to hooks.json so the caller can pass it to
/// `claude --settings`.
pub fn install_if_missing() -> crate::errors::Result<PathBuf> {
    let base = base_dir().ok_or_else(|| AlleleError::Hooks("no home directory".to_string()))?;
    install_in(&base)
}

/// The installer proper, against an explicit `<root>/.allele`.
///
/// Split from [`install_if_missing`] so the events directory the written script
/// names can be pinned against a *redirected* root rather than whatever the
/// process-global one happens to be. DEV-951 was invisible to every test that
/// could only ever see one root.
fn install_in(base: &Path) -> crate::errors::Result<PathBuf> {
    fs::create_dir_all(base)?;
    fs::create_dir_all(base.join("bin"))?;
    let events = events_dir_in(base);
    fs::create_dir_all(&events)?;

    // Tighten on every launch, not just at creation: an existing install has
    // months of 0644 briefs that the receiver's umask alone would never fix.
    let tightened = restrict_events_dir(&events);
    if tightened > 0 {
        info!("hooks: made {tightened} existing event files owner-only");
    }

    // Write the receiver script every time — it's tiny and this guarantees
    // the on-disk copy matches the source in case we ship a fix. It is also
    // what carries the DEV-951 events-directory fix to an install that already
    // exists, on its next launch and with no `_allele_version` bump: the
    // version marker gates nothing here, because nothing is conditional.
    let receiver_path = receiver_script_path_in(base);
    fs::write(&receiver_path, build_receiver_script(&events))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&receiver_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&receiver_path, perms)?;
    }

    // The status-line wrapper (DEV-1259), rewritten every launch for the
    // same reason as the receiver.
    let status_line_path = status_line_script_path_in(base);
    fs::write(
        &status_line_path,
        build_status_line_script(&status_line_payload_path_in(base), &status_line_path),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&status_line_path, fs::Permissions::from_mode(0o755))?;
    }

    // Write (or rewrite) hooks.json. We always rewrite so the receiver path
    // is current and the version marker is up-to-date.
    let hooks_path = hooks_settings_path_in(base);
    let hooks_json = build_hooks_json(
        &receiver_path,
        &status_line_path,
        user_status_line().as_ref(),
    );
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
        let mut command = Command::new("afplay");
        command
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Err(e) = crate::proc::spawn_and_reap(command) {
            tracing::warn!("play_sound({path}): spawn failed: {e}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = path; // avoid unused warning
    }
}

/// Make `s` safe to place between double quotes in an AppleScript string
/// literal. Backslashes are escaped first, then quotes, so a trailing `\` in
/// the input cannot swallow the closing quote. Callers pass text they do not
/// control (session labels, other machines' names), and an unescaped
/// backslash would let that text end the string and run its own AppleScript.
/// Real newlines become `\n` so they render as line breaks.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// The AppleScript `show_notification` hands to `osascript -e`. Both strings
/// go through [`applescript_escape`]; split out so tests can check exactly
/// what osascript would be given.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn notification_script(title: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(body),
        applescript_escape(title)
    )
}

/// Fire a macOS notification via `osascript -e 'display notification ...'`.
/// Spawns detached — never blocks. Silently no-ops on non-macOS.
pub fn show_notification(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        let script = notification_script(title, body);
        let mut command = Command::new("osascript");
        command
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Err(e) = crate::proc::spawn_and_reap(command) {
            tracing::warn!("show_notification: spawn failed: {e}");
        }
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
        let script = format!(
            "display dialog \"{}\" with title \"{}\" with icon stop \
             buttons {{\"OK\"}} default button 1",
            applescript_escape(body),
            applescript_escape(title)
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
    #[test]
    fn applescript_escape_handles_quotes_and_newlines() {
        assert_eq!(applescript_escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(applescript_escape("a\nb"), r"a\nb");
    }

    /// A backslash before a quote must not turn the escaped quote back into a
    /// string terminator, or the text could run its own AppleScript.
    #[test]
    fn applescript_escape_cannot_be_broken_out_of() {
        let hostile = r#"\" & do shell script "touch /tmp/x" & ""#;
        let escaped = applescript_escape(hostile);
        assert_eq!(escaped, r#"\\\" & do shell script \"touch /tmp/x\" & \""#);
        // Every quote in the output is preceded by an odd run of backslashes,
        // so none of them closes the literal.
        let bytes = escaped.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'"' {
                let run = bytes[..i].iter().rev().take_while(|c| **c == b'\\').count();
                assert_eq!(run % 2, 1, "unescaped quote at {i} in {escaped}");
            }
        }
        // A trailing backslash stays inside the literal.
        assert_eq!(applescript_escape(r"name\"), r"name\\");
    }

    use super::*;

    /// The hole DEV-605 closes: a brief written 0644 is readable by every
    /// process running as the user — every MCP server, every dependency
    /// postinstall, every tool any dispatched agent runs.
    #[test]
    fn tightening_makes_world_readable_event_files_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("allele-perms-{}-loose", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("brief.prompt");
        fs::write(&path, b"client context").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let tightened = restrict_events_dir(&dir);

        assert_eq!(tightened, 1);
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file left at {mode:o}");
        let dir_mode = fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "dir left at {dir_mode:o}");

        fs::remove_dir_all(&dir).ok();
    }

    /// Runs on every launch, so it must not report work it did not do — and
    /// must not disturb a file that is already private.
    #[test]
    fn tightening_leaves_already_private_files_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("allele-perms-{}-tight", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("brief.prompt");
        fs::write(&path, b"already private").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");

        assert_eq!(restrict_events_dir(&dir), 0);
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file left at {mode:o}");

        fs::remove_dir_all(&dir).ok();
    }

    /// The umask is what keeps *new* files owner-only; the startup sweep only
    /// repairs history. A receiver script without it silently reopens the hole
    /// for every session created from then on.
    ///
    /// Asserted on the *rendered* script rather than the template: the template
    /// is no longer what reaches disk, and a test that stopped following the
    /// renderer would stop testing the file that actually runs.
    #[test]
    fn the_receiver_script_sets_an_owner_only_umask() {
        assert!(
            build_receiver_script(Path::new("/tmp/any/events")).contains("umask 077"),
            "the generated receiver must not write group- or world-readable event files"
        );
    }

    /// A fresh directory under the temp dir, unique per process and per tag so
    /// `cargo test`'s threads cannot collide over one path.
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("allele-dev951-{}-{tag}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// The regression DEV-951 fixed. The receiver hard-coded
    /// `$HOME/.allele/events`, so a redirected instance wrote its sessions'
    /// events into the **live** tree while watching its own, which stayed empty
    /// — and `dispatch::create` confirms a create by observing a status change
    /// those events drive, so every create into a redirected instance spent its
    /// whole 120s budget and returned `prompt_delivery_unconfirmed`.
    ///
    /// Pinned against a redirected root, because a root that happens to be the
    /// real home cannot tell the two behaviours apart — which is exactly why
    /// the bug survived.
    #[test]
    fn the_installed_receiver_names_the_roots_own_events_directory() {
        let root = unique_dir("install");
        let base = root.join(".allele");

        let hooks_path = install_in(&base).expect("install should succeed");

        let script = fs::read_to_string(receiver_script_path_in(&base)).expect("receiver on disk");
        let events = events_dir_in(&base);
        assert!(
            script.contains(&format!("events_dir='{}'", events.display())),
            "the receiver does not name {}",
            events.display(),
        );
        // Comment lines are exempt on purpose: the script documents the literal
        // it used to carry, and that history is worth keeping readable. What must
        // not come back is a *resolved* path built out of the home directory.
        let home_in_code = script
            .lines()
            .find(|l| l.contains("$HOME") && !l.trim_start().starts_with('#'));
        assert!(
            home_in_code.is_none(),
            "the receiver still resolves a directory out of $HOME: {home_in_code:?}",
        );
        assert!(
            !script.contains(EVENTS_DIR_PLACEHOLDER),
            "the placeholder was left unsubstituted — every event would land in \
             a directory named after it, and nothing would say so",
        );

        // hooks.json names the receiver under *this* root, and the version
        // marker is deliberately untouched: nothing here is conditional on it,
        // so an existing install picks the fix up on its next launch.
        let hooks: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hooks_path).expect("hooks.json"))
                .expect("hooks.json is json");
        assert_eq!(hooks["_allele_version"], 5, "no version bump is needed");
        let command = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("a session_start command");
        assert_eq!(
            command,
            format!(
                "'{}' session_start",
                receiver_script_path_in(&base).display()
            ),
        );

        fs::remove_dir_all(&root).ok();
    }

    /// Text assertions pin what the script *says*. This pins what it *does*:
    /// run the installed receiver on a real hook payload and look for the event
    /// file under the redirected root.
    ///
    /// The root deliberately carries a space and a single quote. A data root is
    /// whatever `--home` was handed, and an unquoted substitution would either
    /// split the assignment or write somewhere nobody named.
    #[test]
    fn the_installed_receiver_writes_into_the_redirected_root() {
        let root = unique_dir("exec dir with a space and an 'apostrophe'");
        let base = root.join(".allele");
        install_in(&base).expect("install should succeed");

        let mut child = std::process::Command::new(receiver_script_path_in(&base))
            .arg("session_start")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the receiver");
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("receiver stdin")
                .write_all(br#"{"session_id":"dev951-probe","cwd":"/tmp/work"}"#)
                .expect("write the payload");
        }
        let out = child.wait_with_output().expect("receiver should exit");
        assert!(
            out.status.success(),
            "receiver exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
        );

        let written = events_dir_in(&base).join("dev951-probe.jsonl");
        let line = fs::read_to_string(&written)
            .unwrap_or_else(|e| panic!("no event file at {}: {e}", written.display()));
        assert!(
            line.contains(r#""kind":"session_start""#),
            "unexpected event line: {line}",
        );

        fs::remove_dir_all(&root).ok();
    }

    /// An ordinary launch must be unchanged. `<home>/.allele/events` is exactly
    /// where the old literal `$HOME/.allele/events` expanded to, so the rendered
    /// script names the same directory every current install already writes to.
    ///
    /// Asserted on the rendered script rather than by installing: installing
    /// against the real home would write over the user's live hooks, and a test
    /// that disturbs the instance you are developing in is its own bug.
    #[test]
    fn an_ordinary_launch_still_names_the_home_events_directory() {
        let home = dirs::home_dir().expect("test host has a home directory");
        let script = build_receiver_script(&events_dir_in(&crate::paths::allele_dir_in(&home)));

        assert!(
            script.contains(&format!("events_dir='{}/.allele/events'", home.display())),
            "an ordinary launch would stop writing where it always has",
        );
    }

    /// A root with a space broke the chain by a second route, found in review of
    /// this change rather than in the field. Claude Code evaluates a hook
    /// `command` through a shell, so an unquoted receiver path word-split into a
    /// path that does not exist, no hook fired at all, and the symptom was
    /// indistinguishable from DEV-951's own: no status transitions and every
    /// create burning its 120s budget.
    ///
    /// Driven through `sh -c`, the way Claude Code drives it. The sibling test
    /// above uses `Command::new`, which does no word-splitting and therefore
    /// cannot see this — on its own it would have given false assurance that a
    /// spaced root works end to end.
    #[test]
    fn the_hook_command_survives_a_root_with_a_space() {
        let root = unique_dir("root with a space");
        let base = root.join(".allele");
        let hooks_path = install_in(&base).expect("install should succeed");

        let hooks: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hooks_path).expect("hooks.json"))
                .expect("hooks.json is json");
        let command = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .expect("a session_start command")
            .to_string();

        let payload = r#"{"session_id":"spaced"}"#;
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' '{payload}' | {command}"))
            .output()
            .expect("run the hook command through a shell");
        assert!(
            out.status.success(),
            "the shell could not run {command}: {}",
            String::from_utf8_lossy(&out.stderr),
        );

        let written = events_dir_in(&base).join("spaced.jsonl");
        assert!(
            written.exists(),
            "the hook command never reached the receiver: {command}",
        );

        fs::remove_dir_all(&root).ok();
    }

    /// `--home` takes an arbitrary path and the receiver is a shell script, so a
    /// space, a `$` or a quote in the root must not change where it writes or
    /// what it evaluates.
    #[test]
    fn a_hostile_root_is_quoted_rather_than_expanded() {
        assert_eq!(shell_quote(Path::new("/tmp/plain")), "'/tmp/plain'");
        assert_eq!(
            shell_quote(Path::new("/tmp/with space")),
            "'/tmp/with space'"
        );
        // Would otherwise expand against the agent's environment at run time.
        assert_eq!(shell_quote(Path::new("/tmp/$HOME/x")), "'/tmp/$HOME/x'");
        assert_eq!(shell_quote(Path::new("/tmp/`id`")), "'/tmp/`id`'");
        // The one character single-quoting cannot carry: close, escape, reopen.
        assert_eq!(shell_quote(Path::new("/tmp/it's")), r"'/tmp/it'\''s'");
    }

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

    // ── Status-line wrapper (DEV-1259) ─────────────────────────────

    /// A scratch layout: an Allele root, a Claude config dir and a project.
    struct StatusLineRig {
        _dir: tempfile::TempDir,
        base: PathBuf,
        config: PathBuf,
        project: PathBuf,
    }

    impl StatusLineRig {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            // A space in every path, as a real `--home` root may have.
            let base = dir.path().join("root dir").join(".allele");
            let config = dir.path().join("claude config");
            let project = dir.path().join("my project");
            fs::create_dir_all(&config).unwrap();
            fs::create_dir_all(project.join(".claude")).unwrap();
            install_in(&base).expect("install should succeed");
            Self {
                _dir: dir,
                base,
                config,
                project,
            }
        }

        fn payload(&self) -> String {
            format!(
                r#"{{"session_id":"s1","workspace":{{"project_dir":{}}},"rate_limits":{{"five_hour":{{"used_percentage":71,"resets_at":2000}}}}}}"#,
                serde_json::to_string(&self.project.to_string_lossy()).unwrap()
            )
        }

        fn set_status_line(&self, settings: &Path, command: &str) {
            let v = serde_json::json!({"statusLine": {"type": "command", "command": command}});
            fs::write(settings, v.to_string()).unwrap();
        }

        /// Run the installed wrapper on `payload`; (stdout, success).
        fn run(&self, payload: &str) -> (String, bool) {
            use std::io::Write;
            let mut child = std::process::Command::new(status_line_script_path_in(&self.base))
                .env("CLAUDE_CONFIG_DIR", &self.config)
                .env("HOME", self._dir.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn the wrapper");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            (
                String::from_utf8_lossy(&out.stdout).into_owned(),
                out.status.success(),
            )
        }
    }

    #[test]
    fn the_wrapper_records_the_payload_for_allele() {
        let rig = StatusLineRig::new();
        let payload = rig.payload();
        let (_, ok) = rig.run(&payload);
        assert!(ok);
        let written = fs::read_to_string(status_line_payload_path_in(&rig.base)).unwrap();
        assert_eq!(written, payload);
    }

    #[test]
    fn a_user_status_line_renders_unchanged_through_the_wrapper() {
        let rig = StatusLineRig::new();
        // The user's command sees the exact payload on stdin, and its output
        // is passed through byte for byte — colours, newlines and all.
        rig.set_status_line(
            &rig.config.join("settings.json"),
            r#"printf '\033[32muser\033[0m %s\n' "$(cat | wc -c | tr -d ' ')""#,
        );
        let payload = rig.payload();
        let (out, ok) = rig.run(&payload);
        assert!(ok);
        assert_eq!(out, format!("\x1b[32muser\x1b[0m {}\n", payload.len()));
    }

    #[test]
    fn the_wrapper_follows_claude_codes_settings_precedence() {
        let rig = StatusLineRig::new();
        let claude = rig.project.join(".claude");
        rig.set_status_line(&rig.config.join("settings.json"), "printf user");
        assert_eq!(rig.run(&rig.payload()).0, "user");
        rig.set_status_line(&claude.join("settings.json"), "printf project");
        assert_eq!(rig.run(&rig.payload()).0, "project");
        rig.set_status_line(&claude.join("settings.local.json"), "printf local");
        assert_eq!(rig.run(&rig.payload()).0, "local");
    }

    #[test]
    fn with_no_user_status_line_the_wrapper_prints_nothing() {
        let rig = StatusLineRig::new();
        let (out, ok) = rig.run(&rig.payload());
        assert!(ok);
        assert_eq!(out, "");
        // Garbage in is still a clean, silent exit.
        let (out, ok) = rig.run("not json");
        assert!(ok);
        assert_eq!(out, "");
    }

    #[test]
    fn the_wrapper_never_runs_itself() {
        let rig = StatusLineRig::new();
        let me = shell_quote(&status_line_script_path_in(&rig.base));
        rig.set_status_line(&rig.config.join("settings.json"), &me);
        let (out, ok) = rig.run(&rig.payload());
        assert!(ok);
        assert_eq!(out, "");
    }

    #[test]
    fn the_status_line_entry_keeps_the_users_display_options() {
        let wrapper = Path::new("/r d/.allele/bin/status-line.sh");
        let user = serde_json::json!({"type": "command", "command": "mine.sh", "padding": 2});
        let hooks = build_hooks_json(Path::new("/r d/recv.sh"), wrapper, Some(&user));
        let entry = &hooks["statusLine"];
        assert_eq!(entry["command"], "'/r d/.allele/bin/status-line.sh'");
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["padding"], 2);

        let bare = build_hooks_json(Path::new("/r/recv.sh"), wrapper, None);
        assert_eq!(
            bare["statusLine"],
            serde_json::json!({"type": "command", "command": "'/r d/.allele/bin/status-line.sh'"})
        );
    }
}
