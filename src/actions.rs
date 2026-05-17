//! Command-pattern types for deferred `AppState` mutations.
//!
//! GPUI listeners borrow the render tree while they run, so they can't mutate
//! `AppState` directly — see `ARCHITECTURE.md` §3.1 and the "what NOT to do"
//! note in §7.5. Instead, listeners enqueue a [`PendingAction`] on
//! `AppState.pending_action`; the next render tick drains it before building
//! the tree.
//!
//! `PendingAction` is a thin wrapper over per-family sub-enums so each family
//! (session / archive / drawer / sidebar / project / settings / browser /
//! overlay) can be dispatched and evolved independently. Call sites
//! construct a family variant and convert with `.into()`.

use std::path::PathBuf;

use crate::naming;
use crate::settings;

#[derive(Debug)]
pub enum PendingAction {
    Session(SessionAction),
    Archive(ArchiveAction),
    Drawer(DrawerAction),
    Sidebar(SidebarAction),
    Project(ProjectAction),
    Settings(SettingsAction),
    Browser(BrowserAction),
    Overlay(OverlayAction),
}

/// Session lifecycle — create, close, select, resume, edit, pin, reveal.
#[derive(Debug)]
pub enum SessionAction {
    NewSessionInActiveProject,
    CloseActiveSession,
    FocusActive,
    AddSessionToProject(usize), // project index
    /// Open the "New session with details" modal for a project.
    OpenNewSessionModal(usize),
    /// Create a session with custom details from the modal.
    AddSessionWithDetails {
        project_idx: usize,
        label: String,
        branch_slug: Option<String>,
        agent_id: Option<String>,
        initial_prompt: Option<String>,
    },
    /// Kill the PTY, keep the clone, mark Suspended. Next click cold-resumes.
    CloseSessionKeepClone { project_idx: usize, session_idx: usize },
    /// Ask for confirmation before discarding — sets `confirming_discard`.
    RequestDiscardSession { project_idx: usize, session_idx: usize },
    /// Cancel an in-flight discard confirmation.
    CancelDiscard,
    /// Permanently delete the clone and remove the session from state.
    DiscardSession { project_idx: usize, session_idx: usize },
    SelectSession { project_idx: usize, session_idx: usize },
    /// Merge session work into canonical and close (archive + merge + delete clone).
    MergeAndClose { project_idx: usize, session_idx: usize },
    /// Proceed with session creation despite dirty canonical.
    ProceedDirtySession(usize),
    /// Cancel dirty-state session creation.
    CancelDirtySession,
    /// Auto-resume a session after launch. Fires once from the first render
    /// tick so `resume_session` has a valid `window` / `cx`.
    ResumeSession { project_idx: usize, session_idx: usize },
    /// Open the edit-session modal for a given session.
    EditSession { project_idx: usize, session_idx: usize },
    /// Reveal the session's clone path (or source path) in Finder.
    RevealSessionInFinder { project_idx: usize, session_idx: usize },
    /// Copy the session's clone path to the clipboard.
    CopySessionPath { project_idx: usize, session_idx: usize },
    /// Toggle the pinned state of a session.
    TogglePinSession { project_idx: usize, session_idx: usize },
    /// Apply edits from the edit-session modal.
    ApplySessionEdit {
        project_idx: usize,
        session_idx: usize,
        label: String,
        branch_slug: Option<String>,
        comment: Option<String>,
        pinned: bool,
    },
}

/// Archive refs — merge / delete a session that has been archived.
#[derive(Debug)]
pub enum ArchiveAction {
    /// Merge an archived session ref into canonical's working tree.
    MergeArchive { project_idx: usize, archive_idx: usize },
    /// Delete an archive ref without merging.
    DeleteArchive { project_idx: usize, archive_idx: usize },
}

/// Bottom drawer terminal tabs — toggle, spawn, rename, close.
#[derive(Debug)]
pub enum DrawerAction {
    /// Toggle the bottom drawer terminal panel.
    ToggleDrawer,
    /// Create a new drawer terminal tab in the active session.
    NewDrawerTab,
    /// Switch the active drawer tab.
    SwitchDrawerTab(usize),
    /// Close a drawer tab by index. Closing the last tab hides the drawer.
    CloseDrawerTab(usize),
    /// Enter rename mode for a drawer tab.
    StartRenameDrawerTab(usize),
    /// Commit the current rename buffer as the tab's new name.
    CommitRenameDrawerTab,
    /// Cancel rename mode without saving.
    CancelRenameDrawerTab,
}

/// Sidebar visibility — left / right pane toggles.
#[derive(Debug)]
pub enum SidebarAction {
    /// Toggle the left sidebar visibility.
    ToggleSidebar,
    /// Toggle the right sidebar visibility.
    ToggleRightSidebar,
}

/// Project lifecycle — add, remove, relocate.
#[derive(Debug)]
pub enum ProjectAction {
    OpenProjectAtPath(PathBuf),
    RemoveProject(usize),
    /// Source path missing — open folder picker so the user can relocate.
    RelocateProject(usize),
}

/// User settings / preferences — emitted by the Settings window.
#[derive(Debug)]
#[allow(clippy::enum_variant_names)]
pub enum SettingsAction {
    /// Replace the session-cleanup-paths list with a new value and persist.
    /// Emitted by the Settings window on every edit.
    UpdateCleanupPaths(Vec<String>),
    /// Replace the external-editor command with a new value and persist.
    /// Emitted by the Settings window on every edit.
    UpdateExternalEditor(String),
    /// Toggle Chrome browser integration on/off. When toggled off we clear
    /// the current sync status so the Browser tab shows the disabled state.
    UpdateBrowserIntegration(bool),
    /// Replace the entire coding-agents list and the default-agent id.
    /// Emitted by the Settings window on every edit (add/remove agent,
    /// toggle enabled, edit path / extra args, pick default, re-detect).
    UpdateAgents {
        agents: Vec<settings::AgentConfig>,
        default_agent: Option<String>,
    },
    /// Toggle "git pull on source root before creating a new session".
    /// Emitted by the Settings window; persisted immediately.
    UpdateGitPullBeforeNewSession(bool),
    /// Toggle "promote attention-needed sessions to top of list".
    /// Emitted by the Settings window; persisted immediately.
    UpdatePromoteAttentionSessions(bool),
    /// Replace the global terminal font size and persist. Emitted by the
    /// Settings window, by Cmd+=/Cmd+- (as a clamped new value), and by
    /// Cmd+0 (reset to DEFAULT_FONT_SIZE). The handler clamps again,
    /// writes `user_settings.font_size`, saves to disk, and broadcasts
    /// the new value to every open `TerminalView`.
    UpdateFontSize(f32),
    /// Update the naming configuration (mode, models). Emitted by the
    /// Settings window naming section.
    UpdateNamingConfig(naming::NamingConfig),
}

/// Chrome browser integration — sync / close linked tabs.
#[derive(Debug)]
pub enum BrowserAction {
    /// Activate the Chrome tab linked to the currently-active session,
    /// creating one if the session has no tab id yet or the stored id is
    /// stale. Fired on session switch, session resume, and Browser-tab
    /// click.
    SyncBrowserToActiveSession,
    /// Close the Chrome tab linked to the given session and clear its
    /// stored tab id. User-initiated via the Browser tab's Close button.
    CloseBrowserTabForSession { project_idx: usize, session_idx: usize },
}

/// Non-sidebar overlays — scratch pad, future modals.
#[derive(Debug)]
pub enum OverlayAction {
    /// Open (or re-focus) the scratch pad compose overlay.
    OpenScratchPad,
}

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
pub struct SessionCursor {
    pub project_idx: usize,
    pub session_idx: usize,
}
