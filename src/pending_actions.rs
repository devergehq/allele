//! Pending action dispatch — processes queued UI actions during render.

use gpui::*;

use crate::actions::{
    ArchiveAction, BrowserAction, DrawerAction, OverlayAction, PendingAction,
    ProjectAction, SessionAction, SessionCursor, SettingsAction, SidebarAction,
};
use crate::app_state::AppState;
use crate::{clone, git, hooks, project, session, settings};
use crate::project::Project;
use session::{Session, SessionStatus};
use settings::{ProjectSave, Settings};
use crate::terminal::{clamp_font_size, TerminalView};

impl AppState {
    /// Process a single pending action. Called from the render method.
    pub(crate) fn dispatch_pending_action(
        &mut self,
        action: PendingAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut skip_refocus = false;
            match action {
                PendingAction::Session(SessionAction::NewInActive) => {
                    if let Some(active) = self.active {
                        self.add_session_to_project(active.project_idx, window, cx);
                    }
                }
                PendingAction::Session(SessionAction::CloseActive) => {
                    // Keyboard/menu "close" — preserve the clone so the user
                    // can cold-resume later. Discard is an explicit gesture only.
                    if let Some(active) = self.active {
                        self.close_session_keep_clone(active, window, cx);
                    }
                }
                PendingAction::Session(SessionAction::FocusActive) => {
                    if let Some(session) = self.active_session() {
                        if let Some(tv) = session.terminal_view.as_ref() {
                            let fh = tv.read(cx).focus_handle.clone();
                            fh.focus(window, cx);
                        }
                    }
                }
                PendingAction::Project(ProjectAction::OpenAtPath(path)) => {
                    let idx = self.create_project(path, cx);
                    // Auto-create first session for the new project
                    self.add_session_to_project(idx, window, cx);
                }
                PendingAction::Session(SessionAction::AddToProject(project_idx)) => {
                    self.add_session_to_project(project_idx, window, cx);
                }
                PendingAction::Project(ProjectAction::Remove(project_idx)) => {
                    self.remove_project(project_idx, window, cx);
                }
                PendingAction::Session(SessionAction::CloseKeepClone { project_idx, session_idx }) => {
                    self.close_session_keep_clone(
                        SessionCursor { project_idx, session_idx },
                        window,
                        cx,
                    );
                }
                PendingAction::Session(SessionAction::RequestDiscard { project_idx, session_idx }) => {
                    // Arm the inline confirmation gate. The sidebar row will
                    // render Confirm/Cancel buttons on the next frame.
                    self.confirmations.discard = Some(SessionCursor { project_idx, session_idx });
                    cx.notify();
                }
                PendingAction::Session(SessionAction::CancelDiscard) => {
                    self.confirmations.discard = None;
                    cx.notify();
                }
                PendingAction::Session(SessionAction::Discard { project_idx, session_idx }) => {
                    self.confirmations.discard = None;
                    self.remove_session(
                        SessionCursor { project_idx, session_idx },
                        window,
                        cx,
                    );
                }
                PendingAction::Archive(ArchiveAction::Merge { project_idx, archive_idx }) => {
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
                                    tracing::info!("Merged archive {session_id} into canonical");
                                }
                                Ok(git::MergeResult::AlreadyUpToDate) => {
                                    let _ = git::delete_ref(
                                        &project.source_path,
                                        &git::archive_ref_name(&session_id),
                                    );
                                    project.archives.remove(archive_idx);
                                    tracing::info!(
                                        "Archive {session_id} had no new commits — nothing to merge (already up to date)"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "merge_archive failed for {session_id}: {e}"
                                    );
                                }
                            }
                        }
                    }
                    self.save_state();
                    cx.notify();
                }
                PendingAction::Archive(ArchiveAction::Delete { project_idx, archive_idx }) => {
                    if let Some(project) = self.projects.get_mut(project_idx) {
                        if let Some(entry) = project.archives.get(archive_idx) {
                            let session_id = entry.id.clone();
                            let _ = git::delete_ref(
                                &project.source_path,
                                &git::archive_ref_name(&session_id),
                            );
                            project.archives.remove(archive_idx);
                            tracing::info!("Deleted archive ref for {session_id}");
                        }
                    }
                    self.save_state();
                    cx.notify();
                }
                PendingAction::Session(SessionAction::MergeAndClose { project_idx, session_idx }) => {
                    let cursor = SessionCursor { project_idx, session_idx };
                    if let Some(project) = self.projects.get_mut(cursor.project_idx) {
                        if cursor.session_idx < project.sessions.len() {
                            let session = &mut project.sessions[cursor.session_idx];
                            let clone_path = session.clone_path.clone();
                            let session_id = session.id.clone();
                            let session_label = session.label.clone();
                            let canonical = project.source_path.clone();
                            let proj_settings = project.settings.clone();

                            // Capture session metadata for potential restoration on failure
                            // (must happen before the mutable borrow for loading_sessions).
                            let restore_started = session.started_at;
                            let restore_last_active = session.last_active;
                            let restore_agent_id = session.agent_id.clone();

                            // If no clone or clone == canonical, just remove (no git ops).
                            let needs_git = clone_path.as_ref().map_or(false, |cp| *cp != canonical);

                            if needs_git {
                                let clone_path = clone_path.unwrap(); // safe: needs_git is true
                                let restore_clone = clone_path.clone();

                                // Show a placeholder while the background pipeline runs.
                                let placeholder_id = uuid::Uuid::new_v4().to_string();
                                {
                                    let project = self.projects.get_mut(cursor.project_idx).unwrap();
                                    project.loading_sessions.push(project::LoadingSession {
                                        id: placeholder_id.clone(),
                                        label: format!("{session_label} (rebasing & merging)"),
                                    });

                                    // Remove session from the list (frees PTY via Drop).
                                    // We DON'T call remove_session() because its background
                                    // task would delete the clone — we need the clone alive
                                    // until we know the merge succeeded.
                                    project.sessions.remove(cursor.session_idx);
                                }

                                // Update active cursor if it pointed at the removed session.
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
                                self.save_state();
                                cx.notify();

                                // Clones for restoration on failure (originals move into the background task).
                                let restore_id = session_id.clone();
                                let restore_label = session_label.clone();

                                // Spawn the archive → rebase → merge → delete pipeline on the background executor.
                                let placeholder_id_for_task = placeholder_id.clone();
                                let project_idx_for_task = cursor.project_idx;
                                cx.spawn(async move |this, cx| {
                                    let result = cx
                                        .background_executor()
                                        .spawn(async move {
                                            // 1. Auto-commit + fetch session branch as archive ref
                                            git::archive_session(&canonical, &clone_path, &session_id)?;

                                            // 2. Optionally fetch remote & rebase canonical onto remote tip
                                            let remote = proj_settings.resolved_remote();
                                            if proj_settings.rebase_before_merge && git::has_remote(&canonical, remote) {
                                                let branch_override = proj_settings.default_branch.as_deref();
                                                if let Err(e) = git::fetch_and_rebase_onto_remote_branch(&canonical, remote, branch_override) {
                                                    tracing::warn!("Rebase onto {remote} failed for {session_id}: {e}");
                                                    // Clean up the archive ref only — preserve the clone
                                                    let _ = git::delete_ref(
                                                        &canonical,
                                                        &git::archive_ref_name(&session_id),
                                                    );
                                                    anyhow::bail!("Rebase failed — resolve conflicts in the session and merge again. {e}");
                                                }
                                                tracing::info!("Rebased canonical onto {remote} for {session_id}");
                                            }

                                            // 3. Merge the archive ref using the configured strategy
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

                                            // 4. Always delete the archive ref (cleanup even on merge failure)
                                            let _ = git::delete_ref(
                                                &canonical,
                                                &git::archive_ref_name(&session_id),
                                            );

                                            match merge_result {
                                                Ok(git::MergeResult::Merged) => {
                                                    tracing::info!("Merged session {session_id} into canonical");
                                                }
                                                Ok(git::MergeResult::AlreadyUpToDate) => {
                                                    tracing::info!("Session {session_id} already up to date — nothing to merge");
                                                }
                                                Err(e) => {
                                                    tracing::warn!("merge_archive failed for {session_id}: {e}");
                                                    // Preserve clone — don't delete it on merge failure
                                                    anyhow::bail!("Merge failed — resolve conflicts in the session and merge again. {e}");
                                                }
                                            }

                                            // 5. Trash the APFS clone (near-instant rename) on
                                            //    success. Actual deletion deferred to startup purge.
                                            if let Err(e) = clone::trash_clone(&clone_path) {
                                                tracing::warn!("Failed to trash clone after merge for {session_id}: {e}");
                                            }
                                            Ok(())
                                        })
                                        .await;

                                    // Update UI on the main thread
                                    let _ = this.update(cx, |this: &mut Self, cx| {
                                        if let Some(project) = this.projects.get_mut(project_idx_for_task) {
                                            project.loading_sessions.retain(|l| l.id != placeholder_id_for_task);
                                        }

                                        if let Err(e) = &result {
                                            tracing::info!("Merge-and-close pipeline error: {e}");

                                            // Restore the session so the user can fix conflicts and retry
                                            let restored = Session::suspended_from_persisted(
                                                restore_id.clone(),
                                                restore_label.clone(),
                                                restore_started,
                                                restore_last_active,
                                                Some(restore_clone.clone()),
                                                false, // not merged — that's the point
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

                                        this.save_state();
                                        cx.notify();
                                    });
                                })
                                .detach();
                            } else {
                                // No clone to manage — mark merged so remove_session
                                // skips creating an archive entry.
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
                PendingAction::Session(SessionAction::Select { project_idx, session_idx }) => {
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
                PendingAction::Drawer(DrawerAction::Toggle) => {
                    skip_refocus = true;
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
                    self.save_state();
                }
                PendingAction::Drawer(DrawerAction::NewTab) => {
                    skip_refocus = true;
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
                        self.save_state();
                    }
                }
                PendingAction::Drawer(DrawerAction::SwitchTab(idx)) => {
                    skip_refocus = true;
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
                        self.save_state();
                    }
                }
                PendingAction::Drawer(DrawerAction::CloseTab(idx)) => {
                    skip_refocus = true;
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
                        self.save_state();
                    }
                }
                PendingAction::Drawer(DrawerAction::StartRenameTab(idx)) => {
                    skip_refocus = true;
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
                PendingAction::Drawer(DrawerAction::CommitRenameTab) => {
                    skip_refocus = true;
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
                        self.save_state();
                    }
                }
                PendingAction::Drawer(DrawerAction::CancelRenameTab) => {
                    skip_refocus = true;
                    let cursor_opt = self.drawer.rename.take().map(|(c, _, _)| c);
                    if let Some(cursor) = cursor_opt {
                        self.focus_active_drawer_tab(cursor, window, cx);
                    }
                    cx.notify();
                }
                PendingAction::Sidebar(SidebarAction::ToggleLeft) => {
                    self.sidebar.visible = !self.sidebar.visible;
                    self.save_settings();
                }
                PendingAction::Sidebar(SidebarAction::ToggleRight) => {
                    self.right_sidebar.visible = !self.right_sidebar.visible;
                    self.save_settings();
                }
                PendingAction::Project(ProjectAction::Relocate(project_idx)) => {
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
                                        tracing::info!(
                                            "Relocated project '{}': {} -> {}",
                                            project.name,
                                            project.source_path.display(),
                                            new_path.display()
                                        );
                                        project.source_path = new_path;
                                        project.name = Project::name_from_path(&project.source_path);
                                        this.save_settings();
                                    }
                                    cx.notify();
                                });
                            }
                        }
                    })
                    .detach();
                }
                PendingAction::Session(SessionAction::ProceedDirty(project_idx)) => {
                    // confirming_dirty_session stays Some so
                    // add_session_to_project skips the dirty check.
                    self.add_session_to_project(project_idx, window, cx);
                }
                PendingAction::Session(SessionAction::CancelDirty) => {
                    self.confirmations.dirty_session = None;
                    cx.notify();
                }
                PendingAction::Settings(SettingsAction::UpdateCleanupPaths(paths)) => {
                    skip_refocus = true;
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
                PendingAction::Session(SessionAction::Resume { project_idx, session_idx }) => {
                    let cursor = SessionCursor { project_idx, session_idx };
                    self.resume_session(cursor, window, cx);
                    self.sync_browser_to_active();
                }
                PendingAction::Browser(BrowserAction::SyncToActive) => {
                    skip_refocus = true;
                    self.sync_browser_to_active();
                }
                PendingAction::Browser(BrowserAction::CloseForSession { project_idx, session_idx }) => {
                    skip_refocus = true;
                    let cursor = SessionCursor { project_idx, session_idx };
                    let tab_id = self
                        .projects
                        .get(cursor.project_idx)
                        .and_then(|p| p.sessions.get(cursor.session_idx))
                        .and_then(|s| s.browser_tab_id);
                    if let Some(id) = tab_id {
                        self.platform.browser.close_tab(crate::platform::TabId(id));
                    }
                    if let Some(session) = self
                        .projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        session.browser_tab_id = None;
                    }
                    self.browser_status = "Chrome tab closed.".to_string();
                    self.save_state();
                }
                PendingAction::Overlay(OverlayAction::OpenScratchPad) => {
                    skip_refocus = true;
                    self.open_scratch_pad(window, cx);
                }
                PendingAction::Settings(SettingsAction::UpdateBrowserIntegration(enabled)) => {
                    skip_refocus = true;
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
                PendingAction::Settings(SettingsAction::UpdateAgents { agents, default_agent }) => {
                    skip_refocus = true;
                    self.user_settings.agents = agents;
                    self.user_settings.default_agent = default_agent;
                }
                PendingAction::Settings(SettingsAction::UpdateGitPullBeforeNewSession(enabled)) => {
                    skip_refocus = true;
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
                PendingAction::Settings(SettingsAction::UpdateFontSize(size)) => {
                    skip_refocus = true;
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
                PendingAction::Settings(SettingsAction::UpdateExternalEditor(cmd)) => {
                    skip_refocus = true;
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
}
