//! Key-binding loader.
//!
//! The built-in bindings are the `default_keys` of each row in the command
//! table (`crate::commands`); there is no second list. Users may override any
//! binding by creating `~/.allele/keymap.json`, merged on top of the defaults:
//!
//! ```json
//! [
//!   { "context": null,         "bindings": { "cmd-k": "allele.open_command_palette" } },
//!   { "context": "ComposeBar", "bindings": { "cmd-enter": "compose.insert_newline" } }
//! ]
//! ```
//!
//! Entries may carry an explicit `"context": "…"` string (scoped to that key
//! context, e.g. only fires when a ComposeBar has focus) or `"context": null` /
//! the field omitted (app-wide, fires anywhere). Keystrokes use GPUI syntax;
//! values are command names from the table.
//!
//! ## Merge semantics
//!
//! Bindings are deduplicated by `(context, keystroke)`. If a user entry
//! specifies a keystroke already bound by a default, the user's command wins
//! and the default is discarded. All other defaults (including other bindings
//! in the same context) are untouched: the merge is keystroke-granular, not
//! entry-granular.
//!
//! ## Error handling
//!
//! - Missing `~/.allele/keymap.json` → silent no-op; defaults apply.
//! - Malformed user file → reported; defaults apply.
//! - Unknown command name → that one binding is skipped and reported, and the
//!   rest of the file still applies (DEV-1264). It used to panic at launch.
//!
//! "Reported" means a `warn!` in the log and a banner in the main window (see
//! [`load_report`]), so a typo is visible rather than a silently dead key.
//!
//! Renames ship with a deprecation alias for 2 releases; see
//! `DEPRECATED_ALIASES` below.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::sync::OnceLock;

use gpui::{App, KeyBinding};
use serde::Deserialize;
use tracing::warn;

use crate::commands::{self, Command};

// ── Deprecation aliases ───────────────────────────────────────────
//
// When a command is renamed, add `("compose.old_name", "compose.new_name")`
// here. The loader rewrites old → new before lookup, so keymap files using
// the old name keep working for 2 releases. Remove entries after the
// deprecation window closes.
const DEPRECATED_ALIASES: &[(&str, &str)] = &[
    // Old Rich-Mode name kept working while users migrate their overrides.
    ("allele.toggle_rich_mode", "allele.toggle_transcript_tab"),
];

fn resolve_alias(name: &str) -> &str {
    for (old, new) in DEPRECATED_ALIASES {
        if *old == name {
            return new;
        }
    }
    name
}

/// The command a keymap file means by `name`, after resolving deprecation
/// aliases.
pub(crate) fn resolve(name: &str) -> Option<&'static Command> {
    commands::find(resolve_alias(name))
}

/// Every deprecated name still accepted, with the name it now means.
pub(crate) fn deprecated_aliases() -> &'static [(&'static str, &'static str)] {
    DEPRECATED_ALIASES
}

// ── JSON schema ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct KeymapEntry {
    #[serde(default)]
    context: Option<String>,
    bindings: BTreeMap<String, String>,
}

// ── Comment stripping ─────────────────────────────────────────────
//
// Serde's JSON parser rejects `//` comments. Strip them line-by-line
// before parsing. Strings in a keymap never contain `//`, so a simple
// first-`//`-to-end-of-line removal is safe. Block comments (`/* */`)
// are not supported.

fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let cut = line.find("//").unwrap_or(line.len());
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Parse a keymap file's text.
fn parse(src: &str) -> Result<Vec<KeymapEntry>, serde_json::Error> {
    serde_json::from_str(&strip_line_comments(src))
}

// ── Merge ─────────────────────────────────────────────────────────

/// One binding after the merge: where it applies, its keystroke, and the
/// command it runs.
struct Merged {
    context: Option<String>,
    keystroke: String,
    command: &'static Command,
}

/// Merge the table's defaults with the user's entries.
///
/// Pure, so the merge and the unknown-name reporting are testable without a
/// window. `source` names the user file in problem messages. Output is sorted
/// by `(context, keystroke)` so registration order is deterministic.
fn merge(user_entries: &[KeymapEntry], source: &str) -> (Vec<Merged>, Vec<String>) {
    let mut merged: HashMap<(Option<String>, String), &'static Command> = HashMap::new();
    for command in commands::COMMANDS {
        for key in command.default_keys {
            merged.insert(
                (command.context.map(str::to_string), key.to_string()),
                command,
            );
        }
    }

    let mut problems = Vec::new();
    for entry in user_entries {
        for (keystroke, name) in &entry.bindings {
            match resolve(name) {
                Some(command) => {
                    merged.insert((entry.context.clone(), keystroke.clone()), command);
                }
                None => problems.push(format!(
                    "{source}: unknown command \"{name}\" bound to {keystroke} — binding skipped"
                )),
            }
        }
    }

    let mut out: Vec<Merged> = merged
        .into_iter()
        .map(|((context, keystroke), command)| Merged {
            context,
            keystroke,
            command,
        })
        .collect();
    out.sort_by(|a, b| (&a.context, &a.keystroke).cmp(&(&b.context, &b.keystroke)));
    (out, problems)
}

// ── User override file ────────────────────────────────────────────

/// What reading `~/.allele/keymap.json` produced.
struct UserFile {
    entries: Vec<KeymapEntry>,
    /// The path, for problem messages.
    label: String,
    problems: Vec<String>,
}

/// Read `~/.allele/keymap.json` if it exists. A missing file is the normal
/// case and yields nothing; an unreadable or malformed one yields a problem
/// and no entries, so the defaults still apply.
fn load_user_overrides() -> UserFile {
    let mut file = UserFile {
        entries: Vec::new(),
        label: String::new(),
        problems: Vec::new(),
    };
    let Some(path) = crate::paths::keymap_file() else {
        return file;
    };
    file.label = path.display().to_string();
    match fs::read_to_string(&path) {
        Ok(raw) => match parse(&raw) {
            Ok(entries) => file.entries = entries,
            Err(err) => file.problems.push(format!(
                "{}: not valid keymap JSON ({err}) — using the defaults",
                file.label
            )),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => file.problems.push(format!(
            "{}: could not be read ({err}) — using the defaults",
            file.label
        )),
    }
    file
}

// ── Entry point ───────────────────────────────────────────────────

/// Problems found by the last [`load`], kept for the main window's banner.
static LOAD_PROBLEMS: OnceLock<Vec<String>> = OnceLock::new();

/// App-wide keystrokes bound to each command after the merge, for the
/// command palette's shortcut column.
static APP_KEYS: OnceLock<HashMap<&'static str, Vec<String>>> = OnceLock::new();

/// Merge the command table's default keys with any user overrides from
/// `~/.allele/keymap.json`, and register every binding with GPUI. Call once
/// during app startup.
pub fn load(cx: &mut App) {
    let user = load_user_overrides();
    let (merged, unknown) = merge(&user.entries, &user.label);
    let mut problems = user.problems;
    problems.extend(unknown);
    for problem in &problems {
        warn!("keymap: {problem}");
    }

    let mut app_keys: HashMap<&'static str, Vec<String>> = HashMap::new();
    for m in merged.iter().filter(|m| m.context.is_none()) {
        app_keys
            .entry(m.command.name)
            .or_default()
            .push(m.keystroke.clone());
    }
    let _ = APP_KEYS.set(app_keys);
    let _ = LOAD_PROBLEMS.set(problems);

    let bindings: Vec<KeyBinding> = merged
        .iter()
        .map(|m| m.command.binding(&m.keystroke, m.context.as_deref()))
        .collect();
    cx.bind_keys(bindings);
}

/// A one-message summary of what [`load`] had to skip, or `None` when the
/// keymap loaded cleanly. Shown as the main window's notice banner.
pub(crate) fn load_report() -> Option<String> {
    summarize(LOAD_PROBLEMS.get()?)
}

fn summarize(problems: &[String]) -> Option<String> {
    match problems {
        [] => None,
        [one] => Some(format!("Keymap: {one}")),
        [first, rest @ ..] => Some(format!(
            "Keymap: {first} (and {} more — see the log)",
            rest.len()
        )),
    }
}

/// The keystroke the command palette shows for `name`, as macOS glyphs
/// (`⌘⇧P`). Reflects the user's overrides; `None` when it is unbound.
pub(crate) fn shortcut_label(name: &str) -> Option<String> {
    let keys = APP_KEYS.get()?.get(name)?;
    // Prefer the built-in key when it is still bound, so the label is stable.
    let default = commands::find(name)
        .and_then(|c| c.default_keys.iter().find(|k| keys.iter().any(|b| b == *k)))
        .map(|k| k.to_string());
    default
        .or_else(|| keys.iter().min().cloned())
        .map(|k| glyphs(&k))
}

/// Render a GPUI keystroke (`cmd-shift-p`) the way macOS menus do (`⌘⇧P`).
fn glyphs(keystroke: &str) -> String {
    let mut parts: Vec<&str> = keystroke.split('-').collect();
    // `cmd--` binds the minus key: splitting leaves two empty parts.
    let key = match parts.pop() {
        Some("") => {
            parts.pop();
            "-"
        }
        Some(key) => key,
        None => "",
    };
    let mut out = String::new();
    for modifier in parts {
        out.push_str(match modifier {
            "ctrl" => "⌃",
            "alt" => "⌥",
            "shift" => "⇧",
            "cmd" => "⌘",
            other => other,
        });
    }
    match key {
        "enter" => out.push('↩'),
        "escape" => out.push('⎋'),
        "backspace" => out.push('⌫'),
        "delete" => out.push('⌦'),
        "tab" => out.push('⇥'),
        "left" => out.push('←'),
        "right" => out.push('→'),
        "up" => out.push('↑'),
        "down" => out.push('↓'),
        "space" => out.push_str("Space"),
        other => out.push_str(&other.to_uppercase()),
    }
    out
}

#[cfg(test)]
mod tests {
    // Import explicitly rather than `use super::*` — gpui's glob would shadow
    // the standard `#[test]` attribute with gpui's own `test` macro.
    use super::{glyphs, merge, parse, resolve, summarize};
    use crate::commands::COMMANDS;

    #[test]
    fn every_default_key_is_registered() {
        let (merged, problems) = merge(&[], "test");
        assert!(problems.is_empty());
        let total: usize = COMMANDS.iter().map(|c| c.default_keys.len()).sum();
        assert_eq!(merged.len(), total);
        assert!(merged.iter().any(|m| m.context.is_none()
            && m.keystroke == "cmd-shift-p"
            && m.command.name == "allele.open_command_palette"));
    }

    /// Acceptance (DEV-1264): an unknown command is reported at load, not
    /// silently ignored, and the rest of the file still applies.
    #[test]
    fn unknown_command_is_reported_and_skipped() {
        let user = parse(
            r#"[{ "context": null, "bindings": {
                "cmd-k": "allele.open_command_palette",
                "cmd-y": "allele.no_such_command"
            }}]"#,
        )
        .unwrap();
        let (merged, problems) = merge(&user, "/u/keymap.json");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("allele.no_such_command"));
        assert!(problems[0].contains("/u/keymap.json"));
        assert!(problems[0].contains("cmd-y"));
        assert!(!merged.iter().any(|m| m.keystroke == "cmd-y"));
        let cmd_k = merged
            .iter()
            .find(|m| m.context.is_none() && m.keystroke == "cmd-k")
            .unwrap();
        assert_eq!(cmd_k.command.name, "allele.open_command_palette");

        let banner = summarize(&problems).expect("a problem produces a banner");
        assert!(banner.contains("allele.no_such_command"));
    }

    #[test]
    fn clean_load_has_no_banner() {
        assert_eq!(summarize(&[]), None);
        let two = summarize(&["a".to_string(), "b".to_string()]).unwrap();
        assert!(two.contains("and 1 more"));
    }

    #[test]
    fn user_binding_replaces_only_its_own_key() {
        let user = parse(
            r#"[{ "context": "ComposeBar", "bindings": { "cmd-enter": "compose.insert_newline" } }]"#,
        )
        .unwrap();
        let (merged, problems) = merge(&user, "test");
        assert!(problems.is_empty());
        let at = |key: &str| {
            merged
                .iter()
                .find(|m| m.context.as_deref() == Some("ComposeBar") && m.keystroke == key)
                .map(|m| m.command.name)
        };
        assert_eq!(at("cmd-enter"), Some("compose.insert_newline"));
        assert_eq!(at("enter"), Some("compose.insert_newline"));
        assert_eq!(at("left"), Some("compose.cursor_left"));
    }

    #[test]
    fn deprecated_alias_still_resolves() {
        assert_eq!(
            resolve("allele.toggle_rich_mode").map(|c| c.name),
            Some("allele.toggle_transcript_tab")
        );
    }

    #[test]
    fn comments_are_stripped() {
        let user =
            parse("// mine\n[{ \"bindings\": { \"cmd-k\": \"allele.quit\" } }] // end").unwrap();
        assert_eq!(user.len(), 1);
    }

    #[test]
    fn glyphs_match_macos_menus() {
        assert_eq!(glyphs("cmd-shift-p"), "⌘⇧P");
        assert_eq!(glyphs("cmd-,"), "⌘,");
        assert_eq!(glyphs("ctrl-cmd-space"), "⌃⌘Space");
        assert_eq!(glyphs("cmd--"), "⌘-");
        assert_eq!(glyphs("alt-left"), "⌥←");
    }
}
