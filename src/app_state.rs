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
use crate::config;
use crate::project::Project;
use crate::settings::Settings;
use crate::{new_session_modal, rich, scratch_pad, settings_window, state, text_input, transcript};

pub(crate) const SIDEBAR_MIN_WIDTH: f32 = 160.0;
pub(crate) const DRAWER_MIN_HEIGHT: f32 = 100.0;
pub(crate) const RIGHT_SIDEBAR_MIN_WIDTH: f32 = 160.0;

/// Which view is shown in the main (center) column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MainTab {
    Claude,
    /// Read-only project comprehension surface (file tree + preview).
    /// Editing happens in an external editor — see `reader.rs`. Formerly
    /// "Editor"; repositioned per DEV-43.
    Reader,
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

/// Git changes panel (right panel body): file list + selected diff.
/// Data is loaded asynchronously off the UI thread; the generation
/// counters let late results from superseded loads be dropped.
#[derive(Default)]
pub(crate) struct ChangesPanelState {
    /// Changed files from `git status`, staged and unstaged entries mixed
    /// (each entry knows its side). Rebuilt wholesale on every refresh.
    pub(crate) files: Vec<crate::git::ChangedFile>,
    /// Selected file row: (repo-relative path, staged side).
    pub(crate) selected: Option<(String, bool)>,
    /// Diff text for the selected file, once loaded.
    pub(crate) diff: Option<crate::git::FileDiff>,
    /// Clone directory the current `files` list was loaded from. Compared
    /// against the active session's clone dir each render to detect that a
    /// refresh is needed (session switch, first open).
    pub(crate) repo_dir: Option<PathBuf>,
    /// False when `repo_dir` turned out not to be a git work tree.
    pub(crate) is_repo: bool,
    pub(crate) loading: bool,
    pub(crate) diff_loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) diff_error: Option<String>,
    /// A refresh arrived while one was in flight — re-run on completion.
    /// Bounds git-status concurrency to one subprocess per AppState.
    pub(crate) refresh_queued: bool,
    /// Bumped on every refresh kick-off; async results carrying an older
    /// generation are discarded.
    pub(crate) refresh_gen: u64,
    /// Same contract as `refresh_gen`, for diff loads.
    pub(crate) diff_gen: u64,
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

/// Outcome of loading a file into the Reader preview. Distinguishes readable
/// text from files that intentionally degrade (binary, oversized, unreadable)
/// so the UI can explain *why* nothing is shown rather than rendering noise.
pub(crate) enum PreviewKind {
    /// UTF-8 (lossy) text ready to render.
    Text(String),
    /// Contained NUL bytes — treated as binary and not rendered.
    Binary,
    /// Exceeded the preview size cap; the value is the file's byte length.
    TooLarge(u64),
    /// `stat`/`read` failed; the value is the error message.
    Unreadable(String),
}

/// A loaded Reader preview: which file, and what came back.
pub(crate) struct Preview {
    pub(crate) path: PathBuf,
    pub(crate) kind: PreviewKind,
}

/// One in-file find hit: 1-based line, and the byte offset + length of the
/// matched substring within that line's text.
#[derive(Clone, Copy)]
pub(crate) struct FindMatch {
    pub(crate) line: usize,
    pub(crate) start: usize,
    pub(crate) len: usize,
}

/// The sticky, per-session slice of Reader state: which file was open, which
/// directories were expanded, and the Markdown view mode. Snapshotted per
/// workspace root so switching sessions restores each session's own view.
#[derive(Clone, Default)]
pub(crate) struct ReaderSelection {
    pub(crate) selected_path: Option<PathBuf>,
    pub(crate) expanded_dirs: HashSet<PathBuf>,
    pub(crate) md_view_source: bool,
}

/// Reader tab file-tree + preview state.
pub(crate) struct ReaderState {
    /// File path currently selected in the Reader tab's file tree.
    pub(crate) selected_path: Option<PathBuf>,
    /// Directories expanded in the Reader tab's file tree.
    pub(crate) expanded_dirs: HashSet<PathBuf>,
    /// Cached preview of the currently selected file.
    pub(crate) preview: Option<Preview>,
    /// Right-click context menu target for the Reader file tree.
    /// Stores (right-clicked path, window-space position of the click).
    pub(crate) context_menu: Option<(PathBuf, Point<Pixels>)>,
    /// In-file find query (substring). Empty when the find bar is inactive.
    pub(crate) find_query: String,
    /// Whether the in-file find bar is visible.
    pub(crate) find_active: bool,
    /// All matches of `find_query` in the current preview, in document order.
    /// Recomputed whenever the query or file changes.
    pub(crate) find_matches: Vec<FindMatch>,
    /// Index into `find_matches` of the currently-focused hit.
    pub(crate) find_current: usize,
    /// For Markdown files: show raw source instead of the rendered view.
    /// Ignored for non-Markdown files. Reset on each file selection.
    pub(crate) md_view_source: bool,
    /// Scroll handle for the rendered-Markdown body, so the outline can scroll
    /// a heading into view (DEV-65).
    pub(crate) md_scroll: gpui::ScrollHandle,
    /// Recently opened files (most-recent first, deduped, capped). Feeds the
    /// Cmd+P empty-query view and biases fuzzy ranking.
    pub(crate) recent: Vec<PathBuf>,
    /// Target line (1-based) to reveal/scroll-to in the source view after a
    /// deep-link navigation. Cleared when the user picks a different file.
    pub(crate) reveal_line: Option<usize>,
    /// Scroll handle for the source body, so deep links can scroll a target
    /// line into view (DEV-44).
    pub(crate) source_scroll: gpui::ScrollHandle,
    /// Workspace root the live Reader fields currently reflect. Compared each
    /// frame against the active session's root to detect a session switch.
    pub(crate) active_root: Option<PathBuf>,
    /// Per-session sticky Reader selection, keyed by workspace root (DEV-66).
    pub(crate) sessions: std::collections::HashMap<PathBuf, ReaderSelection>,
}

/// A fingerprint of the sidebar's *shape*: how many projects exist, how many
/// sessions across all of them, how many archives across all of them.
///
/// Confirmation gates address their target by index, so any change to these
/// three counts can shift the thing a prompt points at. Comparing a stamp taken
/// when the prompt was armed against the current one is how a stale prompt is
/// detected.
///
/// Deliberately counts *structure only*. Session status is excluded: agent
/// statuses flip every few seconds, and a prompt that evaporated while the user
/// was still reading it would be a worse bug than the one this prevents.
pub(crate) type StructuralStamp = (usize, usize, usize);

/// Compute the stamp for a project list. Free function over a slice rather than
/// a method on `AppState` so it can be tested without standing one up.
pub(crate) fn structural_stamp(projects: &[Project]) -> StructuralStamp {
    (
        projects.len(),
        projects.iter().map(|p| p.sessions.len()).sum(),
        projects.iter().map(|p| p.archives.len()).sum(),
    )
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
    /// Project index awaiting remove-project confirmation. When `Some(idx)`
    /// the project header row shows Confirm/Cancel instead of the ✕ button.
    pub(crate) remove_project: Option<usize>,
    /// Session cursor awaiting merge-with-uncommitted-changes confirmation.
    pub(crate) dirty_merge: Option<SessionCursor>,
    /// Archive awaiting permanent ref deletion confirmation.
    pub(crate) delete_archive: Option<(usize, usize)>,
    /// Project index whose *entire* archive list awaits bulk-deletion
    /// confirmation. Kept separate from `delete_archive` so the two gates
    /// can never be confused for one another; arming this one clears that one.
    pub(crate) delete_all_archives: Option<usize>,
    /// The structural stamp taken when the currently-armed gate was set, or
    /// `None` when no index-carrying gate is armed. Set by
    /// `AppState::arm_confirmation`; compared by `AppState::confirmation_is_stale`.
    pub(crate) armed_at: Option<StructuralStamp>,
}

impl ConfirmationState {
    /// Is any index-carrying gate currently armed?
    ///
    /// Answering a prompt clears its own gate but leaves `armed_at` behind, so
    /// the stamp alone is not proof that a prompt is open. Staleness is only
    /// meaningful while something is actually armed.
    pub(crate) fn any_armed(&self) -> bool {
        self.discard.is_some()
            || self.dirty_session.is_some()
            || self.remove_project.is_some()
            || self.dirty_merge.is_some()
            || self.delete_archive.is_some()
            || self.delete_all_archives.is_some()
    }

    /// Clear every gate that addresses something by index, plus the stamp.
    ///
    /// `quit` is deliberately left alone: it is a global banner that names no
    /// project, session, or archive, so nothing can go stale underneath it.
    pub(crate) fn dismiss_armed(&mut self) {
        self.discard = None;
        self.dirty_session = None;
        self.remove_project = None;
        self.dirty_merge = None;
        self.delete_archive = None;
        self.delete_all_archives = None;
        self.armed_at = None;
    }
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
    pub(crate) changes: ChangesPanelState,
    pub(crate) drawer: DrawerState,
    pub(crate) reader: ReaderState,
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
    /// Transient status/result banner for session-sync actions (sync-up /
    /// pull). Dismissed by the user or replaced by the next message.
    pub(crate) sync_notice: Option<String>,
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
    /// Reader in-file find input entity. Its text mirrors into
    /// `ReaderState::find_query` for match highlighting.
    pub(crate) reader_find_input: Entity<text_input::TextInput>,
    /// Cmd+P fuzzy file-retrieval overlay. `Some` while open.
    pub(crate) file_palette: Option<crate::reader::palette::FilePalette>,
    /// Query input for the Cmd+P overlay. Text mirrors into palette results.
    pub(crate) file_palette_input: Entity<text_input::TextInput>,
    /// Background-built, cached workspace file index (DEV-40). Backs Cmd+P and
    /// content search so large repos stay responsive.
    pub(crate) file_index: crate::reader::index::FileIndex,
    /// Cmd+Shift+F content/symbol search overlay. `Some` while open.
    pub(crate) search: Option<crate::reader::search::SearchState>,
    /// Query input for the search overlay.
    pub(crate) search_input: Entity<text_input::TextInput>,
    /// Cmd+Shift+P global command palette. `Some` while open.
    pub(crate) command_palette: Option<crate::reader::command::CommandPalette>,
    /// Query input for the command palette.
    pub(crate) command_palette_input: Entity<text_input::TextInput>,
    /// Inline project-settings panel: default-branch override input.
    pub(crate) project_branch_input: Entity<text_input::TextInput>,
    /// Inline project-settings panel: remote-name override input.
    pub(crate) project_remote_input: Entity<text_input::TextInput>,
    /// Current sidebar filter text (lowercased for matching).
    pub(crate) sidebar_filter: String,
    /// Right-click context menu on a session row: (cursor, click position).
    pub(crate) session_context_menu: Option<(SessionCursor, Point<Pixels>)>,
    /// Right-click context menu for a project header: (project_idx, position).
    pub(crate) project_context_menu: Option<(usize, Point<Pixels>)>,
    /// "Edit session" modal for renaming/commenting an existing session.
    pub(crate) edit_session_modal: Option<Entity<new_session_modal::EditSessionModal>>,
    /// Interactive naming modal — shown when NamingMode::Interactive generates suggestions.
    pub(crate) naming_modal: Option<Entity<new_session_modal::NamingModal>>,
    /// Remote-session browser (sync pull). `Some` while the overlay is visible.
    pub(crate) remote_browser: Option<Entity<crate::remote_browser::RemoteBrowser>>,
    /// Persistent Scratch Pad submission history across all projects.
    /// Loaded from state.json on startup, appended on submit, written back
    /// on every save_state. Filtered by project when the overlay opens.
    pub(crate) scratch_pad_history: Vec<state::ScratchPadEntry>,
    /// Transient storage for a startup command that finished on a non-window
    /// context. The `SpawnStartupTerminals` action picks this up in the
    /// next render tick where `window` is available.
    pub(crate) pending_startup:
        Option<(SessionCursor, config::ProjectConfig, Option<u16>, PathBuf)>,
    /// Transient status line for the base-infra (Traefik) lifecycle, shown
    /// in the Settings → Infrastructure pane. e.g. "Starting…", "Running",
    /// or an error message.
    pub(crate) base_infra_status: Option<String>,
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
    /// Set by the Debug menu or agent request-file watcher.
    pub(crate) capture_ui_requested: bool,
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

#[cfg(test)]
mod tests {
    use super::{structural_stamp, ConfirmationState};
    use crate::actions::{
        ArchiveAction, PendingAction, ProjectAction, SessionAction, SessionCursor,
    };
    use crate::project::Project;
    use crate::session::Session;
    use crate::state::ArchivedSession;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn session() -> Session {
        let now = SystemTime::now();
        Session::suspended_from_persisted(
            "id".into(),
            "label".into(),
            now,
            now,
            Duration::ZERO,
            None,
            false,
        )
    }

    fn archive() -> ArchivedSession {
        ArchivedSession {
            id: "arch".into(),
            project_id: "proj".into(),
            label: "archived".into(),
            archived_at: 0,
            merge_error: None,
        }
    }

    fn project(sessions: usize, archives: usize) -> Project {
        let mut p = Project::new("proj".into(), PathBuf::from("/tmp/proj"));
        p.sessions = (0..sessions).map(|_| session()).collect();
        p.archives = (0..archives).map(|_| archive()).collect();
        p
    }

    fn armed_state() -> ConfirmationState {
        ConfirmationState {
            discard: None,
            dirty_session: None,
            quit: false,
            remove_project: None,
            dirty_merge: None,
            delete_archive: None,
            delete_all_archives: None,
            armed_at: None,
        }
    }

    #[test]
    fn stamp_counts_projects_sessions_and_archives() {
        let projects = vec![project(2, 1), project(3, 0)];
        assert_eq!(structural_stamp(&projects), (2, 5, 1));
    }

    #[test]
    fn stamp_is_stable_when_nothing_structural_changes() {
        let mut projects = vec![project(2, 1)];
        let before = structural_stamp(&projects);
        // Renaming and re-labelling touch no count. Status churn must not
        // dismiss a prompt, which is the whole point of counting shape only.
        projects[0].name = "renamed".into();
        assert_eq!(structural_stamp(&projects), before);
    }

    #[test]
    fn stamp_changes_when_an_archive_is_removed() {
        let mut projects = vec![project(1, 3)];
        let before = structural_stamp(&projects);
        projects[0].archives.clear();
        assert_ne!(structural_stamp(&projects), before);
    }

    #[test]
    fn stamp_changes_when_a_session_is_appended() {
        // An append shifts no existing index, but still dismisses. Deliberate:
        // an unnecessary re-click is cheaper than a misdirected delete.
        let mut projects = vec![project(1, 0)];
        let before = structural_stamp(&projects);
        projects[0].sessions.push(session());
        assert_ne!(structural_stamp(&projects), before);
    }

    #[test]
    fn stamp_cannot_see_a_reorder() {
        // Documents the known blind spot the reorder handlers cover by hand.
        let projects = vec![project(1, 0), project(2, 0)];
        let reordered = vec![project(2, 0), project(1, 0)];
        assert_eq!(structural_stamp(&projects), structural_stamp(&reordered));
    }

    #[test]
    fn nothing_armed_by_default() {
        assert!(!armed_state().any_armed());
    }

    #[test]
    fn each_index_carrying_gate_counts_as_armed() {
        let cursor = SessionCursor {
            project_idx: 0,
            session_idx: 0,
        };
        let cases: Vec<Box<dyn Fn(&mut ConfirmationState)>> = vec![
            Box::new(move |c| c.discard = Some(cursor)),
            Box::new(|c| c.dirty_session = Some(0)),
            Box::new(|c| c.remove_project = Some(0)),
            Box::new(move |c| c.dirty_merge = Some(cursor)),
            Box::new(|c| c.delete_archive = Some((0, 0))),
            Box::new(|c| c.delete_all_archives = Some(0)),
        ];
        for arm in cases {
            let mut state = armed_state();
            arm(&mut state);
            assert!(state.any_armed());
        }
    }

    #[test]
    fn quit_banner_is_not_an_armed_gate() {
        // `quit` names no project or session, so nothing can go stale under it
        // and `dismiss_armed` must leave it standing.
        let mut state = armed_state();
        state.quit = true;
        assert!(!state.any_armed());
        state.dismiss_armed();
        assert!(state.quit);
    }

    #[test]
    fn dismiss_clears_every_gate_and_the_stamp() {
        let cursor = SessionCursor {
            project_idx: 1,
            session_idx: 2,
        };
        let mut state = armed_state();
        state.discard = Some(cursor);
        state.dirty_session = Some(1);
        state.remove_project = Some(1);
        state.dirty_merge = Some(cursor);
        state.delete_archive = Some((1, 2));
        state.delete_all_archives = Some(1);
        state.armed_at = Some((1, 2, 3));

        state.dismiss_armed();

        assert!(!state.any_armed());
        assert!(state.armed_at.is_none());
    }

    #[test]
    fn only_confirmation_gated_actions_are_droppable() {
        let gated: Vec<PendingAction> = vec![
            SessionAction::DiscardSession {
                project_idx: 0,
                session_idx: 0,
            }
            .into(),
            SessionAction::ProceedDirtyMerge {
                project_idx: 0,
                session_idx: 0,
            }
            .into(),
            SessionAction::ProceedDirtySession(0).into(),
            ArchiveAction::DeleteArchive {
                project_idx: 0,
                archive_idx: 0,
            }
            .into(),
            ArchiveAction::DeleteAllArchives { project_idx: 0 }.into(),
            ProjectAction::RemoveProject(0).into(),
        ];
        for action in gated {
            assert!(action.needs_confirmation(), "{action:?} should be gated");
        }
    }

    #[test]
    fn requesting_and_cancelling_are_not_gated_actions() {
        // These arm or clear a prompt. Dropping them on a stale stamp would
        // strand the user with a prompt they cannot dismiss.
        let ungated: Vec<PendingAction> = vec![
            SessionAction::RequestDiscardSession {
                project_idx: 0,
                session_idx: 0,
            }
            .into(),
            SessionAction::CancelDiscard.into(),
            ArchiveAction::RequestDeleteAllArchives { project_idx: 0 }.into(),
            ArchiveAction::CancelDeleteAllArchives.into(),
            ArchiveAction::CancelDeleteArchive.into(),
            ProjectAction::CancelRemoveProject.into(),
            ArchiveAction::RestoreArchive {
                project_idx: 0,
                archive_idx: 0,
            }
            .into(),
            ArchiveAction::MergeArchive {
                project_idx: 0,
                archive_idx: 0,
            }
            .into(),
        ];
        for action in ungated {
            assert!(
                !action.needs_confirmation(),
                "{action:?} should not be gated"
            );
        }
    }
}
