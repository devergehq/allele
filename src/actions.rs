//! Command-pattern action enums.
//!
//! Every UI event that mutates `AppState` goes through a [`PendingAction`].
//! This is a deliberate "queue, then apply" split: listeners can't mutate
//! state while the render tree is borrowed, so they enqueue, and the next
//! render tick drains the queue. See `dispatch_pending_action` in
//! `pending_actions.rs`.
//!
//! Actions are grouped by domain into sub-enums ([`SessionAction`],
//! [`DrawerAction`], etc.), wrapped by the top-level [`PendingAction`]
//! enum. Each family has its own handler in `pending_actions.rs`, so the
//! 36-variant dispatcher decomposes into a 7-arm match that delegates.
//!
//! Construction uses `.into()` to keep call sites terse:
//! ```
//! this.pending_action = Some(SessionAction::CloseActive.into());
//! ```

use std::path::PathBuf;

use crate::settings;

// -----------------------------------------------------------------------
// Session lifecycle actions
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum SessionAction {
    /// Create a new session in the currently active project.
    NewInActive,
    /// Close the currently active session (keep clone, mark Suspended).
    CloseActive,
    /// Focus the currently active session's terminal.
    FocusActive,
    /// Create a new session inside the given project.
    AddToProject(usize),
    /// Kill the PTY, keep the clone, mark Suspended. Next click cold-resumes.
    CloseKeepClone { project_idx: usize, session_idx: usize },
    /// Ask for confirmation before discarding — sets `confirming_discard`.
    RequestDiscard { project_idx: usize, session_idx: usize },
    /// Cancel an in-flight discard confirmation.
    CancelDiscard,
    /// Permanently delete the clone and remove the session from state.
    Discard { project_idx: usize, session_idx: usize },
    /// Select a session (activate or cold-resume if Suspended).
    Select { project_idx: usize, session_idx: usize },
    /// Merge session work into canonical and close (archive + merge + delete clone).
    MergeAndClose { project_idx: usize, session_idx: usize },
    /// Auto-resume a session after launch. Fires once from the first render
    /// tick so `resume_session` has a valid `window` / `cx`.
    Resume { project_idx: usize, session_idx: usize },
    /// Proceed with session creation despite dirty canonical.
    ProceedDirty(usize),
    /// Cancel dirty-state session creation.
    CancelDirty,
}

// -----------------------------------------------------------------------
// Archive actions (merge / delete archived session refs)
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum ArchiveAction {
    /// Merge an archived session ref into canonical's working tree.
    Merge { project_idx: usize, archive_idx: usize },
    /// Delete an archive ref without merging.
    Delete { project_idx: usize, archive_idx: usize },
}

// -----------------------------------------------------------------------
// Drawer actions (bottom terminal panel)
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum DrawerAction {
    /// Toggle the bottom drawer terminal panel.
    Toggle,
    /// Create a new drawer terminal tab in the active session.
    NewTab,
    /// Switch the active drawer tab.
    SwitchTab(usize),
    /// Close a drawer tab by index. Closing the last tab hides the drawer.
    CloseTab(usize),
    /// Enter rename mode for a drawer tab.
    StartRenameTab(usize),
    /// Commit the current rename buffer as the tab's new name.
    CommitRenameTab,
    /// Cancel rename mode without saving.
    CancelRenameTab,
}

// -----------------------------------------------------------------------
// Sidebar actions (left + right panel visibility)
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum SidebarAction {
    /// Toggle the left sidebar visibility.
    ToggleLeft,
    /// Toggle the right sidebar visibility.
    ToggleRight,
}

// -----------------------------------------------------------------------
// Project actions
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum ProjectAction {
    /// Open a project folder at the given path and auto-create its first session.
    OpenAtPath(PathBuf),
    /// Remove a project and all its sessions (background-cleans clones).
    Remove(usize),
    /// Source path missing — open folder picker so the user can relocate.
    Relocate(usize),
}

// -----------------------------------------------------------------------
// Settings actions (persisted user preferences)
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum SettingsAction {
    /// Replace the session-cleanup-paths list with a new value and persist.
    UpdateCleanupPaths(Vec<String>),
    /// Replace the external-editor command with a new value and persist.
    UpdateExternalEditor(String),
    /// Toggle Chrome browser integration on/off.
    UpdateBrowserIntegration(bool),
    /// Replace the entire coding-agents list and the default-agent id.
    UpdateAgents {
        agents: Vec<settings::AgentConfig>,
        default_agent: Option<String>,
    },
    /// Toggle "git pull on source root before creating a new session".
    UpdateGitPullBeforeNewSession(bool),
    /// Replace the global terminal font size and persist. Broadcast to
    /// every open TerminalView so the change applies live.
    UpdateFontSize(f32),
}

// -----------------------------------------------------------------------
// Browser actions (Chrome tab integration)
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum BrowserAction {
    /// Activate the Chrome tab linked to the currently-active session.
    SyncToActive,
    /// Close the Chrome tab linked to the given session.
    CloseForSession { project_idx: usize, session_idx: usize },
}

// -----------------------------------------------------------------------
// Overlay actions (scratch pad, future modals)
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum OverlayAction {
    /// Open (or re-focus) the scratch pad compose overlay.
    OpenScratchPad,
}

// -----------------------------------------------------------------------
// Top-level action wrapper
// -----------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum PendingAction {
    Session(SessionAction),
    Archive(ArchiveAction),
    Drawer(DrawerAction),
    Sidebar(SidebarAction),
    Project(ProjectAction),
    Settings(SettingsAction),
    Browser(BrowserAction),
    Overlay(OverlayAction),
}

// From impls — let call sites write `SessionAction::CloseActive.into()`
// rather than `PendingAction::Session(SessionAction::CloseActive)`.
impl From<SessionAction> for PendingAction {
    fn from(a: SessionAction) -> Self { PendingAction::Session(a) }
}
impl From<ArchiveAction> for PendingAction {
    fn from(a: ArchiveAction) -> Self { PendingAction::Archive(a) }
}
impl From<DrawerAction> for PendingAction {
    fn from(a: DrawerAction) -> Self { PendingAction::Drawer(a) }
}
impl From<SidebarAction> for PendingAction {
    fn from(a: SidebarAction) -> Self { PendingAction::Sidebar(a) }
}
impl From<ProjectAction> for PendingAction {
    fn from(a: ProjectAction) -> Self { PendingAction::Project(a) }
}
impl From<SettingsAction> for PendingAction {
    fn from(a: SettingsAction) -> Self { PendingAction::Settings(a) }
}
impl From<BrowserAction> for PendingAction {
    fn from(a: BrowserAction) -> Self { PendingAction::Browser(a) }
}
impl From<OverlayAction> for PendingAction {
    fn from(a: OverlayAction) -> Self { PendingAction::Overlay(a) }
}

/// Position of a session in the project tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionCursor {
    pub(crate) project_idx: usize,
    pub(crate) session_idx: usize,
}
