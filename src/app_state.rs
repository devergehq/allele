//! The `AppState` god-struct, its sub-enum `MainTab`, and related constants.
//!
//! Inherent methods + `Render` impl live in `src/main.rs` (for now); this
//! module is just the data layout. See `ARCHITECTURE.md` §3.5 for the
//! sub-struct composition target implemented in phase 11 of
//! `docs/RE-DECOMPOSITION-PLAN.md`.

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

/// Left sidebar geometry + live drag state.
pub(crate) struct SidebarState {
    pub(crate) visible: bool,
    pub(crate) width: f32,
    pub(crate) resizing: bool,
}

/// Right panel geometry + live drag state. Same shape as `SidebarState`
/// but a separate type so the two can diverge.
pub(crate) struct RightPanelState {
    pub(crate) visible: bool,
    pub(crate) width: f32,
    pub(crate) resizing: bool,
}

/// Drawer terminal geometry + inline-rename state.
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

/// Editor tab file-tree + preview state.
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

/// Cluster of confirmation-gate flags.
pub(crate) struct ConfirmationState {
    /// Inline confirmation gate for the Discard action. When `Some(cursor)`
    /// the sidebar row at that cursor shows a confirm/cancel prompt instead
    /// of the usual buttons.
    pub(crate) discard: Option<SessionCursor>,
    /// Project index awaiting dirty-state confirmation before session create.
    pub(crate) dirty_session: Option<usize>,
    /// When true, a quit confirmation banner is shown because running sessions exist.
    pub(crate) quit: bool,
}

/// Rich Sidecar (transcript view) state. Lazily created the first time the
/// Transcript tab is rendered. Rebuilt when the active session changes (the
/// tailer is scoped to one session's JSONL).
pub(crate) struct RichState {
    pub(crate) view: Option<Entity<rich::RichView>>,
    /// Tails `~/.claude/projects/<dashed-cwd>/<session>.jsonl` for the
    /// active session. `None` until the first Transcript-tab render.
    pub(crate) transcript_tailer: Option<transcript::TranscriptTailer>,
    /// The allele session cursor the current `view`/`transcript_tailer`
    /// was built for. Used to detect when the active session has changed
    /// and the sidecar needs to be rebuilt from a fresh JSONL.
    pub(crate) cursor: Option<SessionCursor>,
}

pub(crate) struct AppState {
    pub(crate) projects: Vec<Project>,
    pub(crate) active: Option<SessionCursor>,
    pub(crate) pending_action: Option<PendingAction>,
    pub(crate) sidebar: SidebarState,
    pub(crate) right_panel: RightPanelState,
    pub(crate) drawer: DrawerState,
    pub(crate) editor: EditorState,
    pub(crate) confirming: ConfirmationState,
    pub(crate) rich: RichState,
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
    /// Set by `mark_state_dirty()`; drained by `checkpoint_persistence()`
    /// at the end of each render tick. Coalesces N mutations-per-frame
    /// into at most one state.json write. See ARCHITECTURE.md §3.4.
    pub(crate) state_dirty: bool,
    /// Same contract as `state_dirty` but for the settings.json file.
    pub(crate) settings_dirty: bool,
    /// Persistence backends for settings.json and state.json. Arc-cloned
    /// into background tasks so they can write without borrowing AppState.
    /// See `src/repositories.rs` and ARCHITECTURE.md §3.3.
    pub(crate) repos: crate::repositories::Repositories,
    /// OS-abstraction layer. Arc-cloned per subsystem; detected once at
    /// startup. Background tasks get cheap Arc clones. See
    /// ARCHITECTURE.md §3.2 + §4.1.
    pub(crate) platform: crate::platform::Platform,
}

impl AppState {
    /// Flag that persisted state (`state.json`) has been mutated and
    /// should be written on the next `checkpoint_persistence()` call.
    /// Do NOT call `save_state()` directly — see ARCHITECTURE.md §7.2.
    pub(crate) fn mark_state_dirty(&mut self) {
        self.state_dirty = true;
    }

    /// Flag that user settings (`settings.json`) have been mutated
    /// and should be written on the next `checkpoint_persistence()`.
    /// Do NOT call `save_settings()` directly — see ARCHITECTURE.md §7.2.
    pub(crate) fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
    }

    /// Drain the dirty flags and flush pending writes. Called once at
    /// the end of every `Render::render` tick so N mutations per frame
    /// coalesce to at most one write per file.
    pub(crate) fn checkpoint_persistence(&mut self) {
        if self.state_dirty {
            self.save_state();
            self.state_dirty = false;
        }
        if self.settings_dirty {
            self.save_settings();
            self.settings_dirty = false;
        }
    }
}
