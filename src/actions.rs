use crate::settings;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum PendingAction {
    NewSessionInActiveProject,
    CloseActiveSession,
    FocusActive,
    OpenProjectAtPath(PathBuf),
    AddSessionToProject(usize), // project index
    RemoveProject(usize),
    /// Kill the PTY, keep the clone, mark Suspended. Next click cold-resumes.
    CloseSessionKeepClone { project_idx: usize, session_idx: usize },
    /// Ask for confirmation before discarding — sets `confirming_discard`.
    RequestDiscardSession { project_idx: usize, session_idx: usize },
    /// Cancel an in-flight discard confirmation.
    CancelDiscard,
    /// Permanently delete the clone and remove the session from state.
    DiscardSession { project_idx: usize, session_idx: usize },
    SelectSession { project_idx: usize, session_idx: usize },
    /// Merge an archived session ref into canonical's working tree.
    MergeArchive { project_idx: usize, archive_idx: usize },
    /// Delete an archive ref without merging.
    DeleteArchive { project_idx: usize, archive_idx: usize },
    /// Merge session work into canonical and close (archive + merge + delete clone).
    MergeAndClose { project_idx: usize, session_idx: usize },
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
    /// Toggle the left sidebar visibility.
    ToggleSidebar,
    /// Toggle the right sidebar visibility.
    ToggleRightSidebar,
    /// Source path missing — open folder picker so the user can relocate.
    RelocateProject(usize),
    /// Proceed with session creation despite dirty canonical.
    ProceedDirtySession(usize),
    /// Cancel dirty-state session creation.
    CancelDirtySession,
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
    /// Auto-resume a session after launch. Fires once from the first render
    /// tick so `resume_session` has a valid `window` / `cx`.
    ResumeSession { project_idx: usize, session_idx: usize },
    /// Activate the Chrome tab linked to the currently-active session,
    /// creating one if the session has no tab id yet or the stored id is
    /// stale. Fired on session switch, session resume, and Browser-tab
    /// click.
    SyncBrowserToActiveSession,
    /// Close the Chrome tab linked to the given session and clear its
    /// stored tab id. User-initiated via the Browser tab's Close button.
    CloseBrowserTabForSession { project_idx: usize, session_idx: usize },
    /// Open (or re-focus) the scratch pad compose overlay.
    OpenScratchPad,
    /// Replace the global terminal font size and persist. Emitted by the
    /// Settings window, by Cmd+=/Cmd+- (as a clamped new value), and by
    /// Cmd+0 (reset to DEFAULT_FONT_SIZE). The handler clamps again,
    /// writes `user_settings.font_size`, saves to disk, and broadcasts
    /// the new value to every open `TerminalView`.
    UpdateFontSize(f32),
}

/// Position of a session in the project tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionCursor {
    pub(crate) project_idx: usize,
    pub(crate) session_idx: usize,
}
