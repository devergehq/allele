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
//! - Malformed user file → `eprintln!` warning; defaults apply.
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

use crate::rich::compose_bar as cb;
use crate::text_input as ti;
use crate::{
    OpenScratchPadAction, OpenSettings, Quit, ToggleDrawerAction, ToggleSidebarAction,
    ToggleTranscriptTabAction,
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

// ── Action dispatch ───────────────────────────────────────────────
//
// Maps the dotted snake_case action name from JSON to a concrete
// `KeyBinding::new(...)` call with the typed action struct. Every
// action name referenced by any keymap file must appear here. Unknown
// names panic with a descriptive message so config typos fail loudly at
// startup, not silently at keystroke time.
//
// Module aliases (`cb`, `ti`) disambiguate between `compose_bar` and
// `text_input` which share several struct names (SelectLeft, Paste, …).

fn build_binding(
    action_name: &str,
    keystroke: &str,
    context: Option<&str>,
    source: &KeymapSource,
) -> KeyBinding {
    let resolved = resolve_alias(action_name);
    let ctx = context;
    match resolved {
        // ── compose.* (ComposeBar / Rich Mode) ───────────────────
        // Cursor movement
        "compose.cursor_left" => KeyBinding::new(keystroke, cb::CursorLeft, ctx),
        "compose.cursor_right" => KeyBinding::new(keystroke, cb::CursorRight, ctx),
        "compose.cursor_up" => KeyBinding::new(keystroke, cb::CursorUp, ctx),
        "compose.cursor_down" => KeyBinding::new(keystroke, cb::CursorDown, ctx),
        "compose.cursor_word_left" => KeyBinding::new(keystroke, cb::CursorWordLeft, ctx),
        "compose.cursor_word_right" => KeyBinding::new(keystroke, cb::CursorWordRight, ctx),
        "compose.cursor_line_start" => KeyBinding::new(keystroke, cb::CursorLineStart, ctx),
        "compose.cursor_line_end" => KeyBinding::new(keystroke, cb::CursorLineEnd, ctx),
        // Selection
        "compose.select_left" => KeyBinding::new(keystroke, cb::SelectLeft, ctx),
        "compose.select_right" => KeyBinding::new(keystroke, cb::SelectRight, ctx),
        "compose.select_up" => KeyBinding::new(keystroke, cb::SelectUp, ctx),
        "compose.select_down" => KeyBinding::new(keystroke, cb::SelectDown, ctx),
        "compose.select_word_left" => KeyBinding::new(keystroke, cb::SelectWordLeft, ctx),
        "compose.select_word_right" => KeyBinding::new(keystroke, cb::SelectWordRight, ctx),
        "compose.select_line_start" => KeyBinding::new(keystroke, cb::SelectLineStart, ctx),
        "compose.select_line_end" => KeyBinding::new(keystroke, cb::SelectLineEnd, ctx),
        "compose.select_all" => KeyBinding::new(keystroke, cb::SelectAll, ctx),
        // Deletion
        "compose.delete_left" => KeyBinding::new(keystroke, cb::DeleteLeft, ctx),
        "compose.delete_right" => KeyBinding::new(keystroke, cb::DeleteRight, ctx),
        "compose.delete_word_left" => KeyBinding::new(keystroke, cb::DeleteWordLeft, ctx),
        "compose.delete_word_right" => KeyBinding::new(keystroke, cb::DeleteWordRight, ctx),
        "compose.delete_line_left" => KeyBinding::new(keystroke, cb::DeleteLineLeft, ctx),
        // Clipboard
        "compose.copy" => KeyBinding::new(keystroke, cb::Copy, ctx),
        "compose.cut" => KeyBinding::new(keystroke, cb::Cut, ctx),
        "compose.paste" => KeyBinding::new(keystroke, cb::Paste, ctx),
        // Newline / submit / palette
        "compose.insert_newline" => KeyBinding::new(keystroke, cb::InsertNewline, ctx),
        "compose.submit" => KeyBinding::new(keystroke, cb::Submit, ctx),
        "compose.show_character_palette" => {
            KeyBinding::new(keystroke, cb::ShowCharacterPalette, ctx)
        }
        "compose.attach_files" => KeyBinding::new(keystroke, cb::AttachFiles, ctx),

        // ── text_input.* (Settings single-line input) ────────────
        "text_input.backspace" => KeyBinding::new(keystroke, ti::Backspace, ctx),
        "text_input.delete" => KeyBinding::new(keystroke, ti::Delete, ctx),
        "text_input.left" => KeyBinding::new(keystroke, ti::Left, ctx),
        "text_input.right" => KeyBinding::new(keystroke, ti::Right, ctx),
        "text_input.select_left" => KeyBinding::new(keystroke, ti::SelectLeft, ctx),
        "text_input.select_right" => KeyBinding::new(keystroke, ti::SelectRight, ctx),
        "text_input.select_all" => KeyBinding::new(keystroke, ti::SelectAll, ctx),
        "text_input.paste" => KeyBinding::new(keystroke, ti::Paste, ctx),
        "text_input.copy" => KeyBinding::new(keystroke, ti::Copy, ctx),
        "text_input.cut" => KeyBinding::new(keystroke, ti::Cut, ctx),
        "text_input.home" => KeyBinding::new(keystroke, ti::Home, ctx),
        "text_input.end" => KeyBinding::new(keystroke, ti::End, ctx),
        "text_input.show_character_palette" => {
            KeyBinding::new(keystroke, ti::ShowCharacterPalette, ctx)
        }
        "text_input.submit" => KeyBinding::new(keystroke, ti::Submit, ctx),

        // ── allele.* (app-wide) ──────────────────────────────────
        "allele.quit" => KeyBinding::new(keystroke, Quit, ctx),
        "allele.toggle_sidebar" => KeyBinding::new(keystroke, ToggleSidebarAction, ctx),
        "allele.toggle_drawer" => KeyBinding::new(keystroke, ToggleDrawerAction, ctx),
        "allele.toggle_transcript_tab" => {
            KeyBinding::new(keystroke, ToggleTranscriptTabAction, ctx)
        }
        "allele.open_settings" => KeyBinding::new(keystroke, OpenSettings, ctx),
        "allele.open_scratch_pad" => KeyBinding::new(keystroke, OpenScratchPadAction, ctx),

        unknown => panic!(
            "keymap: unknown action '{unknown}' in {} (keystroke='{keystroke}', context={context:?})",
            source.label()
        ),
    }
}

// ── User override file ────────────────────────────────────────────

fn user_keymap_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".allele").join("keymap.json"))
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
            eprintln!(
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
            eprintln!(
                "keymap: failed to parse user overrides at {}: {err}. Using defaults.",
                path.display()
            );
            Vec::new()
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────

const DEFAULT_KEYMAP_JSON: &str = include_str!("../assets/default-keymap.json");

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
