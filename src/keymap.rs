//! JSON-driven key-binding loader.
//!
//! Position C of the keymap architecture: `assets/default-keymap.json`
//! is the sole *built-in* binding surface for the entire app. Users may
//! override any binding by creating `~/.allele/keymap.json` with the
//! same schema — entries there are merged on top of the defaults.
//!
//! Entries may carry an explicit `"context": "…"` string (scoped to that
//! key context — e.g. only fires when a ComposeBar has focus) or
//! `"context": null` / the field omitted (app-wide, fires anywhere).
//!
//! ## Merge semantics
//!
//! Bindings are deduplicated by `(context, keystroke)`. If a user entry
//! specifies a keystroke already bound by a default, the user's action
//! wins and the default is discarded. All other defaults (including
//! other bindings in the same context) are untouched — the merge is
//! keystroke-granular, not entry-granular.
//!
//! ## Error handling
//!
//! - Missing `~/.allele/keymap.json` → silent no-op; defaults apply.
//! - Malformed user file → `tracing::warn!`; defaults apply.
//! - Unknown action name in either file → startup panic with the
//!   offending name and source file. Loud failure is preferred to
//!   silent binding loss; the user fixes the JSON and relaunches.
//!
//! Renames ship with a deprecation alias for 2 releases — see
//! `DEPRECATED_ALIASES` below.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use gpui::{App, KeyBinding};
use serde::Deserialize;
use tracing::warn;

use crate::rich::compose_bar as cb;
use crate::text_input as ti;
use crate::{
    CycleAttentionSession, OpenCommandPaletteAction, OpenFilePaletteAction, OpenScratchPadAction,
    OpenSearchAction, OpenSettings, Quit, ToggleActiveOnlyAction, ToggleDrawerAction,
    ToggleSidebarAction, ToggleTranscriptTabAction,
};

// ── Deprecation aliases ───────────────────────────────────────────
//
// When an action is renamed, add `("compose.old_name", "compose.new_name")`
// here. The loader rewrites old → new before dispatch, so keymap files
// using the old name keep working for 2 releases. Remove entries after
// the deprecation window closes.
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

// ── Source tagging ────────────────────────────────────────────────

/// Which keymap file a binding came from — used only for error messages
/// so the user knows which file to edit when a binding fails to resolve.
#[derive(Debug)]
enum KeymapSource {
    Default,
    User(PathBuf),
}

impl KeymapSource {
    fn label(&self) -> String {
        match self {
            KeymapSource::Default => "<built-in default-keymap.json>".to_string(),
            KeymapSource::User(path) => path.display().to_string(),
        }
    }
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
// before parsing. Strings in our keymap never contain `//`, so a simple
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

// ── Action table ──────────────────────────────────────────────────
//
// Every action a keymap file may name, with what it does. This table is
// the only way to make an action bindable, and every entry must carry a
// description, so an action cannot exist without being documented: the
// agent guide (`crate::agent_guide`) lists every row.
//
// Module aliases (`cb`, `ti`) disambiguate between `compose_bar` and
// `text_input` which share several struct names (SelectLeft, Paste, …).

/// One bindable action: its dotted name, a one-line description, and how
/// to build a GPUI binding for it.
pub(crate) struct KeymapAction {
    pub name: &'static str,
    pub description: &'static str,
    bind: fn(&str, Option<&str>) -> KeyBinding,
}

macro_rules! action {
    ($name:literal, $action:expr, $desc:literal) => {
        KeymapAction {
            name: $name,
            description: $desc,
            bind: |keystroke, ctx| KeyBinding::new(keystroke, $action, ctx),
        }
    };
}

/// Every bindable action, in the order the agent guide lists them.
pub(crate) const ACTIONS: &[KeymapAction] = &[
    action!(
        "compose.cursor_left",
        cb::CursorLeft,
        "Move the cursor one character left."
    ),
    action!(
        "compose.cursor_right",
        cb::CursorRight,
        "Move the cursor one character right."
    ),
    action!(
        "compose.cursor_up",
        cb::CursorUp,
        "Move the cursor up one line."
    ),
    action!(
        "compose.cursor_down",
        cb::CursorDown,
        "Move the cursor down one line."
    ),
    action!(
        "compose.cursor_word_left",
        cb::CursorWordLeft,
        "Move the cursor to the start of the previous word."
    ),
    action!(
        "compose.cursor_word_right",
        cb::CursorWordRight,
        "Move the cursor to the end of the next word."
    ),
    action!(
        "compose.cursor_line_start",
        cb::CursorLineStart,
        "Move the cursor to the start of the line."
    ),
    action!(
        "compose.cursor_line_end",
        cb::CursorLineEnd,
        "Move the cursor to the end of the line."
    ),
    action!(
        "compose.select_left",
        cb::SelectLeft,
        "Extend the selection one character left."
    ),
    action!(
        "compose.select_right",
        cb::SelectRight,
        "Extend the selection one character right."
    ),
    action!(
        "compose.select_up",
        cb::SelectUp,
        "Extend the selection up one line."
    ),
    action!(
        "compose.select_down",
        cb::SelectDown,
        "Extend the selection down one line."
    ),
    action!(
        "compose.select_word_left",
        cb::SelectWordLeft,
        "Extend the selection to the start of the previous word."
    ),
    action!(
        "compose.select_word_right",
        cb::SelectWordRight,
        "Extend the selection to the end of the next word."
    ),
    action!(
        "compose.select_line_start",
        cb::SelectLineStart,
        "Extend the selection to the start of the line."
    ),
    action!(
        "compose.select_line_end",
        cb::SelectLineEnd,
        "Extend the selection to the end of the line."
    ),
    action!(
        "compose.select_all",
        cb::SelectAll,
        "Select all text in the compose bar."
    ),
    action!(
        "compose.delete_left",
        cb::DeleteLeft,
        "Delete the character before the cursor (or the selection)."
    ),
    action!(
        "compose.delete_right",
        cb::DeleteRight,
        "Delete the character after the cursor (or the selection)."
    ),
    action!(
        "compose.delete_word_left",
        cb::DeleteWordLeft,
        "Delete back to the start of the previous word."
    ),
    action!(
        "compose.delete_word_right",
        cb::DeleteWordRight,
        "Delete forward to the end of the next word."
    ),
    action!(
        "compose.delete_line_left",
        cb::DeleteLineLeft,
        "Delete back to the start of the line."
    ),
    action!(
        "compose.copy",
        cb::Copy,
        "Copy the selection to the clipboard."
    ),
    action!(
        "compose.cut",
        cb::Cut,
        "Cut the selection to the clipboard."
    ),
    action!(
        "compose.paste",
        cb::Paste,
        "Paste from the clipboard (images become attachments)."
    ),
    action!(
        "compose.insert_newline",
        cb::InsertNewline,
        "Insert a newline without sending."
    ),
    action!(
        "compose.submit",
        cb::Submit,
        "Send the composed message to the agent."
    ),
    action!(
        "compose.show_character_palette",
        cb::ShowCharacterPalette,
        "Open the macOS emoji & symbols palette."
    ),
    action!(
        "compose.attach_files",
        cb::AttachFiles,
        "Open a file picker and attach the chosen files."
    ),
    action!(
        "text_input.backspace",
        ti::Backspace,
        "Delete the character before the cursor (or the selection)."
    ),
    action!(
        "text_input.delete",
        ti::Delete,
        "Delete the character after the cursor (or the selection)."
    ),
    action!(
        "text_input.delete_word_left",
        ti::DeleteWordLeft,
        "Delete back to the start of the previous word."
    ),
    action!(
        "text_input.delete_word_right",
        ti::DeleteWordRight,
        "Delete forward to the end of the next word."
    ),
    action!(
        "text_input.delete_to_home",
        ti::DeleteToHome,
        "Delete back to the start of the field."
    ),
    action!(
        "text_input.left",
        ti::Left,
        "Move the cursor one character left."
    ),
    action!(
        "text_input.right",
        ti::Right,
        "Move the cursor one character right."
    ),
    action!(
        "text_input.word_left",
        ti::WordLeft,
        "Move the cursor to the start of the previous word."
    ),
    action!(
        "text_input.word_right",
        ti::WordRight,
        "Move the cursor to the end of the next word."
    ),
    action!(
        "text_input.select_left",
        ti::SelectLeft,
        "Extend the selection one character left."
    ),
    action!(
        "text_input.select_right",
        ti::SelectRight,
        "Extend the selection one character right."
    ),
    action!(
        "text_input.select_word_left",
        ti::SelectWordLeft,
        "Extend the selection to the start of the previous word."
    ),
    action!(
        "text_input.select_word_right",
        ti::SelectWordRight,
        "Extend the selection to the end of the next word."
    ),
    action!(
        "text_input.select_to_home",
        ti::SelectToHome,
        "Extend the selection to the start of the field."
    ),
    action!(
        "text_input.select_to_end",
        ti::SelectToEnd,
        "Extend the selection to the end of the field."
    ),
    action!(
        "text_input.select_all",
        ti::SelectAll,
        "Select all text in the field."
    ),
    action!("text_input.paste", ti::Paste, "Paste from the clipboard."),
    action!(
        "text_input.copy",
        ti::Copy,
        "Copy the selection to the clipboard."
    ),
    action!(
        "text_input.cut",
        ti::Cut,
        "Cut the selection to the clipboard."
    ),
    action!(
        "text_input.home",
        ti::Home,
        "Move the cursor to the start of the field."
    ),
    action!(
        "text_input.end",
        ti::End,
        "Move the cursor to the end of the field."
    ),
    action!(
        "text_input.show_character_palette",
        ti::ShowCharacterPalette,
        "Open the macOS emoji & symbols palette."
    ),
    action!(
        "text_input.submit",
        ti::Submit,
        "Confirm the field (the same as pressing its form's primary button)."
    ),
    action!(
        "allele.quit",
        Quit,
        "Quit Allele (asks first when sessions are running)."
    ),
    action!(
        "allele.toggle_sidebar",
        ToggleSidebarAction,
        "Show or hide the left sidebar."
    ),
    action!(
        "allele.toggle_active_only",
        ToggleActiveOnlyAction,
        "Show only sessions with a live agent in the sidebar, or show all."
    ),
    action!(
        "allele.toggle_drawer",
        ToggleDrawerAction,
        "Show or hide the bottom drawer terminals."
    ),
    action!(
        "allele.toggle_transcript_tab",
        ToggleTranscriptTabAction,
        "Switch the active session between its terminal and transcript tabs."
    ),
    action!(
        "allele.open_settings",
        OpenSettings,
        "Open the Settings window."
    ),
    action!(
        "allele.open_scratch_pad",
        OpenScratchPadAction,
        "Open the scratch pad for drafting a prompt."
    ),
    action!(
        "allele.open_file_palette",
        OpenFilePaletteAction,
        "Open the file palette for the active session's workspace."
    ),
    action!(
        "allele.open_search",
        OpenSearchAction,
        "Open search across the active session's workspace."
    ),
    action!(
        "allele.open_command_palette",
        OpenCommandPaletteAction,
        "Open the command palette."
    ),
    action!(
        "allele.cycle_attention",
        CycleAttentionSession,
        "Jump to the next session that needs your attention."
    ),
];

/// The table row for `name`, after resolving deprecation aliases.
pub(crate) fn find_action(name: &str) -> Option<&'static KeymapAction> {
    let resolved = resolve_alias(name);
    ACTIONS.iter().find(|a| a.name == resolved)
}

/// Every deprecated name still accepted, with the name it now means.
pub(crate) fn deprecated_aliases() -> &'static [(&'static str, &'static str)] {
    DEPRECATED_ALIASES
}

// ── Action dispatch ───────────────────────────────────────────────
//
// Unknown names panic with a descriptive message so config typos fail
// loudly at startup, not silently at keystroke time.

fn build_binding(
    action_name: &str,
    keystroke: &str,
    context: Option<&str>,
    source: &KeymapSource,
) -> KeyBinding {
    match find_action(action_name) {
        Some(action) => (action.bind)(keystroke, context),
        None => panic!(
            "keymap: unknown action '{action_name}' in {} (keystroke='{keystroke}', context={context:?})",
            source.label()
        ),
    }
}

// ── User override file ────────────────────────────────────────────

fn user_keymap_path() -> Option<PathBuf> {
    crate::paths::keymap_file()
}

/// Read `~/.allele/keymap.json` if it exists. Returns an empty Vec on
/// missing file (the normal case) and on parse errors (with a stderr
/// warning) — errors in the user file must not prevent the app from
/// starting with defaults.
fn load_user_overrides() -> Vec<KeymapEntry> {
    let Some(path) = user_keymap_path() else {
        return Vec::new();
    };
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            warn!(
                "keymap: failed to read user overrides at {}: {err}",
                path.display()
            );
            return Vec::new();
        }
    };
    let stripped = strip_line_comments(&raw);
    match serde_json::from_str::<Vec<KeymapEntry>>(&stripped) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(
                "keymap: failed to parse user overrides at {}: {err}. Using defaults.",
                path.display()
            );
            Vec::new()
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────

const DEFAULT_KEYMAP_JSON: &str = include_str!("../assets/default-keymap.json");

/// One shipped binding: its key context (`None` = app-wide), keystroke and
/// action name.
pub(crate) struct DefaultBinding {
    pub context: Option<String>,
    pub keystroke: String,
    pub action: String,
}

/// Every binding in the embedded default keymap, in file order within each
/// entry. Read by the agent guide.
pub(crate) fn default_bindings() -> Vec<DefaultBinding> {
    let stripped = strip_line_comments(DEFAULT_KEYMAP_JSON);
    // `load` already panics on an unparseable default keymap (and a test
    // pins that it parses), so an empty list here is unreachable in practice.
    let entries: Vec<KeymapEntry> = serde_json::from_str(&stripped).unwrap_or_default();
    entries
        .into_iter()
        .flat_map(|entry| {
            let context = entry.context;
            entry
                .bindings
                .into_iter()
                .map(move |(keystroke, action)| DefaultBinding {
                    context: context.clone(),
                    keystroke,
                    action,
                })
        })
        .collect()
}

/// Parse the embedded default keymap, merge any user overrides from
/// `~/.allele/keymap.json`, and register every binding with GPUI.
/// Call once during app startup.
pub fn load(cx: &mut App) {
    let stripped = strip_line_comments(DEFAULT_KEYMAP_JSON);
    let default_entries: Vec<KeymapEntry> = serde_json::from_str(&stripped)
        .expect("keymap: failed to parse assets/default-keymap.json");
    let user_entries = load_user_overrides();

    // Deduplicate by (context, keystroke). Defaults go in first; user
    // overrides follow and `insert` returns the old value — we discard
    // it, letting late insertions win. This gives per-keystroke
    // override granularity without relying on GPUI's dispatch order.
    //
    // The value tuple carries the action name plus the source, so the
    // eventual panic message (if any) points at the offending file.
    let mut merged: HashMap<(Option<String>, String), (String, KeymapSource)> = HashMap::new();
    let user_path = user_keymap_path();

    for entry in &default_entries {
        for (keystroke, action_name) in &entry.bindings {
            merged.insert(
                (entry.context.clone(), keystroke.clone()),
                (action_name.clone(), KeymapSource::Default),
            );
        }
    }
    for entry in &user_entries {
        for (keystroke, action_name) in &entry.bindings {
            let source = match &user_path {
                Some(p) => KeymapSource::User(p.clone()),
                None => KeymapSource::Default, // unreachable in practice
            };
            merged.insert(
                (entry.context.clone(), keystroke.clone()),
                (action_name.clone(), source),
            );
        }
    }

    let bindings: Vec<KeyBinding> = merged
        .into_iter()
        .map(|((context, keystroke), (action_name, source))| {
            build_binding(&action_name, &keystroke, context.as_deref(), &source)
        })
        .collect();

    cx.bind_keys(bindings);
}

#[cfg(test)]
mod tests {
    // Import explicitly rather than `use super::*` — gpui's glob would shadow
    // the standard `#[test]` attribute with gpui's own `test` macro.
    use super::{
        build_binding, strip_line_comments, KeymapEntry, KeymapSource, DEFAULT_KEYMAP_JSON,
    };

    /// Every action name in the shipped keymap must resolve in `build_binding`.
    ///
    /// An unresolvable name panics inside `keymap::load`, which runs during
    /// app startup — so a typo here is a launch failure, not a dead keystroke.
    /// This test moves that failure to CI.
    #[test]
    fn default_keymap_resolves_every_action() {
        let stripped = strip_line_comments(DEFAULT_KEYMAP_JSON);
        let entries: Vec<KeymapEntry> =
            serde_json::from_str(&stripped).expect("default keymap must parse");
        assert!(!entries.is_empty(), "default keymap must not be empty");

        for entry in &entries {
            for (keystroke, action_name) in &entry.bindings {
                // Panics on an unknown action name — that is the assertion.
                build_binding(
                    action_name,
                    keystroke,
                    entry.context.as_deref(),
                    &KeymapSource::Default,
                );
            }
        }
    }
}
