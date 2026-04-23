//! The `AppState` god-struct, its sub-enum `MainTab`, and related constants.
//!
//! Inherent methods + `Render` impl live in `src/main.rs` (for now); this
//! module is just the data layout. See `ARCHITECTURE.md` §3.5 for the
//! eventual sub-struct composition target and `docs/RE-DECOMPOSITION-PLAN.md`
//! §5 phase 11 for the follow-on split into `SidebarState`, `DrawerState`,
//! `EditorState`, `RightPanelState`, `ConfirmationState`.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{Entity, FocusHandle, Pixels, Point, WindowHandle};

use crate::actions::{PendingAction, SessionCursor};
use crate::project::Project;
use crate::settings::Settings;
use crate::{
    new_session_modal, rich, scratch_pad, settings_window, state, text_input, transcript,
};

pub(crate) const SIDEBAR_MIN_WIDTH: f32 = 160.0;
pub(crate) const DRAWER_MIN_HEIGHT: f32 = 100.0;
pub(crate) const RIGHT_SIDEBAR_MIN_WIDTH: f32 = 160.0;

/// Which view is shown in the main (center) column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MainTab {
    Claude,
    Editor,
    Browser,
    /// Read-only structured view of the active session's Claude Code
    /// transcript — rendered by `rich::RichView`, fed by `transcript::
    /// TranscriptTailer`. Toggled with ⌘⇧R.
    Transcript,
}

pub(crate) struct AppState {
    pub(crate) projects: Vec<Project>,
    pub(crate) active: Option<SessionCursor>,
    pub(crate) pending_action: Option<PendingAction>,
    // Sidebar state
    pub(crate) sidebar_visible: bool,
    pub(crate) sidebar_width: f32,
    pub(crate) sidebar_resizing: bool,
    /// Inline confirmation gate for the Discard action. When `Some(cursor)`
    /// the sidebar row at that cursor shows a confirm/cancel prompt instead
    /// of the usual buttons.
    pub(crate) confirming_discard: Option<SessionCursor>,
    /// Project index awaiting dirty-state confirmation before session create.
    pub(crate) confirming_dirty_session: Option<usize>,
    /// Absolute path to the Allele hooks.json, passed to claude via
    /// `--settings <path>` at every spawn. `None` if install_if_missing
    /// failed — in that case hooks are silently disabled and the app still
    /// functions normally.
    pub(crate) hooks_settings_path: Option<PathBuf>,
    /// Current user settings (sound/notification preferences).
    pub(crate) user_settings: Settings,
    // Drawer terminal state (visibility is per-session on Session struct)
    pub(crate) drawer_height: f32,
    pub(crate) drawer_resizing: bool,
    /// Active inline tab rename: (session cursor, tab index, current buffer).
    /// When Some, the tab strip renders that tab as an editable label.
    pub(crate) drawer_rename: Option<(SessionCursor, usize, String)>,
    /// Focus handle for the inline rename input. Created lazily when rename
    /// mode first activates in a given AppState instance.
    pub(crate) drawer_rename_focus: Option<FocusHandle>,
    // Right sidebar state
    pub(crate) right_sidebar_visible: bool,
    pub(crate) right_sidebar_width: f32,
    pub(crate) right_sidebar_resizing: bool,
    /// When true, a quit confirmation banner is shown because running sessions exist.
    pub(crate) confirming_quit: bool,
    /// Project index whose settings panel is currently open in the sidebar.
    pub(crate) editing_project_settings: Option<usize>,
    /// Live handle to an open Settings window. Keeps ⌘, from spawning
    /// duplicates — when set, the action re-activates the existing window
    /// instead of opening a new one. Cleared when the window closes.
    pub(crate) settings_window: Option<WindowHandle<settings_window::SettingsWindowState>>,
    /// Transient warning shown when `git pull` on the source root fails
    /// before session creation. Auto-dismissed after a few seconds.
    pub(crate) pull_warning: Option<String>,
    /// Which view the center column is currently showing.
    pub(crate) main_tab: MainTab,
    /// Rich Sidecar state. Lazily created the first time the Transcript
    /// tab is rendered. Rebuilt when the active session changes (the
    /// tailer is scoped to one session's JSONL).
    pub(crate) rich_view: Option<Entity<rich::RichView>>,
    /// Tails `~/.claude/projects/<dashed-cwd>/<session>.jsonl` for the
    /// active session. `None` until the first Transcript-tab render.
    pub(crate) transcript_tailer: Option<transcript::TranscriptTailer>,
    /// The allele session cursor the current `rich_view`/`transcript_tailer`
    /// was built for. Used to detect when the active session has changed
    /// and the sidecar needs to be rebuilt from a fresh JSONL.
    pub(crate) rich_view_cursor: Option<SessionCursor>,
    /// File path currently selected in the Editor tab's file tree.
    pub(crate) editor_selected_path: Option<PathBuf>,
    /// Directories expanded in the Editor tab's file tree.
    pub(crate) editor_expanded_dirs: HashSet<PathBuf>,
    /// Cached (path, contents) of the currently previewed file.
    pub(crate) editor_preview: Option<(PathBuf, String)>,
    /// Right-click context menu target for the Editor file tree.
    /// Stores (right-clicked path, window-space position of the click).
    pub(crate) editor_context_menu: Option<(PathBuf, Point<Pixels>)>,
    /// Status text for the Browser tab panel (e.g. "Chrome is not
    /// running", "Linked to tab #…"). Updated by SyncBrowserToActiveSession
    /// and rendered by render_browser_placeholder.
    pub(crate) browser_status: String,
    /// Scratch pad compose overlay. `Some` while the overlay is visible.
    pub(crate) scratch_pad: Option<Entity<scratch_pad::ScratchPad>>,
    /// "New session with details" modal. `Some` while the overlay is visible.
    pub(crate) new_session_modal: Option<Entity<new_session_modal::NewSessionModal>>,
    /// Sidebar search/filter input entity.
    pub(crate) sidebar_filter_input: Entity<text_input::TextInput>,
    /// Current sidebar filter text (lowercased for matching).
    pub(crate) sidebar_filter: String,
    /// Right-click context menu on a session row: (cursor, click position).
    pub(crate) session_context_menu: Option<(SessionCursor, Point<Pixels>)>,
    /// "Edit session" modal for renaming/commenting an existing session.
    pub(crate) edit_session_modal: Option<Entity<new_session_modal::EditSessionModal>>,
    /// Persistent Scratch Pad submission history across all projects.
    /// Loaded from state.json on startup, appended on submit, written back
    /// on every save_state. Filtered by project when the overlay opens.
    pub(crate) scratch_pad_history: Vec<state::ScratchPadEntry>,
}
