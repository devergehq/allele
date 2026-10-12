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
use crate::app_state::{AppState, GroupRename};
use crate::project::Project;
use crate::session::{Session, SessionStatus};
use crate::settings::{ProjectSave, Settings};
use crate::terminal::{clamp_font_size, TerminalView};
use crate::{browser, git};

impl AppState {
    pub(crate) fn dispatch_pending_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.pending_action.take() else {
            return;
        };
        // Dismiss the session context menu on any action.
        self.session_context_menu = None;
        // The group header's menu goes the same way, including on the actions
        // it is itself the source of — clicking an entry is the last thing that
        // menu does.
        self.group_headers.context_menu = None;

        // A prompt armed against a shape the sidebar no longer has cannot be
        // trusted: its index may now address a different session, project, or
        // archive. Dismiss it, and refuse whatever destructive action it was
        // about to authorise. Unrelated actions still go through — a stale
        // prompt is no reason to swallow an ordinary click.
        if self.confirmation_is_stale() {
            self.confirming.dismiss_armed();
            if action.needs_confirmation() {
                warn!("Dropped {action:?}: its confirmation went stale before it was answered");
                cx.notify();
                return;
            }
            cx.notify();
        }

        let mut skip_refocus = false;
        match action {
            PendingAction::Session(a) => {
                self.handle_session_action(a, &mut skip_refocus, window, cx)
            }
            PendingAction::Archive(a) => {
                self.handle_archive_action(a, &mut skip_refocus, window, cx)
            }
            PendingAction::Drawer(a) => self.handle_drawer_action(a, &mut skip_refocus, window, cx),
            PendingAction::Sidebar(a) => {
                self.handle_sidebar_action(a, &mut skip_refocus, window, cx)
            }
            PendingAction::Project(a) => {
                self.handle_project_action(a, &mut skip_refocus, window, cx)
            }
            PendingAction::Settings(a) => {
                self.handle_settings_action(a, &mut skip_refocus, window, cx)
            }
            PendingAction::Browser(a) => {
                self.handle_browser_action(a, &mut skip_refocus, window, cx)
            }
            PendingAction::Overlay(a) => {
                self.handle_overlay_action(a, &mut skip_refocus, window, cx)
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
                orchestration,
                group,
            } => {
                // Interned *before* the session is created, so the group is
                // recorded against the project even if the clone then fails —
                // which is what keeps a group's position stable rather than
                // dependent on whether a provisioning attempt succeeded.
                let interned = group.as_deref().and_then(|raw| {
                    match self.projects.get_mut(project_idx)?.intern_group(raw) {
                        Ok(name) => name,
                        // Unreachable from the modal, which refuses to submit.
                        // The session is still created — refusing the group is
                        // not a reason to refuse the workspace — it just starts
                        // ungrouped, which is the state it would have had if
                        // the field had been left empty.
                        Err(e) => {
                            warn!("ignoring group on new session: {}", e.message());
                            None
                        }
                    }
                });
                // The id is dropped by the sidebar path — a human's session is
                // followed by watching the sidebar, not by an awaiting caller —
                // but the group needs it: the clone lands asynchronously, so the
                // name is parked against the id and claimed at materialisation
                // (DEV-853). Dispatch needs the id too; see dispatch::create.
                let new_id = self.add_session_to_project_with_details(
                    project_idx,
                    label,
                    branch_slug,
                    agent_id,
                    initial_prompt,
                    orchestration,
                    window,
                    cx,
                );
                if let (Some(id), Some(group)) = (new_id, interned) {
                    self.pending_session_groups.insert(id, group);
                }
                self.mark_state_dirty();
            }
            SessionAction::CloseSessionKeepClone {
                project_idx,
                session_idx,
            } => {
                self.close_session_keep_clone(
                    SessionCursor {
                        project_idx,
                        session_idx,
                    },
                    window,
                    cx,
                );
            }
            SessionAction::RequestDiscardSession {
                project_idx,
                session_idx,
            } => {
                // Arm the inline confirmation gate. The sidebar row will
                // render Confirm/Cancel buttons on the next frame.
                self.arm_confirmation(|c| {
                    c.discard = Some(SessionCursor {
                        project_idx,
                        session_idx,
                    })
                });
                cx.notify();
            }
            SessionAction::CancelDiscard => {
                self.confirming.discard = None;
                cx.notify();
            }
            SessionAction::DiscardSession {
                project_idx,
                session_idx,
            } => {
                self.confirming.discard = None;
                self.remove_session(
                    SessionCursor {
                        project_idx,
                        session_idx,
                    },
                    window,
                    cx,
                );
            }
            SessionAction::SelectSession {
                project_idx,
                session_idx,
            } => {
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
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
            SessionAction::ChooseConversation {
                project_idx,
                session_idx,
            } => {
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                // On-demand: this path is how a user recovers after Allele has
                // already resumed the wrong conversation, so it must work for a
                // running session too, not just a suspended one.
                let details = self
                    .projects
                    .get(cursor.project_idx)
                    .and_then(|p| p.sessions.get(cursor.session_idx))
                    .and_then(|s| {
                        s.clone_path
                            .clone()
                            .map(|cp| (cp, s.label.clone(), s.claude_session_id().to_string()))
                    });
                if let Some((clone_path, label, current)) = details {
                    self.open_conversation_picker(cursor, &clone_path, &label, &current, cx);
                }
            }
            SessionAction::ResumeSession {
                project_idx,
                session_idx,
            } => {
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                self.resume_session(cursor, window, cx);
                self.sync_browser_to_active();
            }
            SessionAction::RetryStartup {
                project_idx,
                session_idx,
            } => {
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                // Clear the timed-out error and re-enter the startup flow.
                if let Some(session) = self
                    .projects
                    .get_mut(project_idx)
                    .and_then(|p| p.sessions.get_mut(session_idx))
                {
                    session.operation_error = None;
                }
                self.apply_project_config(cursor, window, cx);
            }
            SessionAction::SpawnStartupTerminals(cursor) => {
                if let Some((_, cfg, port, clone_path)) = self.pending_startup.take() {
                    // Re-check rather than trusting the earlier gate: a startup
                    // command can run for minutes, and the session can be
                    // switched to lightweight from the Edit modal while it is
                    // still in flight. Dropping the pending state above means
                    // this simply lands nothing. See DEV-400.
                    let runs_terminals = self
                        .projects
                        .get(cursor.project_idx)
                        .and_then(|p| p.sessions.get(cursor.session_idx))
                        .is_some_and(|s| s.orchestration.runs_terminals());
                    if runs_terminals {
                        self.spawn_terminals_and_preview(
                            cursor,
                            &cfg,
                            port,
                            &clone_path,
                            window,
                            cx,
                        );
                    }
                }
            }
            SessionAction::EditSession {
                project_idx,
                session_idx,
            } => {
                *skip_refocus = true;
                self.open_edit_session_modal(project_idx, session_idx, window, cx);
            }
            SessionAction::RevealSessionInFinder {
                project_idx,
                session_idx,
            } => {
                if let Some(session) = self
                    .projects
                    .get(project_idx)
                    .and_then(|p| p.sessions.get(session_idx))
                {
                    let path = session
                        .clone_path
                        .as_ref()
                        .unwrap_or(&self.projects[project_idx].source_path);
                    Self::reveal_in_finder(path);
                }
            }
            SessionAction::ReorderSession {
                project_idx,
                from,
                to,
            } => {
                if from != to {
                    if let Some(project) = self.projects.get_mut(project_idx) {
                        if from < project.sessions.len() && to < project.sessions.len() {
                            // A drop on a row in another group does both halves
                            // of what the gesture says: put the session here,
                            // and put it in the group "here" belongs to
                            // (DEV-937). DEV-853 refused the whole drop instead,
                            // because regrouping had no UI behind it yet; the
                            // regroup is now the feature.
                            //
                            // Interned rather than copied, so a spelling from a
                            // hand-edited `state.json` is normalised to the
                            // project's own on the way across.
                            let target = project.sessions[to].group.clone();
                            project.sessions[from].group = target
                                .as_deref()
                                .map(|name| project.group_order.intern(name));
                            let session = project.sessions.remove(from);
                            project.sessions.insert(to, session);
                            // Remap the active cursor through the move.
                            if let Some(active) = &mut self.active {
                                if active.project_idx == project_idx {
                                    let a = active.session_idx;
                                    active.session_idx = if a == from {
                                        to
                                    } else if from < a && to >= a {
                                        a - 1
                                    } else if from > a && to <= a {
                                        a + 1
                                    } else {
                                        a
                                    };
                                }
                            }
                            // A reorder leaves every count untouched, so the
                            // structural stamp cannot see it. Dismiss here.
                            self.confirming.dismiss_armed();
                            self.mark_state_dirty();
                            cx.notify();
                        }
                    }
                }
            }
            SessionAction::RegroupSession {
                project_idx,
                session_idx,
                group,
            } => {
                let mut changed = false;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    if session_idx < project.sessions.len() {
                        // Interned through the project, so a header drop obeys
                        // the same "first spelling wins" rule as the modals.
                        let next = match group.as_deref().map(|raw| project.intern_group(raw)) {
                            Some(Ok(name)) => name,
                            None => None,
                            // Unreachable from a header drop — the name comes
                            // from a group that exists, so it is already within
                            // the limit. Refused rather than truncated if it
                            // ever becomes reachable.
                            Some(Err(over)) => {
                                warn!("Refused a regroup: {}", over.message());
                                return;
                            }
                        };
                        let session = &mut project.sessions[session_idx];
                        changed = session.group != next;
                        session.group = next;
                    }
                }
                if changed {
                    self.confirming.dismiss_armed();
                    self.mark_state_dirty();
                    cx.notify();
                }
            }
            SessionAction::CopySessionPath {
                project_idx,
                session_idx,
            } => {
                if let Some(session) = self
                    .projects
                    .get(project_idx)
                    .and_then(|p| p.sessions.get(session_idx))
                {
                    let path = session
                        .clone_path
                        .as_ref()
                        .unwrap_or(&self.projects[project_idx].source_path);
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        path.to_string_lossy().to_string(),
                    ));
                }
            }
            SessionAction::TogglePinSession {
                project_idx,
                session_idx,
            } => {
                if let Some(session) = self
                    .projects
                    .get_mut(project_idx)
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
                orchestration,
                group,
            } => {
                // Interned against the project before the session is touched,
                // because both need the same `&mut` and the project's order is
                // what decides the spelling the session stores (DEV-853).
                //
                // Three outcomes, and collapsing any two of them loses a case:
                // a name to store, "clear it" (the field was emptied), and
                // "refuse it" (over-length). `Ok(None)` and `Err` are both
                // "no name", which is why the refusal has to be read out
                // separately — otherwise a rejected name silently *ungroups*
                // the session, doing damage the caller did not ask for on the
                // path that was supposed to change nothing.
                let interned = match self.projects.get_mut(project_idx) {
                    Some(project) => match group.as_deref() {
                        Some(raw) => match project.intern_group(raw) {
                            Ok(name) => Some(name),
                            // Unreachable from either modal (both refuse to
                            // submit) and from dispatch (it validates before
                            // provisioning). Kept correct for the next caller.
                            Err(e) => {
                                warn!("ignoring group on session edit: {}", e.message());
                                None
                            }
                        },
                        // Clearing the field removes the session from its group.
                        // The *name* stays in the project's order, so the group
                        // reappears in place if it refills.
                        None => Some(None),
                    },
                    None => None,
                };
                if let Some(session) = self
                    .projects
                    .get_mut(project_idx)
                    .and_then(|p| p.sessions.get_mut(session_idx))
                {
                    // `None` here is the refusal: leave the group exactly as it
                    // was rather than writing anything.
                    let group_changed = interned
                        .as_ref()
                        .is_some_and(|new| session.group.as_deref() != new.as_deref());
                    session.label = label.clone();
                    session.comment = comment;
                    session.pinned = pinned;
                    if let Some(new_group) = interned {
                        session.group = new_group;
                    }
                    // A group change reorders the sidebar while leaving every
                    // count untouched, so the structural stamp cannot see it and
                    // an armed discard prompt would silently relocate to
                    // whichever row moved into its index. Same reasoning as
                    // `ReorderSession` — see `confirmation_is_stale`.
                    if group_changed {
                        self.confirming.dismiss_armed();
                    }
                    // Takes effect from the next resume: `apply_project_config`
                    // reads it there. Deliberately does not retroactively start
                    // or tear down anything for the running session.
                    session.orchestration = orchestration;
                    session.auto_naming_fired = true;

                    // Rename the git branch if a name was provided.
                    // Use the name directly — no allele/session/ prefix.
                    if let Some(name) = &branch_slug {
                        if let Some(clone_path) = &session.clone_path {
                            let sanitised = git::sanitise_branch_name(name, 100);
                            if !sanitised.is_empty() {
                                if let Err(e) = git::rename_current_branch(clone_path, &sanitised) {
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

    pub(crate) fn handle_archive_action(
        &mut self,
        action: ArchiveAction,
        _skip_refocus: &mut bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ArchiveAction::RequestDeleteArchive {
                project_idx,
                archive_idx,
            } => {
                self.arm_confirmation(|c| c.delete_archive = Some((project_idx, archive_idx)));
                cx.notify();
            }
            ArchiveAction::CancelDeleteArchive => {
                self.confirming.delete_archive = None;
                cx.notify();
            }
            ArchiveAction::RequestDeleteAllArchives { project_idx } => {
                // Only one destructive strip open at a time — a per-archive
                // gate left armed underneath would ask two questions at once.
                self.arm_confirmation(|c| {
                    c.delete_archive = None;
                    c.delete_all_archives = Some(project_idx);
                });
                cx.notify();
            }
            ArchiveAction::CancelDeleteAllArchives => {
                self.confirming.delete_all_archives = None;
                cx.notify();
            }
            ArchiveAction::DeleteAllArchives { project_idx } => {
                self.confirming.delete_all_archives = None;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    // Drain once and rebuild from the failures. Removing by
                    // index inside a loop would shift every later index and
                    // silently skip half the list.
                    let entries = std::mem::take(&mut project.archives);
                    let total = entries.len();
                    for entry in entries {
                        match git::delete_ref(
                            &project.source_path,
                            &git::archive_ref_name(&entry.id),
                        ) {
                            Ok(()) => info!("Deleted archive ref for {}", entry.id),
                            Err(e) => {
                                // Keep the row: the ref still exists, and
                                // dropping it from the list would strand it
                                // with no way to retry.
                                warn!("delete_ref failed for archive {}: {e}", entry.id);
                                project.archives.push(entry);
                            }
                        }
                    }
                    let failed = project.archives.len();
                    info!(
                        "Bulk-deleted {} of {total} archive refs for {}",
                        total - failed,
                        project.name
                    );
                }
                self.mark_state_dirty();
                cx.notify();
            }
            ArchiveAction::DeleteArchive {
                project_idx,
                archive_idx,
            } => {
                self.confirming.delete_archive = None;
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
            ArchiveAction::RestoreArchive {
                project_idx,
                archive_idx,
            } => {
                // Snapshot everything we need under an immutable borrow, then
                // drop it before the heavy clone work so we hold no borrow of
                // AppState across it.
                let Some(project) = self.projects.get(project_idx) else {
                    return;
                };
                let Some(entry) = project.archives.get(archive_idx) else {
                    return;
                };
                let session_id = entry.id.clone();
                let label = entry.label.clone();
                let archived_at = entry.archived_at;
                let source_path = project.source_path.clone();
                let project_name = project.name.clone();
                let cleanup_paths = self.user_settings.session_cleanup_paths.clone();
                let project_name_for_ui = project_name.clone();

                // Resolve the agent the same way a fresh session would — the
                // original agent isn't recorded on the archive entry.
                let project_override =
                    crate::config::ProjectConfig::load(&source_path).and_then(|c| c.agent);
                let agent_id = crate::agents::resolve(
                    &self.user_settings.agents,
                    self.user_settings.default_agent.as_deref(),
                    project_override.as_deref(),
                    None,
                )
                .map(|a| a.id.clone());

                // Steps 1-3 touch the disk and run in the background: the
                // clone waits its turn behind any other and takes tens of
                // seconds on a large tree (DEV-755).
                cx.spawn(async move |this, cx| {
                    let prepared = cx
                        .background_executor()
                        .spawn(async move {
                            // 1. APFS-clone canonical into a fresh workspace.
                            let clone_path = match crate::clone::create_session_clone(
                                &source_path,
                                &project_name,
                                &session_id,
                                &cleanup_paths,
                            ) {
                                Ok(p) => p,
                                Err(e) => {
                                    warn!("restore: clone failed for {session_id}: {e}");
                                    return None;
                                }
                            };

                            // 2. Check the archived work out onto a fresh branch.
                            //    On failure leave the archive entry intact so the
                            //    user can retry, and clean up the half-made clone.
                            let branch = git::restored_branch_name(&session_id, &label);
                            if let Err(e) =
                                git::restore_archive_branch(&clone_path, &session_id, &branch)
                            {
                                warn!("restore: checkout failed for {session_id}: {e}");
                                let _ = crate::clone::delete_clone(&clone_path);
                                return None;
                            }

                            // 3. Marker file for orphan cleanup; keep it out of git.
                            if let Err(e) =
                                std::fs::write(clone_path.join(".allele-session"), &session_id)
                            {
                                warn!("restore: failed to write .allele-session marker: {e}");
                            }
                            git::exclude_pattern_in_clone(&clone_path, ".allele-session");
                            Some((session_id, label, clone_path, branch))
                        })
                        .await;

                    let _ = this.update(cx, move |this, cx| {
                        let Some((session_id, label, clone_path, branch)) = prepared else {
                            cx.notify();
                            return;
                        };

                        // 4. Re-add as a suspended session, then drop the archive
                        //    entry and its ref now the work lives in the clone.
                        //    Found by id: the lists may have moved meanwhile.
                        let started_at =
                            std::time::UNIX_EPOCH + std::time::Duration::from_secs(archived_at);
                        let now = std::time::SystemTime::now();
                        let session = Session::suspended_from_persisted(
                            session_id.clone(),
                            label,
                            started_at,
                            now,
                            std::time::Duration::ZERO,
                            Some(clone_path),
                        )
                        .with_agent_id(agent_id)
                        .with_branch_name(Some(branch));

                        if let Some(project) = this
                            .projects
                            .iter_mut()
                            .find(|p| p.name == project_name_for_ui)
                        {
                            project.sessions.push(session);
                            let _ = git::delete_ref(
                                &project.source_path,
                                &git::archive_ref_name(&session_id),
                            );
                            project.archives.retain(|a| a.id != session_id);
                            info!("Restored archived session {session_id} as a suspended session");
                        }
                        this.mark_state_dirty();
                        cx.notify();
                    });
                })
                .detach();
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
                        let session = self
                            .projects
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
                    } else if let Some(session) = self.active_session() {
                        if let Some(tv) = session.terminal_view.as_ref() {
                            let fh = tv.read(cx).focus_handle.clone();
                            fh.focus(window, cx);
                        }
                    }
                }
                self.mark_state_dirty();
            }
            DrawerAction::NewDrawerTab => {
                *skip_refocus = true;
                if let Some(cursor) = self.active {
                    self.spawn_drawer_tab(cursor, None, None, None, window, cx);
                    if let Some(session) = self
                        .projects
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
                    if let Some(session) = self
                        .projects
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
                        let session = self
                            .projects
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
                    let initial = self
                        .projects
                        .get(cursor.project_idx)
                        .and_then(|p| p.sessions.get(cursor.session_idx))
                        .and_then(|s| s.drawer_tabs.get(idx))
                        .map(|t| t.name.clone());
                    if let Some(name) = initial {
                        self.drawer.rename = Some((cursor, idx, name));
                        let fh = self
                            .drawer
                            .rename_focus
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
                        if let Some(session) = self
                            .projects
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

    /// Apply a committed group rename: the order keeps the group's slot, and
    /// every session in the group moves to the new spelling.
    ///
    /// Both halves or neither. [`crate::sidebar::groups::GroupOrder::rename`] is
    /// the only thing here that can refuse, so it runs first — by the time the
    /// sessions are rewritten the name is known good and the two cannot end up
    /// disagreeing about what the group is called.
    fn rename_group(&mut self, rename: &GroupRename) -> Result<(), String> {
        use crate::sidebar::groups::{same_group, RenameRefused};
        let Some(project) = self.projects.get_mut(rename.project_idx) else {
            return Err(RenameRefused::Unknown.message());
        };
        let to = project
            .group_order
            .rename(&rename.from, &rename.buffer)
            .map_err(|refused| refused.message())?;
        // Every member session, not only the ones on screen. A group *is* the
        // set of sessions carrying its name, so a name left behind on a row
        // hidden by the filter or the active-only toggle would split one group
        // into two the moment that row came back.
        for session in &mut project.sessions {
            if session
                .group
                .as_deref()
                .is_some_and(|g| same_group(g, &rename.from))
            {
                session.group = Some(to.clone());
            }
        }
        Ok(())
    }

    pub(crate) fn handle_sidebar_action(
        &mut self,
        action: SidebarAction,
        skip_refocus: &mut bool,
        window: &mut Window,
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
            SidebarAction::ToggleActiveOnly => {
                self.user_settings.sidebar_active_only = !self.user_settings.sidebar_active_only;
                self.mark_settings_dirty();
            }
            SidebarAction::ShowAllSessions => {
                self.user_settings.sidebar_active_only = false;
                self.mark_settings_dirty();
            }
            SidebarAction::ToggleAttentionBar => {
                self.user_settings.attention_bar_collapsed =
                    !self.user_settings.attention_bar_collapsed;
                self.mark_settings_dirty();
            }
            SidebarAction::RefreshChanges => {
                self.refresh_changes(cx);
            }
            // ── group management (DEV-937) ─────────────────────────────
            //
            // Every one of these is keyed on a group *name*, and every one
            // no-ops when the name is gone. A header's menu or a drag can
            // outlive the group it names: rename it from another window, or let
            // its last session finish, and an index would address whichever
            // group had moved into that slot.
            SidebarAction::ReorderGroup {
                project_idx,
                from,
                to,
            } => {
                let mut moved = false;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    let order = &mut project.group_order;
                    // Slot indices, so a group dropped where the ungrouped
                    // block sits takes that position and pushes the ungrouped
                    // block down — which is the only way the ungrouped block
                    // moves, having no header to drag.
                    if let (Some(from), Some(to)) =
                        (order.slot_of(Some(&from)), order.slot_of(Some(&to)))
                    {
                        moved = order.move_slot(from, to);
                    }
                }
                if moved {
                    self.mark_state_dirty();
                    cx.notify();
                }
            }
            SidebarAction::MoveGroup {
                project_idx,
                name,
                direction,
            } => {
                let mut moved = false;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    moved = project.group_order.move_group(&name, direction);
                }
                if moved {
                    self.mark_state_dirty();
                    cx.notify();
                }
            }
            SidebarAction::ToggleGroupCollapsed { project_idx, name } => {
                let mut toggled = false;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    if project.group_order.position(&name).is_some() {
                        project.group_order.toggle_collapsed(&name);
                        toggled = true;
                    }
                }
                if toggled {
                    // Folding rows away changes which rows exist, so an armed
                    // confirmation strip is no longer pointing where it was.
                    self.confirming.dismiss_armed();
                    self.mark_state_dirty();
                    cx.notify();
                }
            }
            SidebarAction::StartGroupRename { project_idx, name } => {
                // The field is about to take focus; the usual "give the terminal
                // its focus back" tail would take it straight off again.
                *skip_refocus = true;
                let known = self
                    .projects
                    .get(project_idx)
                    .and_then(|p| p.group_order.canonical(&name).map(str::to_string));
                if let Some(from) = known {
                    self.group_headers.rename = Some(GroupRename {
                        project_idx,
                        buffer: from.clone(),
                        from,
                        refusal: None,
                    });
                    let fh = self
                        .group_headers
                        .rename_focus
                        .get_or_insert_with(|| cx.focus_handle())
                        .clone();
                    fh.focus(window, cx);
                    cx.notify();
                }
            }
            SidebarAction::CommitGroupRename => {
                *skip_refocus = true;
                if let Some(rename) = self.group_headers.rename.take() {
                    match self.rename_group(&rename) {
                        Ok(()) => {
                            self.mark_state_dirty();
                        }
                        Err(refusal) => {
                            // The field stays open carrying what was typed, so a
                            // name can be corrected rather than retyped.
                            self.group_headers.rename = Some(GroupRename {
                                refusal: Some(refusal),
                                ..rename
                            });
                            if let Some(fh) = self.group_headers.rename_focus.clone() {
                                fh.focus(window, cx);
                            }
                        }
                    }
                    cx.notify();
                }
            }
            SidebarAction::CancelGroupRename => {
                *skip_refocus = true;
                self.group_headers.rename = None;
                cx.notify();
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
            ProjectAction::ReorderProject { from, to } => {
                if from != to && from < self.projects.len() && to < self.projects.len() {
                    let project = self.projects.remove(from);
                    self.projects.insert(to, project);
                    let remap = |i: usize| -> usize {
                        if i == from {
                            to
                        } else if from < i && to >= i {
                            i - 1
                        } else if from > i && to <= i {
                            i + 1
                        } else {
                            i
                        }
                    };
                    if let Some(active) = &mut self.active {
                        active.project_idx = remap(active.project_idx);
                    }
                    if let Some(editing) = &mut self.editing_project_settings {
                        *editing = remap(*editing);
                    }
                    // A reorder leaves every count untouched, so the structural
                    // stamp cannot see it. Dismiss here.
                    self.confirming.dismiss_armed();
                    self.mark_settings_dirty();
                    self.mark_state_dirty();
                    cx.notify();
                }
            }
            ProjectAction::RevealProjectInFinder(project_idx) => {
                if let Some(project) = self.projects.get(project_idx) {
                    Self::reveal_in_finder(&project.source_path);
                }
            }
            ProjectAction::CopyProjectPath(project_idx) => {
                if let Some(project) = self.projects.get(project_idx) {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        project.source_path.to_string_lossy().to_string(),
                    ));
                }
            }
            ProjectAction::RequestRemoveProject(project_idx) => {
                self.arm_confirmation(|c| c.remove_project = Some(project_idx));
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
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
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
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateAgents {
                agents,
                default_agent,
            } => {
                *skip_refocus = true;
                self.user_settings.agents = agents;
                self.user_settings.default_agent = default_agent;
            }
            SettingsAction::UpdateGitPullBeforeNewSession(enabled) => {
                *skip_refocus = true;
                self.user_settings.git_pull_before_new_session = enabled;
                let snapshot = Settings {
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdatePromoteAttentionSessions(enabled) => {
                *skip_refocus = true;
                self.user_settings.promote_attention_sessions = enabled;
                let snapshot = Settings {
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
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
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
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
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateNamingConfig(config) => {
                *skip_refocus = true;
                self.user_settings.naming = config;
                let snapshot = Settings {
                    projects: self
                        .projects
                        .iter()
                        .map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        })
                        .collect(),
                    ..self.user_settings.clone()
                };
                snapshot.save();
            }
            SettingsAction::UpdateProjectSettings {
                project_idx,
                settings,
            } => {
                *skip_refocus = true;
                if let Some(project) = self.projects.get_mut(project_idx) {
                    project.settings = settings;
                }
                self.mark_settings_dirty();
            }
            SettingsAction::UpdateBaseInfra(enabled) => {
                *skip_refocus = true;
                self.user_settings.base_infra_enabled = enabled;
                self.mark_settings_dirty();
                self.base_infra_status = Some(if enabled {
                    "Starting…".into()
                } else {
                    "Stopping…".into()
                });
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            if enabled {
                                crate::base_infra::up()
                            } else {
                                crate::base_infra::down()
                            }
                        })
                        .await;
                    let status = match result {
                        Ok(()) => {
                            if enabled {
                                "Running".to_string()
                            } else {
                                "Stopped".to_string()
                            }
                        }
                        Err(e) => e,
                    };
                    let _ = this.update(cx, |this: &mut AppState, cx| {
                        this.base_infra_status = Some(status);
                        cx.notify();
                    });
                })
                .detach();
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
            BrowserAction::CloseBrowserTabForSession {
                project_idx,
                session_idx,
            } => {
                *skip_refocus = true;
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                let tab_id = self
                    .projects
                    .get(cursor.project_idx)
                    .and_then(|p| p.sessions.get(cursor.session_idx))
                    .and_then(|s| s.browser_tab_id);
                if let Some(id) = tab_id {
                    if browser::close_tab(id) {
                        if let Some(session) = self
                            .projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            session.browser_tab_id = None;
                        }
                        self.browser_status = "Chrome tab closed.".to_string();
                        self.mark_state_dirty();
                    } else {
                        self.browser_status = "Could not close the Chrome tab. Check Chrome and Automation permission, then retry.".to_string();
                    }
                } else {
                    self.browser_status = "This session has no linked Chrome tab.".to_string();
                }
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
            OverlayAction::OpenFilePalette => {
                *skip_refocus = true;
                self.open_file_palette(window, cx);
            }
            OverlayAction::OpenSearch => {
                *skip_refocus = true;
                self.open_search(window, cx);
            }
            OverlayAction::OpenCommandPalette => {
                *skip_refocus = true;
                self.open_command_palette(window, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Deliberately *not* `use super::*`: this module glob-imports `gpui::*`,
    // which brings gpui's own `test` attribute into scope and sends the
    // `#[gpui::test]` expansion into infinite recursion.
    use crate::actions::{
        ArchiveAction, ProjectAction, SessionAction, SessionCursor, SettingsAction, SidebarAction,
    };
    use crate::app_state::fixture::{Fixture, FixtureSpec};
    use crate::project::Project;
    use crate::session::Session;
    use crate::settings::ProjectSettings;
    use crate::state::ArchivedSession;
    use gpui::TestAppContext;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn project_named(name: &str) -> Project {
        Project::new(name.to_string(), PathBuf::from(format!("/tmp/{name}")))
    }

    fn suspended_session(id: &str) -> Session {
        let now = SystemTime::now();
        Session::suspended_from_persisted(id.into(), id.into(), now, now, Duration::ZERO, None)
    }

    fn archive_entry(id: &str, project_id: &str) -> ArchivedSession {
        ArchivedSession {
            id: id.into(),
            project_id: project_id.into(),
            label: id.into(),
            archived_at: 0,
        }
    }

    /// Sidebar family. The toggle is pure UI geometry, but geometry is
    /// persisted — so the handler's whole persistence contract is the
    /// `settings_dirty` flag, not a write. Assert the intent first, then that
    /// the coordinator turns exactly that intent into exactly one write.
    #[gpui::test]
    fn toggle_sidebar_marks_settings_dirty_and_writes_once_at_checkpoint(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let before = fx.update(cx, |state, _w, _cx| state.sidebar.visible);

        let dirty = fx.dispatch(SidebarAction::ToggleSidebar, cx);

        assert_eq!(
            fx.update(cx, |state, _w, _cx| state.sidebar.visible),
            !before,
            "the handler must flip the sidebar"
        );
        assert_eq!(
            dirty,
            (false, true),
            "sidebar geometry lives in settings.json only — state.json must stay clean"
        );
        assert_eq!(
            fx.settings.save_count(),
            0,
            "ARCHITECTURE.md §7.2 — handlers record intent; they do not write"
        );

        fx.checkpoint(cx);

        assert_eq!(fx.settings.snapshot().sidebar_visible, !before);
        assert_eq!(fx.settings.save_count(), 1);
        assert_eq!(fx.state.save_count(), 0);

        // A second tick with nothing newly dirty must not re-write: that
        // coalescing is the entire point of the flag.
        fx.checkpoint(cx);
        assert_eq!(fx.settings.save_count(), 1);
    }

    /// Project family. A reorder mutates the project list (settings.json) and
    /// re-homes the active cursor (state.json), so it is the one action that
    /// must mark *both* flags — and the persisted project order has to come
    /// out matching the in-memory one.
    #[gpui::test]
    fn reorder_project_remaps_the_active_cursor_and_marks_both_files_dirty(
        cx: &mut TestAppContext,
    ) {
        let mut first = project_named("alpha");
        first.sessions.push(suspended_session("s-alpha"));
        let second = project_named("beta");

        let fx = Fixture::build(
            FixtureSpec {
                projects: vec![first, second],
                active: Some(SessionCursor {
                    project_idx: 0,
                    session_idx: 0,
                }),
                ..FixtureSpec::default()
            },
            cx,
        );

        let dirty = fx.dispatch(ProjectAction::ReorderProject { from: 0, to: 1 }, cx);
        assert_eq!(
            dirty,
            (true, true),
            "a reorder moves both the project list (settings.json) and the active cursor \
             (state.json), so it is the one action that must mark both"
        );

        let (names, active) = fx.update(cx, |state, _w, _cx| {
            (
                state
                    .projects
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>(),
                state.active,
            )
        });
        assert_eq!(names, vec!["beta".to_string(), "alpha".to_string()]);
        assert_eq!(
            active.map(|c| c.project_idx),
            Some(1),
            "the active cursor must follow its project, not stay on the index"
        );

        // This handler notifies, so the redraw's own `checkpoint_persistence()`
        // may already have flushed. Either way one more checkpoint is a no-op:
        // the flags are drained, not the writes repeated.
        fx.checkpoint(cx);

        let persisted_order: Vec<String> = fx
            .settings
            .snapshot()
            .projects
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(
            persisted_order,
            vec!["beta".to_string(), "alpha".to_string()]
        );
        assert_eq!(
            fx.state.snapshot().last_active_session_id.as_deref(),
            Some("s-alpha"),
            "state.json must name the session the remapped cursor now points at"
        );
        assert_eq!(fx.settings.save_count(), 1);
        assert_eq!(fx.state.save_count(), 1);
    }

    /// Settings family. Per-project orchestration settings live in
    /// settings.json; state.json must not be touched.
    #[gpui::test]
    fn update_project_settings_persists_to_settings_json_only(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![project_named("alpha")], cx);

        let dirty = fx.dispatch(
            SettingsAction::UpdateProjectSettings {
                project_idx: 0,
                settings: ProjectSettings {
                    default_branch: Some("develop".into()),
                    ..ProjectSettings::default()
                },
            },
            cx,
        );

        assert_eq!(dirty, (false, true));

        fx.checkpoint(cx);

        let saved = fx.settings.snapshot();
        assert_eq!(
            saved.projects[0].settings.default_branch.as_deref(),
            Some("develop")
        );
        assert_eq!(fx.state.save_count(), 0);
    }

    /// Archive family, via the dispatcher's stale-confirmation guard. A
    /// destructive action authorised against a sidebar shape that no longer
    /// exists must be dropped — and dropping it must persist nothing, because
    /// nothing was mutated.
    #[gpui::test]
    fn a_stale_confirmation_drops_the_delete_and_persists_nothing(cx: &mut TestAppContext) {
        let mut project = project_named("alpha");
        project.archives.push(archive_entry("arch-1", &project.id));
        let fx = Fixture::with_projects(vec![project], cx);

        let _ = fx.dispatch(
            ArchiveAction::RequestDeleteArchive {
                project_idx: 0,
                archive_idx: 0,
            },
            cx,
        );
        assert!(
            fx.update(cx, |state, _w, _cx| state.confirming.any_armed()),
            "the request arms the gate"
        );

        // The sidebar changes shape underneath the open prompt: index 0 may
        // now address something else entirely.
        fx.update(cx, |state, _w, _cx| {
            state.projects[0].sessions.push(suspended_session("s-new"));
        });

        let dirty = fx.dispatch(
            ArchiveAction::DeleteArchive {
                project_idx: 0,
                archive_idx: 0,
            },
            cx,
        );

        assert_eq!(
            fx.update(cx, |state, _w, _cx| state.projects[0].archives.len()),
            1,
            "the archive must survive an authorisation that went stale"
        );
        assert!(!fx.update(cx, |state, _w, _cx| state.confirming.any_armed()));
        assert_eq!(
            dirty,
            (false, false),
            "a dropped action mutated nothing, so it must record no persistence intent"
        );

        fx.checkpoint(cx);
        assert_eq!(fx.state.save_count(), 0);
        assert_eq!(fx.settings.save_count(), 0);
    }

    // ── DEV-853: session groups ────────────────────────────────────────
    //
    // The session family is otherwise untested (ARCHITECTURE.md §8) because its
    // handlers spawn PTYs and shell out to git. These two do neither: a group
    // edit is a state transition, and a refused reorder is the absence of one.
    // `branch_slug: None` keeps `ApplySessionEdit` off the git path.

    fn edit_group(cursor: SessionCursor, label: &str, group: Option<&str>) -> SessionAction {
        SessionAction::ApplySessionEdit {
            project_idx: cursor.project_idx,
            session_idx: cursor.session_idx,
            label: label.to_string(),
            branch_slug: None,
            comment: None,
            pinned: false,
            orchestration: crate::session::Orchestration::Nothing,
            group: group.map(str::to_string),
        }
    }

    fn grouped_project(groups: &[Option<&str>]) -> Project {
        let mut project = project_named("tc-portal");
        for (i, group) in groups.iter().enumerate() {
            let mut session = suspended_session(&format!("s{i}"));
            session.group = group.map(str::to_string);
            if let Some(name) = group {
                project.group_order.intern(name);
            }
            project.sessions.push(session);
        }
        project
    }

    fn group_of(fx: &Fixture, cursor: SessionCursor, cx: &mut TestAppContext) -> Option<String> {
        fx.update(cx, |state, _w, _cx| {
            state.projects[cursor.project_idx].sessions[cursor.session_idx]
                .group
                .clone()
        })
    }

    /// The acceptance criterion: editing a session's group moves it to that
    /// group at once, and the project records the name so the sidebar can order
    /// it.
    #[gpui::test]
    fn editing_a_session_assigns_its_group_and_records_the_name(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[None])], cx);
        let cursor = SessionCursor {
            project_idx: 0,
            session_idx: 0,
        };

        let dirty = fx.dispatch(edit_group(cursor, "Review #9586", Some("review")), cx);

        assert_eq!(group_of(&fx, cursor, cx).as_deref(), Some("review"));
        assert_eq!(
            fx.update(cx, |state, _w, _cx| state.projects[0]
                .group_names()
                .to_vec()),
            ["review"],
            "the project must record the name, or the sidebar cannot order the group"
        );
        assert!(
            dirty.0,
            "a group lives in state.json, so the handler must mark state dirty"
        );
    }

    /// Clearing the field returns the session to the ungrouped block — but the
    /// *name* stays in the project's order, which is what makes a group that
    /// empties and refills come back in its old position.
    #[gpui::test]
    fn clearing_the_group_ungroups_the_session_but_keeps_the_name(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);
        let cursor = SessionCursor {
            project_idx: 0,
            session_idx: 0,
        };

        fx.dispatch(edit_group(cursor, "Review #9586", None), cx);

        assert_eq!(group_of(&fx, cursor, cx), None);
        assert_eq!(
            fx.update(cx, |state, _w, _cx| state.projects[0]
                .group_names()
                .to_vec()),
            ["review"],
            "the group's place in the order survives losing its last session"
        );
    }

    /// `Review` typed into a project that already has `review` joins it, and is
    /// stored in the project's spelling rather than the one just typed.
    #[gpui::test]
    fn a_differently_cased_group_joins_the_existing_one(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review"), None])], cx);
        let second = SessionCursor {
            project_idx: 0,
            session_idx: 1,
        };

        fx.dispatch(edit_group(second, "Review #9590", Some("REVIEW")), cx);

        assert_eq!(
            group_of(&fx, second, cx).as_deref(),
            Some("review"),
            "stored in the first spelling the project saw"
        );
        assert_eq!(
            fx.update(cx, |state, _w, _cx| state.projects[0]
                .group_names()
                .to_vec()),
            ["review"],
            "one group, not two"
        );
    }

    /// An over-length name cannot reach here through the modals, which refuse to
    /// submit, nor through dispatch, which validates before provisioning. If one
    /// ever does, the session keeps the group it had.
    ///
    /// The distinction this guards: `Ok(None)` ("the user cleared the field")
    /// and `Err` ("that name is unusable") are both "no name", and collapsing
    /// them made a rejected edit *ungroup* the session — damage on the path that
    /// was meant to change nothing.
    #[gpui::test]
    fn an_over_length_group_leaves_the_session_alone(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);
        let cursor = SessionCursor {
            project_idx: 0,
            session_idx: 0,
        };
        let too_long = "r".repeat(crate::sidebar::groups::GROUP_NAME_MAX_LEN + 1);

        fx.dispatch(edit_group(cursor, "Review #9586", Some(&too_long)), cx);

        assert_eq!(
            group_of(&fx, cursor, cx).as_deref(),
            Some("review"),
            "refused — so the session keeps the group it had, rather than losing it"
        );
        assert_eq!(
            fx.update(cx, |state, _w, _cx| state.projects[0]
                .group_names()
                .to_vec()),
            ["review"],
            "and the refused name is not recorded as a group"
        );
    }

    /// The other half of the same distinction: an *empty* field really does
    /// clear the group. Without this beside the test above, a fix that simply
    /// never wrote `None` would pass.
    #[gpui::test]
    fn an_empty_group_field_still_clears_the_group(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);
        let cursor = SessionCursor {
            project_idx: 0,
            session_idx: 0,
        };

        fx.dispatch(edit_group(cursor, "Review #9586", Some("   ")), cx);

        assert_eq!(
            group_of(&fx, cursor, cx),
            None,
            "whitespace is an empty field, which is how a session leaves its group"
        );
    }

    fn session_ids(fx: &Fixture, cx: &mut TestAppContext) -> Vec<String> {
        fx.update(cx, |state, _w, _cx| {
            state.projects[0]
                .sessions
                .iter()
                .map(|s| s.id.clone())
                .collect()
        })
    }

    /// Dragging to reorder still works between two sessions in the same group.
    #[gpui::test]
    fn reordering_within_a_group_still_works(cx: &mut TestAppContext) {
        let fx =
            Fixture::with_projects(vec![grouped_project(&[Some("review"), Some("review")])], cx);

        fx.dispatch(
            SessionAction::ReorderSession {
                project_idx: 0,
                from: 1,
                to: 0,
            },
            cx,
        );

        assert_eq!(session_ids(&fx, cx), ["s1", "s0"]);
    }

    /// And between two ungrouped sessions, exactly as it did before groups
    /// existed — the ungrouped block is a run of rows like any other.
    #[gpui::test]
    fn reordering_within_the_ungrouped_block_still_works(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[None, None])], cx);

        fx.dispatch(
            SessionAction::ReorderSession {
                project_idx: 0,
                from: 1,
                to: 0,
            },
            cx,
        );

        assert_eq!(session_ids(&fx, cx), ["s1", "s0"]);
    }

    // ── DEV-937: a cross-group drop retags ─────────────────────────────
    //
    // DEV-853 refused these two drops outright, because regrouping had no UI
    // behind it: honouring the position while silently changing the group would
    // have done something nobody asked for. DEV-937 is that UI, so the drop now
    // means both halves of what the gesture says.

    /// The acceptance criterion: dropping a session on a row in another group
    /// moves it there, and lands it at that row's position.
    #[gpui::test]
    fn dropping_a_session_on_another_groups_row_retags_it(cx: &mut TestAppContext) {
        let fx =
            Fixture::with_projects(vec![grouped_project(&[Some("review"), Some("linear")])], cx);

        fx.dispatch(
            SessionAction::ReorderSession {
                project_idx: 0,
                from: 1,
                to: 0,
            },
            cx,
        );

        assert_eq!(session_ids(&fx, cx), ["s1", "s0"], "and it took the slot");
        assert_eq!(
            group_of(
                &fx,
                SessionCursor {
                    project_idx: 0,
                    session_idx: 0
                },
                cx
            )
            .as_deref(),
            Some("review"),
            "the dragged session joined the group it was dropped into"
        );
    }

    /// The other half: a drop on an ungrouped row clears the session's group and
    /// returns it to the ungrouped block.
    #[gpui::test]
    fn dropping_a_session_on_an_ungrouped_row_clears_its_group(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[None, Some("review")])], cx);

        fx.dispatch(
            SessionAction::ReorderSession {
                project_idx: 0,
                from: 1,
                to: 0,
            },
            cx,
        );

        assert_eq!(session_ids(&fx, cx), ["s1", "s0"]);
        assert_eq!(
            group_of(
                &fx,
                SessionCursor {
                    project_idx: 0,
                    session_idx: 0
                },
                cx
            ),
            None,
            "dropped among the ungrouped rows, so it is ungrouped"
        );
    }

    /// A retag stores the project's own spelling, not the one the row happened
    /// to carry — `state.json` is hand-editable and sync carries no group, so a
    /// row can hold a spelling allele never interned.
    #[gpui::test]
    fn a_retag_stores_the_projects_spelling(cx: &mut TestAppContext) {
        let mut project = grouped_project(&[Some("review"), Some("linear")]);
        // As a hand-edited file could leave it.
        project.sessions[0].group = Some("REVIEW".to_string());

        let fx = Fixture::with_projects(vec![project], cx);
        fx.dispatch(
            SessionAction::ReorderSession {
                project_idx: 0,
                from: 1,
                to: 0,
            },
            cx,
        );

        assert_eq!(
            group_of(
                &fx,
                SessionCursor {
                    project_idx: 0,
                    session_idx: 0
                },
                cx
            )
            .as_deref(),
            Some("review"),
            "interned to the project's spelling on the way across"
        );
    }

    /// A drop on a group *header* changes the group and nothing else — a header
    /// sits between rows rather than among them, so it implies no position.
    #[gpui::test]
    fn dropping_a_session_on_a_header_regroups_without_moving_it(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(
            vec![grouped_project(&[None, Some("review"), Some("linear")])],
            cx,
        );

        fx.dispatch(
            SessionAction::RegroupSession {
                project_idx: 0,
                session_idx: 0,
                group: Some("linear".to_string()),
            },
            cx,
        );

        assert_eq!(
            session_ids(&fx, cx),
            ["s0", "s1", "s2"],
            "the manual order is untouched"
        );
        assert_eq!(
            group_of(
                &fx,
                SessionCursor {
                    project_idx: 0,
                    session_idx: 0
                },
                cx
            )
            .as_deref(),
            Some("linear")
        );
    }

    /// Dropping a session on the ungrouped block clears its group.
    #[gpui::test]
    fn regrouping_with_no_name_returns_a_session_to_the_ungrouped_block(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);

        fx.dispatch(
            SessionAction::RegroupSession {
                project_idx: 0,
                session_idx: 0,
                group: None,
            },
            cx,
        );

        assert_eq!(
            group_of(
                &fx,
                SessionCursor {
                    project_idx: 0,
                    session_idx: 0
                },
                cx
            ),
            None
        );
    }

    /// A regroup to a name the project has never seen records it, so the
    /// sidebar has a slot to draw the new group in.
    #[gpui::test]
    fn regrouping_to_a_new_name_records_it_in_the_order(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);

        fx.dispatch(
            SessionAction::RegroupSession {
                project_idx: 0,
                session_idx: 0,
                group: Some("linear".to_string()),
            },
            cx,
        );

        let names = fx.update(cx, |state, _w, _cx| {
            state.projects[0].group_order.names().to_vec()
        });
        assert_eq!(names, ["review", "linear"]);
    }

    // ── DEV-937: group order, collapse and rename ──────────────────────

    fn order_slots(fx: &Fixture, cx: &mut TestAppContext) -> Vec<String> {
        fx.update(cx, |state, _w, _cx| {
            state.projects[0]
                .group_order
                .slots()
                .into_iter()
                .map(|slot| match slot {
                    crate::sidebar::groups::GroupSlot::Ungrouped => "·".to_string(),
                    crate::sidebar::groups::GroupSlot::Named(name) => name,
                })
                .collect()
        })
    }

    /// The acceptance criterion: dragging one header onto another reorders the
    /// two groups.
    #[gpui::test]
    fn dragging_a_group_onto_another_reorders_them(cx: &mut TestAppContext) {
        let fx =
            Fixture::with_projects(vec![grouped_project(&[Some("review"), Some("linear")])], cx);

        fx.dispatch(
            SidebarAction::ReorderGroup {
                project_idx: 0,
                from: "linear".to_string(),
                to: "review".to_string(),
            },
            cx,
        );

        assert_eq!(order_slots(&fx, cx), ["·", "linear", "review"]);
    }

    /// Moving the top group up puts it above the ungrouped block — which is how
    /// that block is positioned, having no header of its own to drag.
    #[gpui::test]
    fn moving_the_first_group_up_puts_it_above_the_ungrouped_block(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(
            vec![grouped_project(&[None, Some("review"), Some("linear")])],
            cx,
        );

        fx.dispatch(
            SidebarAction::MoveGroup {
                project_idx: 0,
                name: "review".to_string(),
                direction: crate::sidebar::groups::MoveDirection::Up,
            },
            cx,
        );

        assert_eq!(order_slots(&fx, cx), ["review", "·", "linear"]);
    }

    /// A group that is gone — renamed elsewhere, or never recorded — is a
    /// no-op rather than a move of whatever sits in its slot.
    #[gpui::test]
    fn moving_a_group_the_project_does_not_have_does_nothing(cx: &mut TestAppContext) {
        let fx =
            Fixture::with_projects(vec![grouped_project(&[Some("review"), Some("linear")])], cx);

        fx.dispatch(
            SidebarAction::MoveGroup {
                project_idx: 0,
                name: "gone".to_string(),
                direction: crate::sidebar::groups::MoveDirection::Up,
            },
            cx,
        );

        assert_eq!(order_slots(&fx, cx), ["·", "review", "linear"]);
    }

    #[gpui::test]
    fn toggling_a_group_collapses_then_expands_it(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);
        let collapsed = |cx: &mut TestAppContext| {
            fx.update(cx, |state, _w, _cx| {
                state.projects[0].group_order.is_collapsed("review")
            })
        };

        assert!(!collapsed(cx), "groups start expanded");
        fx.dispatch(
            SidebarAction::ToggleGroupCollapsed {
                project_idx: 0,
                name: "review".to_string(),
            },
            cx,
        );
        assert!(collapsed(cx));
        fx.dispatch(
            SidebarAction::ToggleGroupCollapsed {
                project_idx: 0,
                name: "review".to_string(),
            },
            cx,
        );
        assert!(!collapsed(cx), "and clicking again unfolds it");
    }

    /// The acceptance criterion: renaming a group from its header renames it for
    /// every session in it — including ones the sidebar is not currently showing.
    #[gpui::test]
    fn renaming_a_group_renames_it_for_every_session_in_it(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(
            vec![grouped_project(&[Some("review"), Some("review"), None])],
            cx,
        );

        fx.dispatch(
            SidebarAction::StartGroupRename {
                project_idx: 0,
                name: "review".to_string(),
            },
            cx,
        );
        fx.update(cx, |state, _w, _cx| {
            state
                .group_headers
                .rename
                .as_mut()
                .expect("the rename is open")
                .buffer = "code review".to_string();
        });
        fx.dispatch(SidebarAction::CommitGroupRename, cx);

        let groups = fx.update(cx, |state, _w, _cx| {
            state.projects[0]
                .sessions
                .iter()
                .map(|s| s.group.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            groups,
            [
                Some("code review".to_string()),
                Some("code review".to_string()),
                None
            ]
        );
        assert_eq!(
            order_slots(&fx, cx),
            ["·", "code review"],
            "and the group kept its slot"
        );
    }

    /// A rename onto another group's name would merge two groups. Refused, with
    /// the reason left beside the field so the name can be corrected.
    #[gpui::test]
    fn renaming_a_group_onto_another_is_refused(cx: &mut TestAppContext) {
        let fx =
            Fixture::with_projects(vec![grouped_project(&[Some("review"), Some("linear")])], cx);

        fx.dispatch(
            SidebarAction::StartGroupRename {
                project_idx: 0,
                name: "review".to_string(),
            },
            cx,
        );
        fx.update(cx, |state, _w, _cx| {
            state
                .group_headers
                .rename
                .as_mut()
                .expect("the rename is open")
                .buffer = "LINEAR".to_string();
        });
        fx.dispatch(SidebarAction::CommitGroupRename, cx);

        let refusal = fx.update(cx, |state, _w, _cx| {
            state
                .group_headers
                .rename
                .as_ref()
                .map(|r| r.refusal.clone())
        });
        assert!(
            refusal.flatten().is_some_and(|m| m.contains("linear")),
            "the field stays open and names the group in the way"
        );
        assert_eq!(
            order_slots(&fx, cx),
            ["·", "review", "linear"],
            "and nothing was merged"
        );
    }

    /// Correcting the canonical spelling is a rename, not a collision — it is
    /// the only way out of a spelling whoever typed the name first chose.
    #[gpui::test]
    fn respelling_a_group_in_another_case_is_allowed(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![grouped_project(&[Some("review")])], cx);

        fx.dispatch(
            SidebarAction::StartGroupRename {
                project_idx: 0,
                name: "review".to_string(),
            },
            cx,
        );
        fx.update(cx, |state, _w, _cx| {
            state
                .group_headers
                .rename
                .as_mut()
                .expect("the rename is open")
                .buffer = "Review".to_string();
        });
        fx.dispatch(SidebarAction::CommitGroupRename, cx);

        assert_eq!(order_slots(&fx, cx), ["·", "Review"]);
        assert_eq!(
            group_of(
                &fx,
                SessionCursor {
                    project_idx: 0,
                    session_idx: 0
                },
                cx
            )
            .as_deref(),
            Some("Review")
        );
    }
}
