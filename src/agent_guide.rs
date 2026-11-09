//! The generated guide for agents: `~/.allele/AGENTS.md` (DEV-1263).
//!
//! A Claude session asked to change something in Allele — a keybinding, a
//! setting, the editor command — used to have nothing to read, so it guessed.
//! On every launch Allele now writes a guide into its data root, generated
//! from the code rather than written by hand: every user setting with its
//! type and default, where the keymap lives and every action it can bind, the
//! MCP tools, and which files an agent may edit versus which are Allele's
//! private state.
//!
//! The guide is rewritten only when its content changed, so its modified time
//! is stable across launches of the same build.
//!
//! **Keeping it complete.** Keymap actions cannot be undocumented: the action
//! table in `keymap.rs` is the only way to make one bindable and every row
//! carries a description. Settings are plain serde structs, so the tests below
//! read every struct's field list straight out of its `Deserialize` impl and
//! fail when a field has no row in [`SETTINGS`] (or a row names a field that
//! no longer exists). Adding a setting means adding a row here.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{info, warn};

use crate::settings::Settings;

// ── Documentation tables ──────────────────────────────────────────

/// One documented setting. `key` is the dotted path into the JSON file, with
/// `[]` marking an array element (`projects[].name`).
pub(crate) struct SettingDoc {
    pub key: &'static str,
    pub ty: &'static str,
    pub description: &'static str,
}

const fn doc(key: &'static str, ty: &'static str, description: &'static str) -> SettingDoc {
    SettingDoc {
        key,
        ty,
        description,
    }
}

/// Every field of `settings.json`, nested structs included.
pub(crate) const SETTINGS: &[SettingDoc] = &[
    // Window & layout
    doc("sidebar_width", "number", "Left sidebar width in points."),
    doc("font_size", "number", "Terminal and transcript font size in points."),
    doc("window_x", "number | null", "Last window x position. Written by Allele on move."),
    doc("window_y", "number | null", "Last window y position. Written by Allele on move."),
    doc("window_width", "number | null", "Last window width. Written by Allele on resize."),
    doc("window_height", "number | null", "Last window height. Written by Allele on resize."),
    doc("sidebar_visible", "bool", "Whether the left sidebar is shown."),
    doc("drawer_height", "number", "Bottom drawer height in points."),
    doc("drawer_visible", "bool", "Whether the bottom drawer is shown."),
    doc(
        "drawer_park_idle_mins",
        "integer",
        "Minutes a session must sit unfocused before its drawer terminals are killed to reclaim memory. `0` disables parking. Parking kills real processes (dev servers, workers).",
    ),
    doc("right_sidebar_visible", "bool", "Whether the right sidebar is shown."),
    doc("right_sidebar_width", "number", "Right sidebar width in points."),
    // Attention
    doc("sound_on_awaiting_input", "bool", "Play a sound when a session starts waiting for input (a permission prompt)."),
    doc("sound_on_response_ready", "bool", "Play a sound when a session finishes a response."),
    doc("notifications_enabled", "bool", "Post a macOS notification on attention events."),
    doc(
        "awaiting_input_sound_path",
        "string | null",
        "Sound file for awaiting-input. `null` means `/System/Library/Sounds/Hero.aiff`.",
    ),
    doc(
        "response_ready_sound_path",
        "string | null",
        "Sound file for response-ready. `null` means `/System/Library/Sounds/Glass.aiff`.",
    ),
    doc(
        "promote_attention_sessions",
        "bool",
        "Move sessions that need attention to the top of their project in the sidebar.",
    ),
    // Editor
    doc(
        "external_editor_command",
        "string | null",
        "Command for \"Open in External Editor\", run as `<cmd> <path>[:line[:col]]`; may include arguments (`code -g`). `null` means `subl`.",
    ),
    // Sessions
    doc(
        "session_cleanup_paths",
        "string[]",
        "Paths, relative to a new session's clone, deleted right after cloning (stale sockets, pid files, path-dependent build caches).",
    ),
    doc(
        "git_pull_before_new_session",
        "bool",
        "Run `git pull` in the project's source before cloning a new session. Failures are logged, not fatal.",
    ),
    doc(
        "keep_awake_while_working",
        "bool",
        "Prevent idle system sleep while any session is running or awaiting input. The display still sleeps.",
    ),
    // Integrations
    doc(
        "browser_integration_enabled",
        "bool",
        "Manage one Google Chrome tab per session via AppleScript.",
    ),
    doc(
        "base_infra_enabled",
        "bool",
        "Run the shared Traefik proxy and `allele` Docker network for HTTPS routing between sessions. Requires Docker.",
    ),
    doc(
        "tool_visibility",
        "object<string, bool>",
        "Per tool name (e.g. `\"Bash\"`): `true` collapses its calls into the transcript's activity rail, `false` always expands them. Absent tools use the built-in default.",
    ),
    // Sidebar view state
    doc(
        "sidebar_active_only",
        "bool",
        "Show only sessions with a live agent in the sidebar (toggle: `allele.toggle_active_only`).",
    ),
    doc(
        "attention_bar_collapsed",
        "bool",
        "Collapse the attention bar to its summary header.",
    ),
    doc(
        "sidebar_collapsed_projects",
        "string[]",
        "Ids of local projects folded in the sidebar.",
    ),
    doc(
        "sidebar_collapsed_machines",
        "string[]",
        "Peer ids of paired machines folded in the MACHINES section.",
    ),
    doc(
        "sidebar_collapsed_remote_projects",
        "string[]",
        "Remote projects folded under their heading in the MACHINES section.",
    ),
    doc(
        "sidebar_machines_on_top",
        "bool",
        "Place the MACHINES section above the local projects.",
    ),
    // Projects
    doc(
        "projects",
        "object[]",
        "The projects shown in the sidebar. Add projects from the app rather than by hand.",
    ),
    doc("projects[].id", "string", "Stable project id. Never change it."),
    doc("projects[].name", "string", "Display name; also names `~/.allele/projects/<name>/scripts/`."),
    doc("projects[].source_path", "string", "Absolute path to the project's source checkout."),
    doc("projects[].settings", "object", "Per-project clone, sync and orchestration settings."),
    doc(
        "projects[].settings.default_branch",
        "string | null",
        "Base branch for new sessions. `null` auto-detects from `refs/remotes/<remote>/HEAD`, falling back to `master`.",
    ),
    doc("projects[].settings.remote", "string | null", "Git remote to fetch and rebase against. `null` means `origin`."),
    doc(
        "projects[].settings.terminals",
        "object[]",
        "Drawer terminals opened when a session is created, one tab each.",
    ),
    doc("projects[].settings.terminals[].label", "string", "Drawer tab label."),
    doc(
        "projects[].settings.terminals[].command",
        "string",
        "Command run in the tab. `{{unique_port}}` and `{{folder}}` are substituted.",
    ),
    doc(
        "projects[].settings.startup",
        "string | null",
        "One-shot command run before the terminals start; it must finish before the session appears. Relative paths resolve against `~/.allele/projects/<name>/scripts/`.",
    ),
    doc(
        "projects[].settings.shutdown",
        "string | null",
        "One-shot command run when a session is discarded. Resolved like `startup`.",
    ),
    doc(
        "projects[].settings.env",
        "object<string, string>",
        "Environment variables for every process a session spawns. Values support `{{unique_port}}` and `{{folder}}`.",
    ),
    doc(
        "projects[].settings.path_prepend",
        "string[]",
        "Directories pushed onto the front of `PATH`, in order. Same substitution as `env`.",
    ),
    // Agents
    doc(
        "agents",
        "object[]",
        "Configured coding agents. Seeded with the built-in kinds on first load.",
    ),
    doc("agents[].id", "string", "Stable id, referenced by `default_agent` and `allele.json`'s `agent`."),
    doc("agents[].kind", "\"claude\" | \"opencode\" | \"generic\"", "Which adapter builds the command line."),
    doc("agents[].display_name", "string", "Name shown in the UI."),
    doc(
        "agents[].path",
        "string | null",
        "Absolute path to the binary. `null` means not detected / not configured.",
    ),
    doc("agents[].extra_args", "string[]", "Arguments appended to the ones the adapter builds."),
    doc("agents[].enabled", "bool", "Whether the agent is offered for new sessions."),
    doc(
        "default_agent",
        "string | null",
        "Id of the default agent. A project's `allele.json` `agent` field overrides it.",
    ),
    // Naming
    doc("naming", "object", "How session branches are named."),
    doc(
        "naming.mode",
        "\"auto\" | \"interactive\" | \"legacy\"",
        "`auto`: an LLM picks the name. `interactive`: show suggestions to choose from. `legacy`: keyword extraction, no network.",
    ),
    doc("naming.claude", "object", "Naming model used when the session's agent is Claude."),
    doc("naming.claude.model", "string | null", "Model id used for naming."),
    doc("naming.claude.api_base", "string | null", "Accepted but currently unused."),
    doc("naming.claude.api_key_env", "string | null", "Accepted but currently unused."),
    doc("naming.opencode", "object", "Naming model used when the session's agent is opencode."),
    doc("naming.opencode.model", "string | null", "Model id used for naming."),
    doc("naming.opencode.api_base", "string | null", "Accepted but currently unused."),
    doc("naming.opencode.api_key_env", "string | null", "Accepted but currently unused."),
    // Sync
    doc("sync", "object", "Cross-machine session sync store. No credentials are stored here."),
    doc("sync.bucket", "string | null", "S3 bucket name."),
    doc("sync.region", "string | null", "Bucket region (resolved from the bucket by the connection test)."),
    doc("sync.profile", "string | null", "Named AWS profile in `~/.aws/credentials`."),
    doc("sync.endpoint", "string | null", "Custom endpoint for S3-compatible stores (R2, MinIO)."),
    doc("sync.device_id", "string | null", "This device's id, generated once. Do not edit."),
    // Dispatch
    doc("dispatch", "object", "Limits for sessions created over the MCP control socket."),
    doc(
        "dispatch.max_depth",
        "integer",
        "Deepest nesting a dispatched session may be created at. `1`: a human's session may dispatch, its workers may not. `0` turns dispatch off.",
    ),
    doc(
        "dispatch.max_sessions",
        "integer",
        "Maximum dispatched sessions that exist at once, across all dispatchers. Human-started sessions are not counted.",
    ),
    // Remote
    doc("remote", "object", "Multi-machine identity and transport (Settings → Machines)."),
    doc("remote.machine_name", "string | null", "Name paired machines see. `null` means the computer name."),
    doc("remote.relay_url", "string | null", "Self-hosted `iroh-relay` URL. `null` means n0's public relays."),
    doc(
        "remote.allow_connections",
        "bool",
        "Allow paired machines to connect. Off until the first pairing turns it on.",
    ),
];

/// Every field of a project's `allele.json`. Every field is optional.
pub(crate) const PROJECT_CONFIG: &[SettingDoc] = &[
    doc(
        "terminals",
        "object[]",
        "Drawer terminals opened when a session is created.",
    ),
    doc("terminals[].label", "string", "Drawer tab label."),
    doc(
        "terminals[].command",
        "string",
        "Command run in the tab. `{{unique_port}}` and `{{folder}}` are substituted.",
    ),
    doc("preview", "object", "The session's preview page."),
    doc(
        "preview.url",
        "string",
        "URL opened for the session (browser integration). Supports the same substitution.",
    ),
    doc(
        "agent",
        "string",
        "Agent id (from `settings.json` `agents[].id`) for this project's sessions.",
    ),
    doc(
        "startup",
        "string",
        "One-shot command run before the terminals start. Empty means absent.",
    ),
    doc(
        "shutdown",
        "string",
        "One-shot command run when a session is discarded.",
    ),
    doc(
        "env",
        "object<string, string>",
        "Environment variables for every process a session spawns.",
    ),
    doc(
        "path_prepend",
        "string[]",
        "Directories pushed onto the front of `PATH`, in order.",
    ),
];

// ── Paths ─────────────────────────────────────────────────────────

/// The resolved locations the guide names. Passed in rather than read from
/// `crate::paths`, so a sandboxed root (`ALLELE_HOME`) gets a guide naming its
/// own files, and tests can render against fixed paths.
pub(crate) struct GuidePaths {
    pub allele_dir: PathBuf,
    pub settings_file: PathBuf,
}

impl GuidePaths {
    fn from_runtime() -> Option<Self> {
        Some(Self {
            allele_dir: crate::paths::allele_dir()?,
            settings_file: crate::paths::settings_file()?,
        })
    }

    fn in_dir(&self, name: &str) -> String {
        self.allele_dir.join(name).display().to_string()
    }
}

// ── Rendering ─────────────────────────────────────────────────────

/// Render the guide. Pure and deterministic: the same build and the same
/// paths always produce byte-identical output, which is what lets
/// [`write_if_changed`] leave the file alone across launches.
pub(crate) fn render(paths: &GuidePaths) -> String {
    let mut out = String::new();
    let settings = paths.settings_file.display();
    let keymap = paths.in_dir("keymap.json");

    let _ = writeln!(
        out,
        "# Allele — guide for agents\n\n\
         <!-- Generated by Allele {version} on every launch. Do not edit: changes are overwritten. -->\n\n\
         Allele is a macOS session manager for Claude Code. This file tells an agent \
         how to configure it correctly instead of guessing: which files it reads, \
         every setting, every keymap action, and the MCP tools. It is generated from \
         Allele's own code, so it matches the installed version ({version}).\n",
        version = env!("CARGO_PKG_VERSION"),
    );

    // Files
    out.push_str("## Files\n\n### Safe for an agent to edit\n\n");
    out.push_str("| Path | What | When it takes effect |\n|---|---|---|\n");
    let _ = writeln!(
        out,
        "| `{settings}` | User settings (see [Settings](#settings)). | Next launch. **Quit Allele first**: the running app rewrites the whole file from memory whenever a setting changes, which discards hand edits. |"
    );
    let _ = writeln!(
        out,
        "| `{keymap}` | Key binding overrides (see [Keymap](#keymap)). Create it if missing. | Next launch. |"
    );
    out.push_str("| `<project>/allele.json` | Per-project overrides, checked into the project (see [Project config](#project-config-allelejson)). | Next session created in that project. |\n");
    let _ = writeln!(
        out,
        "| `{}` | A project's `startup` / `shutdown` scripts. | Next session. |",
        paths.in_dir("projects/<name>/scripts/")
    );

    out.push_str("\n### Private to Allele — do not edit, move or delete\n\n");
    out.push_str("| Path | Why |\n|---|---|\n");
    let private: &[(&str, &str)] = &[
        ("state.json", "**Allele's private state** (sessions, workspaces, history). It is rewritten constantly while Allele runs; an edit is either lost or corrupts every session. Never touch it, or its `.bak` copies."),
        ("workspaces/", "Session workspaces (APFS clones). Discard a session from the app or with `allele_sessions_discard`; never `rm` one."),
        ("trash/", "Discarded workspaces awaiting cleanup."),
        ("peers.json", "Paired machines."),
        ("sync-ledger.json", "Session-sync bookkeeping."),
        ("hooks.json", "Claude Code hook settings; rewritten on every launch."),
        ("bin/", "Hook receiver scripts; rewritten on every launch."),
        ("events/", "Hook event inbox."),
        ("control.sock", "The MCP control socket. Use the MCP tools below, not the socket."),
        ("attachments/", "Files attached to composed messages."),
        ("crash/", "Crash reports."),
        ("diagnostics/", "Diagnostic captures."),
        ("debug/", "Debug captures."),
        ("AGENTS.md", "This file. Generated; edits are overwritten."),
    ];
    for (name, why) in private {
        let _ = writeln!(out, "| `{}` | {why} |", paths.in_dir(name));
    }

    // Settings
    let defaults = serde_json::to_value(Settings::default()).unwrap_or(Value::Null);
    let _ = writeln!(
        out,
        "\n## Settings\n\nFile: `{settings}`. JSON. Every field is optional; a missing field takes \
         its default. Unknown fields are ignored. Keys are dotted paths; `[]` marks an \
         array element. Most of these are also editable in the Settings window (⌘,).\n"
    );
    out.push_str("| Key | Type | Default | What it does |\n|---|---|---|---|\n");
    for s in SETTINGS {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            s.key,
            cell(s.ty),
            default_cell(&defaults, s.key, s.ty),
            cell(s.description)
        );
    }

    // Project config
    out.push_str(
        "\n## Project config (`allele.json`)\n\nAn optional `allele.json` at a project's root, \
         checked in so everyone who clones the repo gets it. When present it replaces the \
         project's orchestration in `settings.json` as a whole: `terminals`, `startup`, \
         `shutdown` and `preview` left out of it are absent, not inherited. `env` and \
         `path_prepend` fall back to `settings.json` when empty. A malformed file is ignored \
         with a warning.\n\n",
    );
    out.push_str("| Key | Type | What it does |\n|---|---|---|\n");
    for s in PROJECT_CONFIG {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            s.key,
            cell(s.ty),
            cell(s.description)
        );
    }

    // Keymap
    let _ = writeln!(
        out,
        "\n## Keymap\n\nFile: `{keymap}` (create it if missing). Same format as the built-in \
         defaults below: a JSON array of `{{\"context\": \"…\" | null, \"bindings\": {{ \"<keystroke>\": \"<action>\" }}}}`. \
         `//` line comments are allowed; block comments are not. Keystrokes use GPUI syntax \
         (`cmd-shift-p`, `alt-left`, `ctrl-cmd-space`). `context: null` binds app-wide; \
         `\"ComposeBar\"` and `\"TextInput\"` bind only while that input has focus.\n\n\
         Your entries are merged over the defaults per `(context, keystroke)`: binding a key \
         replaces the default at that key and leaves every other default alone. A malformed \
         file is ignored and the defaults apply. **An unknown action name stops Allele from \
         launching**, so use only names from the table below.\n\nExample — move the command \
         palette to ⌘K and the scratch pad to ⌘⇧K:\n\n```json\n[\n  {{\n    \"context\": null,\n    \"bindings\": {{\n      \"cmd-k\": \"allele.open_command_palette\",\n      \"cmd-shift-k\": \"allele.open_scratch_pad\"\n    }}\n  }}\n]\n```\n"
    );

    let defaults = crate::keymap::default_bindings();
    out.push_str(
        "### Bindable actions\n\n| Action | Default keys | What it does |\n|---|---|---|\n",
    );
    for action in crate::keymap::ACTIONS {
        let keys: Vec<String> = defaults
            .iter()
            .filter(|b| b.action == action.name)
            .map(|b| match &b.context {
                Some(ctx) => format!("`{}` ({ctx})", b.keystroke),
                None => format!("`{}`", b.keystroke),
            })
            .collect();
        let keys = if keys.is_empty() {
            "—".to_string()
        } else {
            keys.join(", ")
        };
        let _ = writeln!(
            out,
            "| `{}` | {keys} | {} |",
            action.name,
            cell(action.description)
        );
    }
    let aliases = crate::keymap::deprecated_aliases();
    if !aliases.is_empty() {
        out.push_str("\nDeprecated names, still accepted:\n\n");
        for (old, new) in aliases {
            let _ = writeln!(out, "- `{old}` → `{new}`");
        }
    }

    // MCP
    out.push_str(
        "\n## MCP tools\n\nAllele serves these tools to the sessions it starts (server name \
         `allele`). They are the supported way for an agent to list, create, inspect, interrupt \
         and discard sessions.\n",
    );
    if let Value::Array(tools) = crate::dispatch::mcp::tool_definitions() {
        for tool in &tools {
            let name = tool["name"].as_str().unwrap_or_default();
            let description = tool["description"].as_str().unwrap_or_default();
            let _ = writeln!(out, "\n### `{name}`\n\n{description}\n");
            let required: Vec<&str> = tool["inputSchema"]["required"]
                .as_array()
                .map(|r| r.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match tool["inputSchema"]["properties"].as_object() {
                Some(props) if !props.is_empty() => {
                    out.push_str(
                        "| Argument | Type | Required | What it is |\n|---|---|---|---|\n",
                    );
                    for (arg, spec) in props {
                        let ty = match spec["enum"].as_array() {
                            Some(values) => values
                                .iter()
                                .filter_map(Value::as_str)
                                .map(|v| format!("`{v}`"))
                                .collect::<Vec<_>>()
                                .join(" \\| "),
                            None => spec["type"].as_str().unwrap_or("any").to_string(),
                        };
                        let _ = writeln!(
                            out,
                            "| `{arg}` | {ty} | {} | {} |",
                            if required.contains(&arg.as_str()) {
                                "yes"
                            } else {
                                "no"
                            },
                            cell(spec["description"].as_str().unwrap_or_default())
                        );
                    }
                }
                _ => out.push_str("No arguments.\n"),
            }
        }
    }
    out
}

/// Escape a value for a Markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The default for `key`, read from `Settings::default()` so the guide can
/// never drift from the code. Keys under an array element have no single
/// default.
fn default_cell(defaults: &Value, key: &str, ty: &str) -> String {
    if key.contains("[]") {
        return "—".to_string();
    }
    let pointer = format!("/{}", key.replace('.', "/"));
    match defaults.pointer(&pointer) {
        // Absent from the serialized defaults: an empty collection skipped
        // by `skip_serializing_if`, or a field that is read but never written.
        None if ty.ends_with("[]") => "`[]`".to_string(),
        None => "unset".to_string(),
        Some(Value::Object(map)) if !map.is_empty() => "see below".to_string(),
        Some(value) => format!("`{}`", cell(&value.to_string())),
    }
}

// ── Writing ───────────────────────────────────────────────────────

/// Write `content` to `path` unless the file already holds exactly that.
/// Returns whether it wrote. Leaving an unchanged file alone keeps its
/// modified time stable, so nothing watching `~/.allele` sees churn on every
/// launch. The write itself is temp + rename, so a reader never sees half a
/// guide.
pub(crate) fn write_if_changed(path: &Path, content: &str) -> io::Result<bool> {
    if std::fs::read(path).ok().as_deref() == Some(content.as_bytes()) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Write the guide for this instance's data root. Called once at launch.
/// Failure is logged and swallowed: a missing guide must never stop Allele.
pub fn install() {
    let (Some(paths), Some(file)) = (
        GuidePaths::from_runtime(),
        crate::paths::agents_guide_file(),
    ) else {
        return;
    };
    match write_if_changed(&file, &render(&paths)) {
        Ok(true) => info!("agent guide: wrote {}", file.display()),
        Ok(false) => {}
        Err(e) => warn!("agent guide: failed to write {}: {e}", file.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::{self, DeserializeOwned, Visitor};
    use std::cell::Cell;
    use std::collections::BTreeSet;

    /// The field names a `#[derive(Deserialize)]` struct accepts, read from
    /// the derive itself: it hands its full field list to
    /// `deserialize_struct`, which this deserializer records and then
    /// refuses. Renames are already applied, so these are the JSON keys.
    fn serde_fields<T: DeserializeOwned>() -> &'static [&'static str] {
        struct Probe<'a>(&'a Cell<Option<&'static [&'static str]>>);

        impl<'de> de::Deserializer<'de> for Probe<'_> {
            type Error = de::value::Error;

            fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
                Err(de::Error::custom("not a struct"))
            }

            fn deserialize_struct<V: Visitor<'de>>(
                self,
                _name: &'static str,
                fields: &'static [&'static str],
                _visitor: V,
            ) -> Result<V::Value, Self::Error> {
                self.0.set(Some(fields));
                Err(de::Error::custom("probe"))
            }

            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                bytes byte_buf option unit unit_struct newtype_struct seq tuple
                tuple_struct map enum identifier ignored_any
            }
        }

        let seen = Cell::new(None);
        let _ = T::deserialize(Probe(&seen));
        seen.get()
            .expect("type must derive Deserialize as a struct")
    }

    /// Every serde struct reachable from `settings.json`, with the key prefix
    /// its fields sit under. A new nested struct must be added here — the
    /// `every_nested_settings_object_is_walked` test enforces that.
    fn settings_structs() -> Vec<(&'static str, &'static [&'static str])> {
        use crate::config::TerminalCfg;
        use crate::dispatch::admission::DispatchLimits;
        use crate::naming::{NamingConfig, NamingModelConfig};
        use crate::settings::{
            AgentConfig, ProjectSave, ProjectSettings, RemoteSettings, SyncSettings,
        };
        vec![
            ("", serde_fields::<Settings>()),
            ("projects[].", serde_fields::<ProjectSave>()),
            ("projects[].settings.", serde_fields::<ProjectSettings>()),
            (
                "projects[].settings.terminals[].",
                serde_fields::<TerminalCfg>(),
            ),
            ("agents[].", serde_fields::<AgentConfig>()),
            ("naming.", serde_fields::<NamingConfig>()),
            ("naming.claude.", serde_fields::<NamingModelConfig>()),
            ("naming.opencode.", serde_fields::<NamingModelConfig>()),
            ("sync.", serde_fields::<SyncSettings>()),
            ("dispatch.", serde_fields::<DispatchLimits>()),
            ("remote.", serde_fields::<RemoteSettings>()),
        ]
    }

    fn project_config_structs() -> Vec<(&'static str, &'static [&'static str])> {
        use crate::config::{PreviewCfg, ProjectConfig, TerminalCfg};
        vec![
            ("", serde_fields::<ProjectConfig>()),
            ("terminals[].", serde_fields::<TerminalCfg>()),
            ("preview.", serde_fields::<PreviewCfg>()),
        ]
    }

    fn keys_in_code(structs: Vec<(&'static str, &'static [&'static str])>) -> BTreeSet<String> {
        structs
            .into_iter()
            .flat_map(|(prefix, fields)| fields.iter().map(move |f| format!("{prefix}{f}")))
            .collect()
    }

    fn keys_in_docs(docs: &[SettingDoc]) -> BTreeSet<String> {
        docs.iter().map(|d| d.key.to_string()).collect()
    }

    fn sample_paths() -> GuidePaths {
        GuidePaths {
            allele_dir: PathBuf::from("/Users/agent/.allele"),
            settings_file: PathBuf::from("/Users/agent/.config/allele/settings.json"),
        }
    }

    #[test]
    fn every_setting_in_code_is_documented() {
        let missing: Vec<_> = keys_in_code(settings_structs())
            .difference(&keys_in_docs(SETTINGS))
            .cloned()
            .collect();
        assert!(
            missing.is_empty(),
            "settings.json fields with no row in agent_guide::SETTINGS: {missing:?}"
        );
    }

    #[test]
    fn every_documented_setting_exists_in_code() {
        let stale: Vec<_> = keys_in_docs(SETTINGS)
            .difference(&keys_in_code(settings_structs()))
            .cloned()
            .collect();
        assert!(
            stale.is_empty(),
            "documented settings that no longer exist: {stale:?}"
        );
    }

    #[test]
    fn every_project_config_field_is_documented_and_exists() {
        let code = keys_in_code(project_config_structs());
        let docs = keys_in_docs(PROJECT_CONFIG);
        assert_eq!(
            code, docs,
            "allele.json fields and agent_guide::PROJECT_CONFIG differ"
        );
    }

    /// Guards the walk itself: a new nested settings struct whose fields the
    /// tests above never look at would let its settings go undocumented.
    #[test]
    fn every_nested_settings_object_is_walked() {
        let defaults = serde_json::to_value(Settings::default()).unwrap();
        let walked: BTreeSet<String> = settings_structs()
            .into_iter()
            .map(|(prefix, _)| prefix.trim_end_matches('.').to_string())
            .collect();
        for (key, value) in defaults.as_object().unwrap() {
            if matches!(value, Value::Object(map) if !map.is_empty()) {
                assert!(
                    walked.contains(key),
                    "`{key}` is a nested struct; add it to settings_structs()"
                );
            }
        }
    }

    #[test]
    fn rendered_guide_names_every_setting() {
        let guide = render(&sample_paths());
        for s in SETTINGS.iter().chain(PROJECT_CONFIG) {
            assert!(
                guide.contains(&format!("| `{}` |", s.key)),
                "guide is missing `{}`",
                s.key
            );
        }
    }

    #[test]
    fn rendered_guide_names_every_keymap_action() {
        let guide = render(&sample_paths());
        for action in crate::keymap::ACTIONS {
            assert!(
                !action.description.trim().is_empty(),
                "`{}` has no description",
                action.name
            );
            assert!(
                guide.contains(&format!("| `{}` |", action.name)),
                "guide is missing keymap action `{}`",
                action.name
            );
        }
    }

    /// Every default binding names a documented action, so the "Default
    /// keys" column never silently drops one.
    #[test]
    fn every_default_binding_is_a_documented_action() {
        for b in crate::keymap::default_bindings() {
            assert!(
                crate::keymap::find_action(&b.action).is_some(),
                "default keymap binds unknown action `{}`",
                b.action
            );
        }
    }

    #[test]
    fn rendered_guide_names_every_mcp_tool() {
        let guide = render(&sample_paths());
        let tools = crate::dispatch::mcp::tool_definitions();
        for tool in tools.as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert!(
                guide.contains(&format!("### `{name}`")),
                "guide is missing tool `{name}`"
            );
        }
    }

    #[test]
    fn guide_names_state_json_as_off_limits() {
        let guide = render(&sample_paths());
        let private = guide
            .split("### Private to Allele")
            .nth(1)
            .expect("guide has a private-files section");
        let private = private.split("\n## ").next().unwrap();
        assert!(
            private.contains("`/Users/agent/.allele/state.json`"),
            "state.json must be listed under the private files"
        );
        let editable = guide
            .split("### Safe for an agent to edit")
            .nth(1)
            .unwrap()
            .split("### Private to Allele")
            .next()
            .unwrap();
        assert!(
            !editable.contains("state.json"),
            "state.json must not be listed as editable"
        );
    }

    #[test]
    fn guide_uses_the_given_paths() {
        let guide = render(&sample_paths());
        assert!(guide.contains("`/Users/agent/.config/allele/settings.json`"));
        assert!(guide.contains("`/Users/agent/.allele/keymap.json`"));
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(render(&sample_paths()), render(&sample_paths()));
    }

    #[test]
    fn defaults_come_from_code() {
        let guide = render(&sample_paths());
        assert!(
            guide.contains("| `sidebar_width` | number | `240.0` |"),
            "{guide}"
        );
        assert!(guide.contains("| `dispatch.max_sessions` | integer | `20` |"));
        assert!(guide.contains("| `sidebar_collapsed_projects` | string[] | `[]` |"));
        assert!(guide.contains("| `naming.claude.api_base` | string \\| null | unset |"));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("allele-agent-guide-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    /// Acceptance: a first launch writes the file.
    #[test]
    fn first_write_creates_the_file() {
        let dir = temp_dir("create");
        let file = dir.join("AGENTS.md");
        let content = render(&sample_paths());
        assert!(write_if_changed(&file, &content).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), content);
        assert!(
            !file.with_extension("md.tmp").exists(),
            "temp file left behind"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Acceptance: launching again with nothing changed leaves the modified
    /// time alone.
    #[test]
    fn unchanged_content_leaves_mtime_alone() {
        let dir = temp_dir("mtime");
        let file = dir.join("AGENTS.md");
        let content = render(&sample_paths());
        write_if_changed(&file, &content).unwrap();
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            !write_if_changed(&file, &content).unwrap(),
            "rewrote unchanged guide"
        );
        let after = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(before, after);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn changed_content_is_rewritten() {
        let dir = temp_dir("change");
        let file = dir.join("AGENTS.md");
        write_if_changed(&file, "old").unwrap();
        assert!(write_if_changed(&file, "new").unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }
}
