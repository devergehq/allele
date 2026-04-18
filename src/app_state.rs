use gpui::*;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::actions::{PendingAction, SessionCursor};
use crate::platform::Platform;
use crate::project::Project;
use crate::scratch_pad;
use crate::session::Session;
use crate::settings::Settings;
use crate::settings_window;
use crate::state;

/// Which view is shown in the main (center) column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MainTab {
    Claude,
    Editor,
    Browser,
}

// -----------------------------------------------------------------------
// Cohesive sub-struct state bundles
// -----------------------------------------------------------------------

/// Left sidebar visibility + resize state.
pub(crate) struct SidebarState {
    pub(crate) visible: bool,
    pub(crate) width: f32,
    pub(crate) resizing: bool,
}

/// Right-hand inspector panel. Same shape as `SidebarState` but kept as
/// a separate type so the two can diverge (e.g. different min widths,
/// content renderers) without swapping field names.
pub(crate) struct RightPanelState {
    pub(crate) visible: bool,
    pub(crate) width: f32,
    pub(crate) resizing: bool,
}

/// Bottom drawer terminal panel. Visibility is per-session on
/// `Session::drawer_visible`; this struct covers global chrome
/// (height, resize handle, tab-rename input).
pub(crate) struct DrawerState {
    pub(crate) height: f32,
    pub(crate) resizing: bool,
    /// Active inline tab rename: (session cursor, tab index, current buffer).
    /// When Some, the tab strip renders that tab as an editable label.
    pub(crate) rename: Option<(SessionCursor, usize, String)>,
    /// Focus handle for the inline rename input. Created lazily when rename
    /// mode first activates in a given AppState instance.
    pub(crate) rename_focus: Option<FocusHandle>,
}

/// Editor tab state — file tree selection, preview cache, right-click menu.
pub(crate) struct EditorState {
    /// File path currently selected in the Editor tab's file tree.
    pub(crate) selected_path: Option<PathBuf>,
    /// Directories expanded in the Editor tab's file tree.
    pub(crate) expanded_dirs: HashSet<PathBuf>,
    /// Cached (path, contents) of the currently previewed file.
    pub(crate) preview: Option<(PathBuf, String)>,
    /// Right-click context menu target for the Editor file tree.
    /// Stores (right-clicked path, window-space position of the click).
    pub(crate) context_menu: Option<(PathBuf, Point<Pixels>)>,
}

/// Inline confirmation gates. Each is `Some` / `true` while a user is
/// being prompted before a destructive action proceeds.
pub(crate) struct ConfirmationState {
    /// When `Some(cursor)` the sidebar row at that cursor shows a
    /// confirm/cancel prompt instead of the usual buttons.
    pub(crate) discard: Option<SessionCursor>,
    /// Project index awaiting dirty-state confirmation before session create.
    pub(crate) dirty_session: Option<usize>,
    /// When true, a quit confirmation banner is shown because running
    /// sessions exist.
    pub(crate) quit: bool,
}

// -----------------------------------------------------------------------
// Top-level AppState
// -----------------------------------------------------------------------

pub(crate) struct AppState {
    pub(crate) projects: Vec<Project>,
    pub(crate) active: Option<SessionCursor>,
    pub(crate) pending_action: Option<PendingAction>,
    pub(crate) sidebar: SidebarState,
    pub(crate) right_sidebar: RightPanelState,
    pub(crate) drawer: DrawerState,
    pub(crate) editor: EditorState,
    pub(crate) confirmations: ConfirmationState,
    /// Absolute path to the Allele hooks.json, passed to claude via
    /// `--settings <path>` at every spawn. `None` if install_if_missing
    /// failed — in that case hooks are silently disabled and the app still
    /// functions normally.
    pub(crate) hooks_settings_path: Option<PathBuf>,
    /// Current user settings (sound/notification preferences).
    pub(crate) user_settings: Settings,
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
    /// Status text for the Browser tab panel (e.g. "Chrome is not
    /// running", "Linked to tab #…"). Updated by SyncBrowserToActiveSession
    /// and rendered by render_browser_placeholder.
    pub(crate) browser_status: String,
    /// Scratch pad compose overlay. `Some` while the overlay is visible.
    pub(crate) scratch_pad: Option<Entity<scratch_pad::ScratchPad>>,
    /// Persistent Scratch Pad submission history across all projects.
    /// Loaded from state.json on startup, appended on submit, written back
    /// on every save_state. Filtered by project when the overlay opens.
    pub(crate) scratch_pad_history: Vec<state::ScratchPadEntry>,
    /// Platform-abstraction bundle (COW clones, browser control, shell).
    /// Selected once at startup via `Platform::detect()`; passed into
    /// background tasks via `Clone` of the inner `Arc`s.
    pub(crate) platform: Platform,
}

pub(crate) const SIDEBAR_MIN_WIDTH: f32 = 160.0;
pub(crate) const DRAWER_MIN_HEIGHT: f32 = 100.0;
pub(crate) const RIGHT_SIDEBAR_MIN_WIDTH: f32 = 160.0;

impl AppState {
    /// Get the currently active session, if any.
    pub(crate) fn active_session(&self) -> Option<&Session> {
        let cursor = self.active?;
        self.projects
            .get(cursor.project_idx)?
            .sessions
            .get(cursor.session_idx)
    }
}
