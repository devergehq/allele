//! PendingAction dispatcher — drains the queued PendingAction on AppState
//! and performs the side-effectful work.
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 6.
//! Phase 10 family-split the action enum into 8 sub-enums; dispatch is now
//! a 2-level match (wrapper → family). Phase 15 split each family arm
//! into its own `handle_*_action` handler method (see ARCHITECTURE.md §3.1).

use gpui::*;
use tracing::{info, warn};

use crate::actions::{
    ArchiveAction, BrowserAction, DrawerAction, OverlayAction, PendingAction, ProjectAction,
    SessionAction, SessionCursor, SettingsAction, SidebarAction,
};
use crate::app_state::AppState;
use crate::project::{self, Project};
use crate::session::{Session, SessionStatus};
use crate::settings::{ProjectSave, Settings};
use crate::terminal::{clamp_font_size, TerminalView};
use crate::{browser, clone, git, hooks};

impl AppState {
    pub(crate) fn dispatch_pending_action(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(action) = self.pending_action.take() else { return };
        // Dismiss the session context menu on any action.
        self.session_context_menu = None;
        let mut skip_refocus = false;
        match action {
            PendingAction::Session(a) => self.handle_session_action(a, &mut skip_refocus, window, cx),
            PendingAction::Archive(a) => self.handle_archive_action(a, &mut skip_refocus, window, cx),
            PendingAction::Drawer(a) => self.handle_drawer_action(a, &mut skip_refocus, window, cx),
            PendingAction::Sidebar(a) => self.handle_sidebar_action(a, &mut skip_refocus, window, cx),
            PendingAction::Project(a) => self.handle_project_action(a, &mut skip_refocus, window, cx),
            PendingAction::Settings(a) => self.handle_settings_action(a, &mut skip_refocus, window, cx),
            PendingAction::Browser(a) => self.handle_browser_action(a, &mut skip_refocus, window, cx),
            PendingAction::Overlay(a) => self.handle_overlay_action(a, &mut skip_refocus, window, cx),
        }

        // After any sidebar-triggered action, re-focus the active
        // terminal so keyboard input goes back to Claude Code.
        // ToggleDrawer manages its own focus, so skip it.
        if !skip_refocus {
            if let Some(session) = self.active_session() {
                if let Some(tv) = session.terminal_view.as_ref() {
                    let fh = tv.read(cx).focus_handle.clone();
                    fh.focus(window, cx);
                }
            }
        }
    }

    pub(crate) fn handle_session_action(
        &mut self,
        action: SessionAction,
        skip_refocus: &mut bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SessionAction::NewSessionInActiveProject => {
                if let Some(active) = self.active {
                    self.add_session_to_project(active.project_idx, window, cx);
                }
            }
            SessionAction::CloseActiveSession => {
                // Keyboard/menu "close" — preserve the clone so the user
                // can cold-resume later. Discard is an explicit gesture only.
                if let Some(active) = self.active {
                    self.close_session_keep_clone(active, window, cx);
                }
            }
            SessionAction::FocusActive => {
                if let Some(session) = self.active_session() {
                    if let Some(tv) = session.terminal_view.as_ref() {
                        let fh = tv.read(cx).focus_handle.clone();
                        fh.focus(window, cx);
                    }
                }
            }
            SessionAction::AddSessionToProject(project_idx) => {
                self.add_session_to_project(project_idx, window, cx);
            }
            SessionAction::OpenNewSessionModal(project_idx) => {
                *skip_refocus = true;
                self.open_new_session_modal(project_idx, window, cx);
            }
            SessionAction::AddSessionWithDetails {
                project_idx,
                label,
                branch_slug,
                agent_id,
                initial_prompt,
            } => {
                self.add_session_to_project_with_details(
                    project_idx,
                    label,
                    branch_slug,
                    agent_id,
                    initial_prompt,
                    window,
                    cx,
                );
            }
            SessionAction::CloseSessionKeepClone { project_idx, session_idx } => {
                self.close_session_keep_clone(
                    SessionCursor { project_idx, session_idx },
                    window,
                    cx,
                );
            }
            SessionAction::RequestDiscardSession { project_idx, session_idx } => {
                // Arm the inline confirmation gate. The sidebar row will
                // render Confirm/Cancel buttons on the next frame.
                self.confirming.discard = Some(SessionCursor { project_idx, session_idx });
                cx.notify();
            }
            SessionAction::CancelDiscard => {
                self.confirming.discard = None;
                cx.notify();
            }
            SessionAction::DiscardSession { project_idx, session_idx } => {
                self.confirming.discard = None;
                self.remove_session(
                    SessionCursor { project_idx, session_idx },
                    window,
                    cx,
                );
            }
            SessionAction::CancelDirtyMerge => {
                self.confirming.dirty_merge = None;
                cx.notify();
            }
            SessionAction::ProceedDirtyMerge { project_idx, session_idx } => {
                self.confirming.dirty_merge = None;
                self.execute_merge_and_close(
                    SessionCursor { project_idx, session_idx },
                    true, // discard_uncommitted
                    window,
                    cx,
                );
            }
            SessionAction::MergeAndClose { project_idx, session_idx } => {
                let cursor = SessionCursor { project_idx, session_idx };
                let is_dirty = self.projects.get(cursor.project_idx)
                    .and_then(|p| p.sessions.get(cursor.session_idx))
                    .and_then(|s| s.clone_path.as_ref())
                    .map(|cp| git::is_working_tree_dirty(cp))
                    .unwrap_or(false);

                if is_dirty {
                    self.confirming.dirty_merge = Some(cursor);
                    cx.notify();
                } else {
                    self.execute_merge_and_close(cursor, false, window, cx);
                }
            }
            SessionAction::SelectSession { project_idx, session_idx } => {
                let cursor = SessionCursor { project_idx, session_idx };
                // Clicking a Suspended session cold-resumes it; clicking
                // any other session just makes it the active one.
                let is_suspended = self
                    .projects
                    .get(project_idx)
                    .and_then(|p| p.sessions.get(session_idx))
                    .map(|s| s.status == SessionStatus::Suspended)
                    .unwrap_or(false);

                if is_suspended {
                    self.resume_session(cursor, window, cx);
                } else {
                    self.active = Some(cursor);
                    if let Some(session) = self.active_session() {
                        if let Some(tv) = session.terminal_view.as_ref() {
                            let fh = tv.read(cx).focus_handle.clone();
                            fh.focus(window, cx);
                        }
                    }
                }
                // Keep Chrome's active tab aligned with the active session.
                self.sync_browser_to_active();
            }
            SessionAction::ProceedDirtySession(project_idx) => {
                // confirming_dirty_session stays Some so
                // add_session_to_project skips the dirty check.
                self.add_session_to_project(project_idx, window, cx);
            }
            SessionAction::CancelDirtySession => {
                self.confirming.dirty_session = None;
                cx.notify();
            }
            SessionAction::ResumeSession { project_idx, session_idx } => {
                let cursor = SessionCursor { project_idx, session_idx };
                self.resume_session(cursor, window, cx);
                self.sync_browser_to_active();
            }
            SessionAction::SpawnStartupTerminals(cursor) => {
                if let Some((_, cfg, port, clone_path)) = self.pending_startup.take() {
                    self.spawn_terminals_and_preview(cursor, &cfg, port, &clone_path, window, cx);
                }
            }
            SessionAction::EditSession { project_idx, session_idx } => {
                *skip_refocus = true;
                self.open_edit_session_modal(project_idx, session_idx, window, cx);
            }
            SessionAction::RevealSessionInFinder { project_idx, session_idx } => {
                if let Some(session) = self.projects.get(project_idx)
                    .and_then(|p| p.sessions.get(session_idx))
                {
                    let path = session.clone_path.as_ref()
                        .unwrap_or(&self.projects[project_idx].source_path);
                    Self::reveal_in_finder(path);
                }
            }
            SessionAction::CopySessionPath { project_idx, session_idx } => {
                if let Some(session) = self.projects.get(project_idx)
                    .and_then(|p| p.sessions.get(session_idx))
                {
                    let path = session.clone_path.as_ref()
                        .unwrap_or(&self.projects[project_idx].source_path);
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        path.to_string_lossy().to_string(),
                    ));
                }
            }
            SessionAction::TogglePinSession { project_idx, session_idx } => {
                if let Some(session) = self.projects.get_mut(project_idx)
                    .and_then(|p| p.sessions.get_mut(session_idx))
                {
                    session.pinned = !session.pinned;
                }
                self.mark_state_dirty();
            }
            SessionAction::ApplySessionEdit {
                project_idx,
                session_idx,
                label,
                branch_slug,
                comment,
                pinned,
            } => {
                if let Some(session) = self.projects.get_mut(project_idx)
                    .and_then(|p| p.sessions.get_mut(session_idx))
                {
                    session.label = label.clone();
                    session.comment = comment;
                    session.pinned = pinned;
                    session.auto_naming_fired = true;

                    // Rename the git branch if a name was provided.
                    // Use the name directly — no allele/session/ prefix.
                    if let Some(name) = &branch_slug {
                        if let Some(clone_path) = &session.clone_path {
                            let sanitised = git::sanitise_branch_name(name, 100);
                            if !sanitised.is_empty() {
                                if let Err(e) = git::rename_current_branch(
                                    clone_path,
                                    &sanitised,
                                ) {
                                    warn!("branch rename failed: {e}");
                                }
                            }
                        }
                    }
                }
                self.mark_state_dirty();
            }
        }
    }

    fn execute_merge_and_close(
        &mut self,
        cursor: SessionCursor,
        discard_uncommitted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(project) = self.projects.get_mut(cursor.project_idx) {
            if cursor.session_idx < project.sessions.len() {
                let session = &mut project.sessions[cursor.session_idx];
                let clone_path = session.clone_path.clone();
                let session_id = session.id.clone();
                let session_label = session.label.clone();
                let canonical = project.source_path.clone();
                let proj_settings = project.settings.clone();

                let restore_started = session.started_at;
                let restore_last_active = session.last_active;
                let restore_agent_id = session.agent_id.clone();

                let needs_git = clone_path.as_ref().map_or(false, |cp| *cp != canonical);

                if needs_git {
                    let clone_path = clone_path.unwrap();
                    let restore_clone = clone_path.clone();

                    let placeholder_id = uuid::Uuid::new_v4().to_string();
                    {
                        let project = self.projects.get_mut(cursor.project_idx)
                            .expect("cursor produced by a sidebar click; project_idx always in bounds");
                        project.loading_sessions.push(project::LoadingSession {
                            id: placeholder_id.clone(),
                            label: format!("{session_label} (rebasing & merging)"),
                        });
                        project.sessions.remove(cursor.session_idx);
                    }

                    if let Some(active) = self.active {
                        if active == cursor {
                            let project = &self.projects[cursor.project_idx];
                            self.active = if !project.sessions.is_empty() {
                                let new_idx = cursor.session_idx.min(project.sessions.len() - 1);
                                Some(SessionCursor { project_idx: cursor.project_idx, session_idx: new_idx })
                            } else {
                                self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                                    if !p.sessions.is_empty() {
                                        Some(SessionCursor { project_idx: p_idx, session_idx: 0 })
                                    } else {
                                        None
                                    }
                                })
                            };
                        } else if active.project_idx == cursor.project_idx && active.session_idx > cursor.session_idx {
                            self.active = Some(SessionCursor {
                                project_idx: active.project_idx,
                                session_idx: active.session_idx - 1,
                            });
                        }
                    }
                    self.mark_state_dirty();
                    cx.notify();

                    let restore_id = session_id.clone();
                    let restore_label = session_label.clone();

                    let placeholder_id_for_task = placeholder_id.clone();
                    let project_idx_for_task = cursor.project_idx;
                    cx.spawn(async move |this, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                if discard_uncommitted {
                                    git::archive_session_committed_only(&canonical, &clone_path, &session_id)?;
                                } else {
                                    git::archive_session(&canonical, &clone_path, &session_id)?;
                                }

                                let remote = proj_settings.resolved_remote();
                                if proj_settings.rebase_before_merge && git::has_remote(&canonical, remote) {
                                    let branch_override = proj_settings.default_branch.as_deref();
                                    if let Err(e) = git::fetch_and_rebase_onto_remote_branch(&canonical, remote, branch_override) {
                                        warn!("Rebase onto {remote} failed for {session_id}: {e}");
                                        let _ = git::delete_ref(
                                            &canonical,
                                            &git::archive_ref_name(&session_id),
                                        );
                                        anyhow::bail!("Rebase failed — resolve conflicts in the session and merge again. {e}");
                                    }
                                    info!("Rebased canonical onto {remote} for {session_id}");
                                }

                                let merge_result = match proj_settings.merge_strategy {
                                    crate::settings::MergeStrategy::Merge => {
                                        git::merge_archive(&canonical, &session_id)
                                    }
                                    crate::settings::MergeStrategy::Squash => {
                                        git::squash_merge_archive(&canonical, &session_id)
                                    }
                                    crate::settings::MergeStrategy::RebaseThenMerge => {
                                        git::rebase_merge_archive(&canonical, &session_id)
                                    }
                                };

                                let _ = git::delete_ref(
                                    &canonical,
                                    &git::archive_ref_name(&session_id),
                                );

                                match merge_result {
                                    Ok(git::MergeResult::Merged) => {
                                        info!("Merged session {session_id} into canonical");
                                    }
                                    Ok(git::MergeResult::AlreadyUpToDate) => {
                                        info!("Session {session_id} already up to date — nothing to merge");
                                    }
                                    Err(e) => {
                                        warn!("merge_archive failed for {session_id}: {e}");
                                        anyhow::bail!("Merge failed — resolve conflicts in the session and merge again. {e}");
                                    }
                                }

                                if let Err(e) = clone::trash_clone(&clone_path) {
                                    warn!("Failed to trash clone after merge for {session_id}: {e}");
                                }
                                Ok(())
                            })
                            .await;

                        let _ = this.update(cx, |this: &mut Self, cx| {
                            if let Some(project) = this.projects.get_mut(project_idx_for_task) {
                                project.loading_sessions.retain(|l| l.id != placeholder_id_for_task);
                            }

                            if let Err(e) = &result {
                                warn!("Merge-and-close pipeline error: {e}");

                                let restored = Session::suspended_from_persisted(
                                    restore_id.clone(),
                                    restore_label.clone(),
                                    restore_started,
                                    restore_last_active,
                                    Some(restore_clone.clone()),
                                    false,
                                )
                                .with_agent_id(restore_agent_id.clone());
                                if let Some(project) = this.projects.get_mut(project_idx_for_task) {
                                    project.sessions.push(restored);
                                }

                                hooks::show_notification(
                                    "Merge failed",
                                    &format!("{restore_label}: resolve conflicts and merge again"),
                                );
                            }

                            this.mark_state_dirty();
                            cx.notify();
                        });
                    })
                    .detach();
                } else {
                    if let Some(project) = self.projects.get_mut(cursor.project_idx) {
                        if cursor.session_idx < project.sessions.len() {
                            project.sessions[cursor.session_idx].merged = true;
                        }
                    }
                    self.remove_session(cursor, window, cx);
                }
            }
        }
    }

    pub(crate) fn handle_archive_action(
        &mut self,
        action: ArchiveAction,
        _skip_refocus: &mut bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ArchiveAction::MergeArchive { project_idx, archive_idx } => {
                if let Some(project) = self.projects.get_mut(project_idx) {
                    if let Some(entry) = project.archives.get(archive_idx) {
                        let session_id = entry.id.clone();
                        let merge_result = match project.settings.merge_strategy {
                            crate::settings::MergeStrategy::Merge => {
                                git::merge_archive(&project.source_path, &session_id)
                            }
                            crate::settings::MergeStrategy::Squash => {
                                git::squash_merge_archive(&project.source_path, &session_id)
                            }
                            crate::settings::MergeStrategy::RebaseThenMerge => {
                                git::rebase_merge_archive(&project.source_path, &session_id)
                            }
                        };
                        match merge_result {
                            Ok(git::MergeResult::Merged) => {
                                let _ = git::delete_ref(
                                    &project.source_path,
                                    &git::archive_ref_name(&session_id),
                                );
                                project.archives.remove(archive_idx);
                                info!("Merged archive {session_id} into canonical");
                            }
                            Ok(git::MergeResult::AlreadyUpToDate) => {
                                let _ = git::delete_ref(
                                    &project.source_path,
                                    &git::archive_ref_name(&session_id),
                                );
                                project.archives.remove(archive_idx);
                                info!(
                                    "Archive {session_id} had no new commits — nothing to merge (already up to date)"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    "merge_archive failed for {session_id}: {e}"
                                );
                            }
                        }
                    }
                }
                self.mark_state_dirty();
                cx.notify();
            }
            ArchiveAction::DeleteArchive { project_idx, archive_idx } => {
                if let Some(project) = self.projects.get_mut(project_idx) {
                    if let Some(entry) = project.archives.get(archive_idx) {
                        let session_id = entry.id.clone();
                        let _ = git::delete_ref(
                            &project.source_path,
                            &git::archive_ref_name(&session_id),
                        );
                        project.archives.remove(archive_idx);
                        info!("Deleted archive ref for {session_id}");
                    }
                }
                self.mark_state_dirty();
                cx.notify();
            }
        }
    }

    pub(crate) fn handle_drawer_action(
        &mut self,
        action: DrawerAction,
        skip_refocus: &mut bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            DrawerAction::ToggleDrawer => {
                *skip_refocus = true;
                if let Some(cursor) = self.active {
                    let now_visible = {
                        let session = self.projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx));
                        if let Some(s) = session {
                            s.drawer_visible = !s.drawer_visible;
                            s.drawer_visible
                        } else {
                            false
                        }
                    };
                    if now_visible {
                        self.ensure_drawer_tabs(cursor, window, cx);
                        self.focus_active_drawer_tab(cursor, window, cx);
                    } else {
                        if let Some(session) = self.active_session() {
                            if let Some(tv) = session.terminal_view.as_ref() {
                                let fh = tv.read(cx).focus_handle.clone();
                                fh.focus(window, cx);
                            }
                        }
                    }
                }
                self.mark_state_dirty();
            }
            DrawerAction::NewDrawerTab => {
                *skip_refocus = true;
                if let Some(cursor) = self.active {
                    self.spawn_drawer_tab(cursor, None, None, window, cx);
                    if let Some(session) = self.projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        session.drawer_active_tab = session.drawer_tabs.len().saturating_sub(1);
                        session.drawer_visible = true;
                    }
                    self.focus_active_drawer_tab(cursor, window, cx);
                    self.mark_state_dirty();
                }
            }
            DrawerAction::SwitchDrawerTab(idx) => {
                *skip_refocus = true;
                if let Some(cursor) = self.active {
                    if let Some(session) = self.projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        if idx < session.drawer_tabs.len() {
                            session.drawer_active_tab = idx;
                        }
                    }
                    self.drawer.rename = None;
                    self.focus_active_drawer_tab(cursor, window, cx);
                    self.mark_state_dirty();
                }
            }
            DrawerAction::CloseDrawerTab(idx) => {
                *skip_refocus = true;
                if let Some(cursor) = self.active {
                    let (remaining, hide_drawer) = {
                        let session = self.projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx));
                        if let Some(s) = session {
                            if idx < s.drawer_tabs.len() {
                                s.drawer_tabs.remove(idx);
                            }
                            if s.drawer_active_tab >= s.drawer_tabs.len() {
                                s.drawer_active_tab = s.drawer_tabs.len().saturating_sub(1);
                            }
                            let empty = s.drawer_tabs.is_empty();
                            if empty {
                                s.drawer_visible = false;
                            }
                            (s.drawer_tabs.len(), empty)
                        } else {
                            (0, true)
                        }
                    };
                    if let Some((rc, ri, _)) = &self.drawer.rename {
                        if *rc == cursor && *ri >= remaining {
                            self.drawer.rename = None;
                        }
                    }
                    if hide_drawer {
                        if let Some(session) = self.active_session() {
                            if let Some(tv) = session.terminal_view.as_ref() {
                                let fh = tv.read(cx).focus_handle.clone();
                                fh.focus(window, cx);
                            }
                        }
                    } else {
                        self.focus_active_drawer_tab(cursor, window, cx);
                    }
                    self.mark_state_dirty();
                }
            }
            DrawerAction::StartRenameDrawerTab(idx) => {
                *skip_refocus = true;
                if let Some(cursor) = self.active {
                    let initial = self.projects
                        .get(cursor.project_idx)
                        .and_then(|p| p.sessions.get(cursor.session_idx))
                        .and_then(|s| s.drawer_tabs.get(idx))
                        .map(|t| t.name.clone());
                    if let Some(name) = initial {
                        self.drawer.rename = Some((cursor, idx, name));
                        let fh = self.drawer.rename_focus
                            .get_or_insert_with(|| cx.focus_handle())
                            .clone();
                        fh.focus(window, cx);
                        cx.notify();
                    }
                }
            }
            DrawerAction::CommitRenameDrawerTab => {
                *skip_refocus = true;
                if let Some((cursor, idx, buf)) = self.drawer.rename.take() {
                    let trimmed = buf.trim().to_string();
                    if !trimmed.is_empty() {
                        if let Some(session) = self.projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            if let Some(tab) = session.drawer_tabs.get_mut(idx) {
                                tab.name = trimmed;
                            }
                        }
                    }
                    self.focus_active_drawer_tab(cursor, window, cx);
                    self.mark_state_dirty();
                }
            }
            DrawerAction::CancelRenameDrawerTab => {
                *skip_refocus = true;
                let cursor_opt = self.drawer.rename.take().map(|(c, _, _)| c);
                if let Some(cursor) = cursor_opt {
                    self.focus_active_drawer_tab(cursor, window, cx);
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn handle_sidebar_action(
        &mut self,
        action: SidebarAction,
        _skip_refocus: &mut bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SidebarAction::ToggleSidebar => {
                self.sidebar.visible = !self.sidebar.visible;
                self.mark_settings_dirty();
            }
            SidebarAction::ToggleRightSidebar => {
                self.right_panel.visible = !self.right_panel.visible;
                self.mark_settings_dirty();
                if self.right_panel.visible {
                    self.refresh_changes(cx);
                }
            }
            SidebarAction::RefreshChanges => {
                self.refresh_changes(cx);
            }
            SidebarAction::SelectChangedFile { path, staged } => {
                self.changes.selected = Some((path.clone(), staged));
                self.load_changes_diff(path, staged, cx);
            }
            SidebarAction::ClearChangesSelection => {
                self.changes.selected = None;
                self.changes.diff = None;
                self.changes.diff_gen += 1; // drop any in-flight diff
            }
        }
    }

    pub(crate) fn handle_project_action(
        &mut self,
        action: ProjectAction,
        _skip_refocus: &mut bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ProjectAction::OpenProjectAtPath(path) => {
                let idx = self.create_project(path, cx);
                // Auto-create first session for the new project
                self.add_session_to_project(idx, window, cx);
            }
            ProjectAction::RequestRemoveProject(project_idx) => {
                self.confirming.remove_project = Some(project_idx);
                cx.notify();
            }
            ProjectAction::CancelRemoveProject => {
                self.confirming.remove_project = None;
                cx.notify();
            }
            ProjectAction::RemoveProject(project_idx) => {
                self.confirming.remove_project = None;
                self.remove_project(project_idx, window, cx);
            }
            ProjectAction::RelocateProject(project_idx) => {
                let paths = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Relocate project folder".into()),
                });

                cx.spawn(async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = paths.await {
                        if let Some(new_path) = paths.into_iter().next() {
                            let _ = this.update(cx, |this: &mut Self, cx| {
                                if let Some(project) = this.projects.get_mut(project_idx) {
                                    info!(
                                        "Relocated project '{}': {} -> {}",
                                        project.name,
                                        project.source_path.display(),
                                        new_path.display()
                                    );
                                    project.source_path = new_path;
                                    project.name = Project::name_from_path(&project.source_path);
                                    this.mark_settings_dirty();
                                }
                                cx.notify();
                            });
                        }
                    }
                })
                .detach();
            }
        }
    }

    pub(crate) fn handle_settings_action(
        &mut self,
        action: SettingsAction,
        skip_refocus: &mut bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SettingsAction::UpdateCleanupPaths(paths) => {
                *skip_refocus = true;
                self.user_settings.session_cleanup_paths = paths;
                // Persist. Settings::save() also needs the up-to-date
                // projects/window-geometry fields — synthesise them
                // from AppState before writing, mirroring the pattern
                // used in observe_window_bounds.
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateBrowserIntegration(enabled) => {
                *skip_refocus = true;
                self.user_settings.browser_integration_enabled = enabled;
                if !enabled {
                    self.browser_status.clear();
                }
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateAgents { agents, default_agent } => {
                *skip_refocus = true;
                self.user_settings.agents = agents;
                self.user_settings.default_agent = default_agent;
            }
            SettingsAction::UpdateGitPullBeforeNewSession(enabled) => {
                *skip_refocus = true;
                self.user_settings.git_pull_before_new_session = enabled;
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdatePromoteAttentionSessions(enabled) => {
                *skip_refocus = true;
                self.user_settings.promote_attention_sessions = enabled;
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateFontSize(size) => {
                *skip_refocus = true;
                let new_size = clamp_font_size(size);
                let changed = (self.user_settings.font_size - new_size).abs() > f32::EPSILON;
                self.user_settings.font_size = new_size;
                // Broadcast to every open terminal (per-session main view
                // and drawer tabs) so the change applies live. Collect
                // the handles first to avoid holding a borrow across the
                // per-view update calls.
                if changed {
                    let mut views: Vec<Entity<TerminalView>> = Vec::new();
                    for project in &self.projects {
                        for session in &project.sessions {
                            if let Some(tv) = session.terminal_view.as_ref() {
                                views.push(tv.clone());
                            }
                            for tab in &session.drawer_tabs {
                                views.push(tab.view.clone());
                            }
                        }
                    }
                    for view in views {
                        view.update(cx, |tv, cx| tv.set_font_size(new_size, window, cx));
                    }
                }
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateExternalEditor(cmd) => {
                *skip_refocus = true;
                let trimmed = cmd.trim();
                self.user_settings.external_editor_command = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateNamingConfig(config) => {
                *skip_refocus = true;
                self.user_settings.naming = config;
                let snapshot = Settings {
                    projects: self.projects.iter().map(|p| ProjectSave {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        source_path: p.source_path.clone(),
                        settings: p.settings.clone(),
                    }).collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateProjectSettings { project_idx, settings } => {
                *skip_refocus = true;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    project.settings = settings;
                }
                self.mark_settings_dirty();
            }
        }
    }

    pub(crate) fn handle_browser_action(
        &mut self,
        action: BrowserAction,
        skip_refocus: &mut bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        match action {
            BrowserAction::SyncBrowserToActiveSession => {
                *skip_refocus = true;
                self.sync_browser_to_active();
            }
            BrowserAction::CloseBrowserTabForSession { project_idx, session_idx } => {
                *skip_refocus = true;
                let cursor = SessionCursor { project_idx, session_idx };
                let tab_id = self
                    .projects
                    .get(cursor.project_idx)
                    .and_then(|p| p.sessions.get(cursor.session_idx))
                    .and_then(|s| s.browser_tab_id);
                if let Some(id) = tab_id {
                    let _ = browser::close_tab(id);
                }
                if let Some(session) = self
                    .projects
                    .get_mut(cursor.project_idx)
                    .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                {
                    session.browser_tab_id = None;
                }
                self.browser_status = "Chrome tab closed.".to_string();
                self.mark_state_dirty();
            }
        }
    }

    pub(crate) fn handle_overlay_action(
        &mut self,
        action: OverlayAction,
        skip_refocus: &mut bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            OverlayAction::OpenScratchPad => {
                *skip_refocus = true;
                self.open_scratch_pad(window, cx);
            }
        }
    }
}
