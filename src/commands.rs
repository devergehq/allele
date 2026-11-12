//! The command table (DEV-1264): every action Allele can bind, show in a
//! menu or run from the command palette, under one stable name.
//!
//! Before this, the keymap, the command palette and the menus each kept their
//! own list of actions. Now each command is one row here (its dotted name,
//! the GPUI action it dispatches, what it does, the key context it binds in,
//! and its default keys) and everything else reads the row:
//!
//! - **Keymap** (`crate::keymap`): the built-in bindings are each row's
//!   `default_keys`, and a name in `~/.allele/keymap.json` resolves here.
//! - **Command palette** (`crate::reader::command`): lists the rows that carry
//!   [`Palette`] info and runs one by dispatching its action.
//! - **Menus** (`crate::mac_menu`): name the rows they show.
//! - **Agent guide** (`crate::agent_guide`): lists every row.
//!
//! Adding a command is one row plus, for a new action, an App-scope handler
//! in `crate::startup::actions`.
//!
//! Names are stable. To rename one, keep the old name working through
//! `keymap::DEPRECATED_ALIASES` for two releases.

use gpui::{Action, KeyBinding};

use crate::app_state::AppState;
use crate::rich::compose_bar as cb;
use crate::text_input as ti;
use crate::{
    About, CaptureUi, CycleAttentionSession, OpenCommandPaletteAction, OpenFilePaletteAction,
    OpenScratchPadAction, OpenSearchAction, OpenSettings, Quit, ToggleActiveOnlyAction,
    ToggleDrawerAction, ToggleSidebarAction, ToggleTranscriptTabAction,
};

// Actions that existed only as command-palette cases until DEV-1264. Their
// handlers live in `crate::startup::actions` with the rest.
gpui::actions!(
    allele,
    [
        GoToClaudeTab,
        GoToReaderTab,
        GoToTranscriptTab,
        GoToBrowserTab,
        ToggleChangesPanel,
        NewSessionAction,
        DiscardSessionAction,
        ToggleDecisionHistory
    ]
);

/// How a command appears in the command palette.
pub(crate) struct Palette {
    /// Second line under the title.
    pub subtitle: Option<&'static str>,
    /// Extra terms folded into fuzzy matching (never shown).
    pub keywords: &'static str,
    /// Shown in the danger colour.
    pub destructive: bool,
    /// `Some(reason)` greys the row out and blocks running it.
    pub unavailable: fn(&AppState) -> Option<&'static str>,
}

/// One command.
pub(crate) struct Command {
    /// Stable dotted name, used in `keymap.json` (`allele.toggle_sidebar`).
    pub name: &'static str,
    /// Label in menus and the command palette.
    pub title: &'static str,
    /// One line on what it does, for the agent guide.
    pub description: &'static str,
    /// Key context the bindings apply in. `None` means app-wide.
    pub context: Option<&'static str>,
    /// Built-in keystrokes, GPUI syntax.
    pub default_keys: &'static [&'static str],
    /// `Some` lists the command in the command palette.
    pub palette: Option<Palette>,
    action: fn() -> Box<dyn Action>,
    bind: fn(&str, Option<&str>) -> KeyBinding,
}

impl Command {
    /// A fresh instance of the command's action, for dispatch or a menu item.
    pub(crate) fn action(&self) -> Box<dyn Action> {
        (self.action)()
    }

    /// A GPUI binding of `keystroke` to this command, in `context`.
    pub(crate) fn binding(&self, keystroke: &str, context: Option<&str>) -> KeyBinding {
        (self.bind)(keystroke, context)
    }
}

fn always(_: &AppState) -> Option<&'static str> {
    None
}

fn needs_session(state: &AppState) -> Option<&'static str> {
    state.active.is_none().then_some("No active session")
}

fn needs_project(state: &AppState) -> Option<&'static str> {
    state.active.is_none().then_some("Select a project first")
}

fn needs_browser_tab(state: &AppState) -> Option<&'static str> {
    (!state.browser_tab_available()).then_some("No linked Chrome tab for this session")
}

/// An app-wide command.
macro_rules! app {
    ($name:literal, $action:expr, $title:literal, [$($key:literal),*], $desc:literal $(, $palette:expr)?) => {
        Command {
            name: $name,
            title: $title,
            description: $desc,
            context: None,
            default_keys: &[$($key),*],
            palette: app!(@palette $($palette)?),
            action: || Box::new($action),
            bind: |keystroke, ctx| KeyBinding::new(keystroke, $action, ctx),
        }
    };
    (@palette) => { None };
    (@palette $palette:expr) => { Some($palette) };
}

/// A text-editing command, bound only while its input has focus.
macro_rules! edit {
    ($name:literal, $action:expr, $ctx:expr, [$($key:literal),*], $desc:literal) => {
        Command {
            name: $name,
            title: $name,
            description: $desc,
            context: Some($ctx),
            default_keys: &[$($key),*],
            palette: None,
            action: || Box::new($action),
            bind: |keystroke, ctx| KeyBinding::new(keystroke, $action, ctx),
        }
    };
}

const fn palette(
    subtitle: Option<&'static str>,
    keywords: &'static str,
    unavailable: fn(&AppState) -> Option<&'static str>,
) -> Palette {
    Palette {
        subtitle,
        keywords,
        destructive: false,
        unavailable,
    }
}

/// Every command, in the order the command palette and the agent guide list
/// them.
pub(crate) const COMMANDS: &[Command] = &[
    // ── Surfaces ──────────────────────────────────────────────────
    app!("allele.go_to_claude", GoToClaudeTab, "Go to Claude", [],
        "Show the active session's agent terminal.",
        palette(Some("Main tab"), "surface tab agent", always)),
    app!("allele.go_to_reader", GoToReaderTab, "Go to Reader", [],
        "Show the Reader tab (the session's files and source).",
        palette(Some("Files & source"), "surface tab files source read", always)),
    app!("allele.go_to_transcript", GoToTranscriptTab, "Go to Transcript", [],
        "Show the active session's transcript.",
        palette(Some("Session transcript"), "surface tab log history", always)),
    app!("allele.go_to_browser", GoToBrowserTab, "Go to Browser", [],
        "Show the session's linked Chrome tab (needs browser integration).",
        palette(Some("Linked Chrome tab"), "surface tab web chrome", needs_browser_tab)),
    app!("allele.toggle_transcript_tab", ToggleTranscriptTabAction, "Toggle Transcript", ["cmd-shift-r"],
        "Switch the active session between its terminal and transcript tabs.",
        palette(None, "surface tab log history rich", always)),
    // ── Panels & overlays ─────────────────────────────────────────
    app!("allele.toggle_sidebar", ToggleSidebarAction, "Show/Hide Sidebar", ["cmd-b"],
        "Show or hide the left sidebar.",
        palette(None, "toggle panel projects sessions hide show", always)),
    app!("allele.toggle_active_only", ToggleActiveOnlyAction, "Active Sessions Only", ["cmd-shift-e"],
        "Show only sessions with a live agent in the sidebar, or show all.",
        palette(None, "toggle filter sidebar live running", always)),
    app!("allele.toggle_changes_panel", ToggleChangesPanel, "Show/Hide Changes Panel", [],
        "Show or hide the right-hand changes panel (the session's git diff).",
        palette(None, "toggle panel git diff review", always)),
    app!("allele.toggle_drawer", ToggleDrawerAction, "Show/Hide Terminal", ["cmd-j"],
        "Show or hide the bottom drawer terminals.",
        palette(None, "toggle drawer terminal shell bottom", always)),
    app!("allele.open_file_palette", OpenFilePaletteAction, "Go to File…", ["cmd-p"],
        "Open the file palette for the active session's workspace.",
        palette(Some("Fuzzy file search"), "open find path", always)),
    app!("allele.open_search", OpenSearchAction, "Search Project…", ["cmd-shift-f"],
        "Open search across the active session's workspace.",
        palette(Some("Content & symbols"), "grep find text symbol", always)),
    app!("allele.open_scratch_pad", OpenScratchPadAction, "Open Scratch Pad", ["cmd-k"],
        "Open the scratch pad for drafting a prompt.",
        palette(None, "notes compose", always)),
    app!("allele.open_command_palette", OpenCommandPaletteAction, "Command Palette", ["cmd-shift-p"],
        "Open the command palette."),
    app!("allele.open_settings", OpenSettings, "Settings…", ["cmd-,"],
        "Open the Settings window.",
        palette(None, "preferences config options", always)),
    // ── Sessions ──────────────────────────────────────────────────
    app!("allele.new_session", NewSessionAction, "New Session…", [],
        "Open the new-session dialog for the active session's project.",
        palette(Some("In the current project"), "create add clone", needs_project)),
    app!("allele.toggle_decision_history", ToggleDecisionHistory, "Show/Hide Decision History", [],
        "Show or hide answered, withdrawn and expired questions in the decision strip, even when none are open.",
        palette(None, "decisions questions answers history asked", always)),
    app!("allele.cycle_attention", CycleAttentionSession, "Next Session Needing Attention", ["cmd-shift-a"],
        "Jump to the next session that needs your attention.",
        palette(None, "attention waiting awaiting input next jump", always)),
    app!("allele.discard_session", DiscardSessionAction, "Discard Session…", [],
        "Ask to discard the active session. Nothing is removed until you confirm in the sidebar.",
        Palette {
            subtitle: Some("Asks to confirm in the sidebar first"),
            keywords: "delete remove reset revert destructive",
            destructive: true,
            unavailable: needs_session,
        }),
    app!("allele.away", crate::away::AwayAction, "Away — Remote Control for All Sessions", [],
        "Send /remote-control to every live Claude session, so you can follow them from your phone.",
        palette(Some("Sends /remote-control to every live Claude session"),
            "away remote control phone mobile lunch rc leave", always)),
    // ── App ───────────────────────────────────────────────────────
    app!("allele.about", About, "About Allele", [],
        "Show the About panel.",
        palette(None, "version info", always)),
    app!("allele.capture_ui", CaptureUi, "Capture UI for Agent", [],
        "Write a screenshot and UI tree of the window for an agent to read.",
        palette(None, "debug screenshot capture", always)),
    app!("allele.quit", Quit, "Quit Allele", ["cmd-q"],
        "Quit Allele (asks first when sessions are running).",
        palette(None, "exit close", always)),
    // ── Compose bar (Rich Mode) ───────────────────────────────────
    edit!("compose.cursor_left", cb::CursorLeft, cb::KEY_CONTEXT, ["left"], "Move the cursor one character left."),
    edit!("compose.cursor_right", cb::CursorRight, cb::KEY_CONTEXT, ["right"], "Move the cursor one character right."),
    edit!("compose.cursor_up", cb::CursorUp, cb::KEY_CONTEXT, ["up"], "Move the cursor up one line."),
    edit!("compose.cursor_down", cb::CursorDown, cb::KEY_CONTEXT, ["down"], "Move the cursor down one line."),
    edit!("compose.cursor_word_left", cb::CursorWordLeft, cb::KEY_CONTEXT, ["alt-left"], "Move the cursor to the start of the previous word."),
    edit!("compose.cursor_word_right", cb::CursorWordRight, cb::KEY_CONTEXT, ["alt-right"], "Move the cursor to the end of the next word."),
    edit!("compose.cursor_line_start", cb::CursorLineStart, cb::KEY_CONTEXT, ["cmd-left", "home"], "Move the cursor to the start of the line."),
    edit!("compose.cursor_line_end", cb::CursorLineEnd, cb::KEY_CONTEXT, ["cmd-right", "end"], "Move the cursor to the end of the line."),
    edit!("compose.select_left", cb::SelectLeft, cb::KEY_CONTEXT, ["shift-left"], "Extend the selection one character left."),
    edit!("compose.select_right", cb::SelectRight, cb::KEY_CONTEXT, ["shift-right"], "Extend the selection one character right."),
    edit!("compose.select_up", cb::SelectUp, cb::KEY_CONTEXT, ["shift-up"], "Extend the selection up one line."),
    edit!("compose.select_down", cb::SelectDown, cb::KEY_CONTEXT, ["shift-down"], "Extend the selection down one line."),
    edit!("compose.select_word_left", cb::SelectWordLeft, cb::KEY_CONTEXT, ["alt-shift-left"], "Extend the selection to the start of the previous word."),
    edit!("compose.select_word_right", cb::SelectWordRight, cb::KEY_CONTEXT, ["alt-shift-right"], "Extend the selection to the end of the next word."),
    edit!("compose.select_line_start", cb::SelectLineStart, cb::KEY_CONTEXT, ["cmd-shift-left"], "Extend the selection to the start of the line."),
    edit!("compose.select_line_end", cb::SelectLineEnd, cb::KEY_CONTEXT, ["cmd-shift-right"], "Extend the selection to the end of the line."),
    edit!("compose.select_all", cb::SelectAll, cb::KEY_CONTEXT, ["cmd-a"], "Select all text in the compose bar."),
    edit!("compose.delete_left", cb::DeleteLeft, cb::KEY_CONTEXT, ["backspace"], "Delete the character before the cursor (or the selection)."),
    edit!("compose.delete_right", cb::DeleteRight, cb::KEY_CONTEXT, ["delete"], "Delete the character after the cursor (or the selection)."),
    edit!("compose.delete_word_left", cb::DeleteWordLeft, cb::KEY_CONTEXT, ["alt-backspace"], "Delete back to the start of the previous word."),
    edit!("compose.delete_word_right", cb::DeleteWordRight, cb::KEY_CONTEXT, ["alt-delete"], "Delete forward to the end of the next word."),
    edit!("compose.delete_line_left", cb::DeleteLineLeft, cb::KEY_CONTEXT, ["cmd-backspace"], "Delete back to the start of the line."),
    edit!("compose.copy", cb::Copy, cb::KEY_CONTEXT, ["cmd-c"], "Copy the selection to the clipboard."),
    edit!("compose.cut", cb::Cut, cb::KEY_CONTEXT, ["cmd-x"], "Cut the selection to the clipboard."),
    edit!("compose.paste", cb::Paste, cb::KEY_CONTEXT, ["cmd-v"], "Paste from the clipboard (images become attachments)."),
    edit!("compose.insert_newline", cb::InsertNewline, cb::KEY_CONTEXT, ["enter"], "Insert a newline without sending."),
    edit!("compose.submit", cb::Submit, cb::KEY_CONTEXT, ["cmd-enter"], "Send the composed message to the agent."),
    edit!("compose.show_character_palette", cb::ShowCharacterPalette, cb::KEY_CONTEXT, ["ctrl-cmd-space"], "Open the macOS emoji & symbols palette."),
    edit!("compose.attach_files", cb::AttachFiles, cb::KEY_CONTEXT, [], "Open a file picker and attach the chosen files."),
    edit!("text_input.backspace", ti::Backspace, ti::KEY_CONTEXT, ["backspace"], "Delete the character before the cursor (or the selection)."),
    edit!("text_input.delete", ti::Delete, ti::KEY_CONTEXT, ["delete"], "Delete the character after the cursor (or the selection)."),
    edit!("text_input.delete_word_left", ti::DeleteWordLeft, ti::KEY_CONTEXT, ["alt-backspace"], "Delete back to the start of the previous word."),
    edit!("text_input.delete_word_right", ti::DeleteWordRight, ti::KEY_CONTEXT, ["alt-delete"], "Delete forward to the end of the next word."),
    edit!("text_input.delete_to_home", ti::DeleteToHome, ti::KEY_CONTEXT, ["cmd-backspace"], "Delete back to the start of the field."),
    edit!("text_input.left", ti::Left, ti::KEY_CONTEXT, ["left"], "Move the cursor one character left."),
    edit!("text_input.right", ti::Right, ti::KEY_CONTEXT, ["right"], "Move the cursor one character right."),
    edit!("text_input.word_left", ti::WordLeft, ti::KEY_CONTEXT, ["alt-left"], "Move the cursor to the start of the previous word."),
    edit!("text_input.word_right", ti::WordRight, ti::KEY_CONTEXT, ["alt-right"], "Move the cursor to the end of the next word."),
    edit!("text_input.select_left", ti::SelectLeft, ti::KEY_CONTEXT, ["shift-left"], "Extend the selection one character left."),
    edit!("text_input.select_right", ti::SelectRight, ti::KEY_CONTEXT, ["shift-right"], "Extend the selection one character right."),
    edit!("text_input.select_word_left", ti::SelectWordLeft, ti::KEY_CONTEXT, ["alt-shift-left"], "Extend the selection to the start of the previous word."),
    edit!("text_input.select_word_right", ti::SelectWordRight, ti::KEY_CONTEXT, ["alt-shift-right"], "Extend the selection to the end of the next word."),
    edit!("text_input.select_to_home", ti::SelectToHome, ti::KEY_CONTEXT, ["cmd-shift-left"], "Extend the selection to the start of the field."),
    edit!("text_input.select_to_end", ti::SelectToEnd, ti::KEY_CONTEXT, ["cmd-shift-right"], "Extend the selection to the end of the field."),
    edit!("text_input.select_all", ti::SelectAll, ti::KEY_CONTEXT, ["cmd-a"], "Select all text in the field."),
    edit!("text_input.paste", ti::Paste, ti::KEY_CONTEXT, ["cmd-v"], "Paste from the clipboard."),
    edit!("text_input.copy", ti::Copy, ti::KEY_CONTEXT, ["cmd-c"], "Copy the selection to the clipboard."),
    edit!("text_input.cut", ti::Cut, ti::KEY_CONTEXT, ["cmd-x"], "Cut the selection to the clipboard."),
    edit!("text_input.home", ti::Home, ti::KEY_CONTEXT, ["cmd-left", "home"], "Move the cursor to the start of the field."),
    edit!("text_input.end", ti::End, ti::KEY_CONTEXT, ["cmd-right", "end"], "Move the cursor to the end of the field."),
    edit!("text_input.show_character_palette", ti::ShowCharacterPalette, ti::KEY_CONTEXT, ["ctrl-cmd-space"], "Open the macOS emoji & symbols palette."),
    edit!("text_input.submit", ti::Submit, ti::KEY_CONTEXT, ["enter"], "Confirm the field (the same as pressing its form's primary button)."),
];

/// The row for `name`, exactly as written. Deprecated aliases are resolved by
/// the keymap loader, not here.
pub(crate) fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// The rows the command palette lists, in table order.
pub(crate) fn palette_commands() -> impl Iterator<Item = (&'static Command, &'static Palette)> {
    COMMANDS
        .iter()
        .filter_map(|c| c.palette.as_ref().map(|p| (c, p)))
}

#[cfg(test)]
mod tests {
    use super::{find, palette_commands, COMMANDS};
    use std::collections::BTreeSet;

    #[test]
    fn names_are_unique() {
        let mut seen = BTreeSet::new();
        for c in COMMANDS {
            assert!(seen.insert(c.name), "duplicate command name `{}`", c.name);
        }
    }

    #[test]
    fn names_are_namespaced_snake_case() {
        for c in COMMANDS {
            let (ns, rest) = c.name.split_once('.').expect("name has a namespace");
            assert!(
                ["allele", "compose", "text_input"].contains(&ns),
                "`{}`: unknown namespace",
                c.name
            );
            assert!(
                rest.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'),
                "`{}`: use snake_case",
                c.name
            );
        }
    }

    #[test]
    fn every_command_is_described() {
        for c in COMMANDS {
            assert!(
                !c.description.trim().is_empty(),
                "`{}` has no description",
                c.name
            );
            assert!(!c.title.trim().is_empty(), "`{}` has no title", c.name);
        }
    }

    /// Two commands claiming one key in one context would make which one
    /// fires depend on registration order.
    #[test]
    fn no_two_commands_share_a_default_key() {
        let mut seen = BTreeSet::new();
        for c in COMMANDS {
            for key in c.default_keys {
                assert!(
                    seen.insert((c.context, *key)),
                    "`{key}` in {:?} is bound twice (second: `{}`)",
                    c.context,
                    c.name
                );
            }
        }
    }

    /// Only app-wide commands make sense in the palette: an editing command
    /// needs its input focused, and the palette has taken focus.
    #[test]
    fn palette_lists_only_app_wide_commands() {
        for (c, _) in palette_commands() {
            assert!(c.context.is_none(), "`{}` is context-bound", c.name);
        }
    }

    /// Everything the hand-written palette list had before DEV-1264 is still
    /// in it.
    #[test]
    fn palette_keeps_every_command_it_had() {
        let palette: BTreeSet<&str> = palette_commands().map(|(c, _)| c.name).collect();
        for name in [
            "allele.go_to_claude",
            "allele.go_to_reader",
            "allele.go_to_transcript",
            "allele.go_to_browser",
            "allele.toggle_sidebar",
            "allele.toggle_changes_panel",
            "allele.open_file_palette",
            "allele.open_search",
            "allele.open_scratch_pad",
            "allele.new_session",
            "allele.discard_session",
            "allele.away",
        ] {
            assert!(palette.contains(name), "palette lost `{name}`");
        }
    }

    #[test]
    fn every_command_builds_its_binding_and_action() {
        for c in COMMANDS {
            let _ = c.binding("cmd-f12", c.context);
            let action = c.action();
            assert!(
                !action.name().is_empty(),
                "`{}` has a nameless action",
                c.name
            );
        }
        assert!(find("allele.toggle_sidebar").is_some());
        assert!(find("allele.no_such_command").is_none());
    }
}
