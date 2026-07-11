//! Session lifecycle operations on AppState.
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 3.
//! Add/resume/close/remove/navigate session methods live here; see
//! ARCHITECTURE.md §2 for the module map and §3.5 for the eventual
//! AppState sub-struct composition.

use gpui::*;
use tracing::{info, warn};

use crate::actions::{
    DrawerAction, OverlayAction, ProjectAction, SessionAction, SessionCursor, SettingsAction,
    SidebarAction,
};
use crate::app_state::AppState;
use crate::session::{OperationError, OperationErrorKind, Session, SessionStatus};
use crate::state::ArchivedSession;
use crate::terminal::{clamp_font_size, TerminalEvent, TerminalView, DEFAULT_FONT_SIZE};
use crate::{
    agents, browser, claude_session_history_exists, clone, config, git, project, settings,
};

impl AppState {
    /// Create a new session inside a project. Runs the APFS clone on a
    /// background task so the UI stays responsive. A "Cloning..." placeholder
    /// appears in the sidebar while the clone is in flight.
    pub(crate) fn add_session_to_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else {
            return;
        };

        // Guard: if the source directory no longer exists (e.g. repo was
        // moved), prompt the user to relocate rather than failing mid-clone.
        if !project.source_path.exists() {
            warn!(
                "Project source path missing: {} — prompting for relocation",
                project.source_path.display()
            );
            self.pending_action = Some(ProjectAction::RelocateProject(project_idx).into());
            cx.notify();
            return;
        }

        // If the working tree has uncommitted changes, prompt the user
        // before creating a session. The user can choose to proceed (the
        // dirty state will be present in the clone) or cancel to clean up.
        if git::is_working_tree_dirty(&project.source_path)
            && self.confirming.dirty_session.is_none()
        {
            self.confirming.dirty_session = Some(project_idx);
            cx.notify();
            return;
        }
        // Clear any prior dirty confirmation (user chose to proceed).
        self.confirming.dirty_session = None;

        let source_path = project.source_path.clone();
        let project_name = project.name.clone();
        let session_count = project.sessions.len() + project.loading_sessions.len() + 1;

        // Pick the agent for this session: allele.json override first,
        // then the global default. Falls through to the first enabled
        // agent with a resolved path. `None` here means "no agent
        // available" — the PTY drops into the user's default shell.
        let project_override =
            config::ProjectConfig::load(&project.source_path).and_then(|c| c.agent);
        let agent = agents::resolve(
            &self.user_settings.agents,
            self.user_settings.default_agent.as_deref(),
            project_override.as_deref(),
            None,
        )
        .cloned();

        let session_id = uuid::Uuid::new_v4().to_string();
        let display_label = match &agent {
            Some(a) => format!("{} {session_count}", a.display_name),
            None => format!("Shell {session_count}"),
        };
        let agent_id = agent.as_ref().map(|a| a.id.clone());

        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let ctx = agents::SpawnCtx {
            session_id: &session_id,
            label: &display_label,
            hooks_settings_path: hooks_path_str.as_deref(),
            has_history: false,
        };
        let command = agent
            .as_ref()
            .and_then(|a| agents::build_command(a, &ctx, false));

        // Add a loading placeholder immediately so the user sees feedback
        project.loading_sessions.push(project::LoadingSession {
            id: session_id.clone(),
            label: display_label.clone(),
            status: if self.user_settings.git_pull_before_new_session {
                "Pulling and cloning…".into()
            } else {
                "Cloning workspace…".into()
            },
        });
        cx.notify();

        // Spawn the clone on a background task, then finish on the main thread
        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        let pull_before_clone = self.user_settings.git_pull_before_new_session;
        let cleanup_paths_for_task = self.user_settings.session_cleanup_paths.clone();
        // Two copies: one moves into the background clonefile closure (where
        // it's used as the short-ID source), the other is captured by the
        // main-thread update_in closure to set Session.id.
        let session_id_for_clone = session_id.clone();
        let session_id_for_session = session_id.clone();
        let display_label_for_task = display_label.clone();
        let agent_id_for_task = agent_id.clone();

        cx.spawn_in(window, async move |this, cx| {
            let (clone_result, pull_error) = cx
                .background_executor()
                .spawn(async move {
                    let pull_error = if pull_before_clone {
                        match git::pull(&source_for_task) {
                            Ok(()) => None,
                            Err(e) => {
                                let msg = format!("{e}");
                                warn!(
                                    "git pull on {} failed before new session: {msg} \
                                     (continuing with clone)",
                                    source_for_task.display()
                                );
                                Some(msg)
                            }
                        }
                    } else {
                        None
                    };
                    let clone = clone::create_session_clone(
                        &source_for_task,
                        &project_name_for_task,
                        &session_id_for_clone,
                        &cleanup_paths_for_task,
                    );
                    (clone, pull_error)
                })
                .await;

            // Back on the main thread with window access
            let _ = this.update_in(cx, move |this: &mut Self, window, cx| {
                // Surface git pull failures as a transient warning banner.
                if let Some(msg) = pull_error {
                    this.pull_warning = Some(msg);
                    cx.notify();
                }

                let clone_path = match clone_result {
                    Ok(p) => {
                        info!("Created APFS clone at: {}", p.display());
                        p
                    }
                    Err(e) => {
                        warn!("Failed to create APFS clone: {e}");
                        source_path.clone()
                    }
                };

                let clone_succeeded = clone_path != source_path;

                // Purge stale runtime files (Overmind/Foreman sockets, server
                // pid files, etc.) that the parent left in the working tree —
                // clonefile(2) faithfully copied them. Must happen before any
                // drawer tab spawns its command.
                if clone_succeeded {
                    clone::cleanup_stale_runtime(
                        &clone_path,
                        &this.user_settings.session_cleanup_paths,
                    );
                }

                // Find the project again (indices may have shifted if user removed projects)
                let Some(project) = this.projects.get_mut(project_idx) else {
                    let _ = clone::delete_clone(&clone_path);
                    return;
                };

                // Remove the loading placeholder
                project.loading_sessions.retain(|l| l.id != session_id);

                // Create the session branch in the clone rooted at HEAD.
                // Only do this when clonefile succeeded — when we fell back
                // to source_path we must NOT mutate canonical's HEAD.
                if clone_succeeded {
                    if let Err(e) = git::create_session_branch(&clone_path, &session_id_for_session)
                    {
                        warn!("create_session_branch failed for {session_id_for_session}: {e}");
                    }

                    // Write marker file for orphan cleanup identification.
                    let marker_path = clone_path.join(".allele-session");
                    if let Err(e) = std::fs::write(&marker_path, &session_id_for_session) {
                        warn!("failed to write .allele-session marker: {e}");
                    }

                    // Exclude the marker from git so auto-commit never
                    // captures it into the session branch.
                    crate::git::exclude_pattern_in_clone(&clone_path, ".allele-session");
                }

                // Create the terminal view with the clone as PWD
                let initial_font_size = this.user_settings.font_size;
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        window,
                        cx,
                        command,
                        Some(clone_path.clone()),
                        initial_font_size,
                    )
                });

                // Subscribe to terminal events
                cx.subscribe(
                    &terminal_view,
                    |this: &mut Self,
                     _tv: Entity<TerminalView>,
                     event: &TerminalEvent,
                     cx: &mut Context<Self>| {
                        match event {
                            TerminalEvent::NewSession => {
                                this.pending_action =
                                    Some(SessionAction::NewSessionInActiveProject.into());
                                cx.notify();
                            }
                            TerminalEvent::CloseSession => {
                                this.pending_action =
                                    Some(SessionAction::CloseActiveSession.into());
                                cx.notify();
                            }
                            TerminalEvent::SwitchSession(target) => {
                                let target = *target;
                                let mut flat_idx = 0;
                                'outer: for (p_idx, project) in this.projects.iter().enumerate() {
                                    for (s_idx, _) in project.sessions.iter().enumerate() {
                                        if flat_idx == target {
                                            this.active = Some(SessionCursor {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            });
                                            this.pending_action =
                                                Some(SessionAction::FocusActive.into());
                                            cx.notify();
                                            break 'outer;
                                        }
                                        flat_idx += 1;
                                    }
                                }
                            }
                            TerminalEvent::PrevSession => {
                                this.navigate_session(-1, cx);
                            }
                            TerminalEvent::NextSession => {
                                this.navigate_session(1, cx);
                            }
                            TerminalEvent::ToggleDrawer => {
                                this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                                cx.notify();
                            }
                            TerminalEvent::ToggleSidebar => {
                                this.pending_action = Some(SidebarAction::ToggleSidebar.into());
                                cx.notify();
                            }
                            TerminalEvent::ToggleRightSidebar => {
                                this.pending_action =
                                    Some(SidebarAction::ToggleRightSidebar.into());
                                cx.notify();
                            }
                            TerminalEvent::OpenScratchPad => {
                                this.pending_action = Some(OverlayAction::OpenScratchPad.into());
                                cx.notify();
                            }
                            TerminalEvent::AdjustFontSize(delta) => {
                                let new_size =
                                    clamp_font_size(this.user_settings.font_size + delta);
                                this.pending_action =
                                    Some(SettingsAction::UpdateFontSize(new_size).into());
                                cx.notify();
                            }
                            TerminalEvent::ResetFontSize => {
                                this.pending_action =
                                    Some(SettingsAction::UpdateFontSize(DEFAULT_FONT_SIZE).into());
                                cx.notify();
                            }
                            TerminalEvent::OpenExternalEditor { path, line_col } => {
                                let cmd = this
                                    .user_settings
                                    .external_editor_command
                                    .as_deref()
                                    .unwrap_or(settings::DEFAULT_EXTERNAL_EDITOR);
                                settings::spawn_external_editor(cmd, path, *line_col);
                            }
                            TerminalEvent::EnterPressed => {
                                this.handle_terminal_enter(&_tv, cx);
                            }
                        }
                    },
                )
                .detach();

                let mut session = Session::new_with_id(
                    session_id_for_session,
                    display_label_for_task,
                    terminal_view,
                )
                .with_clone(clone_path)
                .with_agent_id(agent_id_for_task.clone());
                session.operation_result = Some(if clone_succeeded {
                    "Workspace cloned successfully.".into()
                } else {
                    "Clone failed; session is running in the project source.".into()
                });
                let Some(project) = this.projects.get_mut(project_idx) else {
                    return;
                };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                this.active = Some(cursor);
                this.apply_project_config(cursor, window, cx);
                this.mark_state_dirty();
                cx.notify();
            });

            // Auto-dismiss the pull warning banner after 8 seconds.
            if this
                .read_with(cx, |this, _cx| this.pull_warning.is_some())
                .unwrap_or(false)
            {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(8))
                    .await;
                let _ = this.update_in(cx, |this: &mut Self, _window, cx| {
                    this.pull_warning = None;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Create a new session with custom details (name, branch, agent, prompt).
    ///
    /// This is the "with details" counterpart to `add_session_to_project`.
    /// It accepts optional overrides for label, branch slug, agent, and an
    /// initial prompt to send to the agent after creation.
    pub(crate) fn add_session_to_project_with_details(
        &mut self,
        project_idx: usize,
        custom_label: String,
        custom_branch_slug: Option<String>,
        explicit_agent_id: Option<String>,
        initial_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else {
            return;
        };

        if !project.source_path.exists() {
            self.pending_action = Some(ProjectAction::RelocateProject(project_idx).into());
            cx.notify();
            return;
        }

        let source_path = project.source_path.clone();
        let project_name = project.name.clone();
        let session_count = project.sessions.len() + project.loading_sessions.len() + 1;

        let project_override =
            config::ProjectConfig::load(&project.source_path).and_then(|c| c.agent);
        let agent = agents::resolve(
            &self.user_settings.agents,
            self.user_settings.default_agent.as_deref(),
            project_override.as_deref(),
            explicit_agent_id.as_deref(),
        )
        .cloned();

        let session_id = uuid::Uuid::new_v4().to_string();
        let default_label = match &agent {
            Some(a) => format!("{} {session_count}", a.display_name),
            None => format!("Shell {session_count}"),
        };
        let display_label = if custom_label.trim().is_empty() {
            default_label
        } else {
            custom_label.clone()
        };
        // Skip auto-naming if user provided a custom label or branch slug.
        let skip_auto_naming = !custom_label.trim().is_empty() || custom_branch_slug.is_some();
        let agent_id = agent.as_ref().map(|a| a.id.clone());

        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let ctx = agents::SpawnCtx {
            session_id: &session_id,
            label: &display_label,
            hooks_settings_path: hooks_path_str.as_deref(),
            has_history: false,
        };
        let command = agent
            .as_ref()
            .and_then(|a| agents::build_command(a, &ctx, false));

        project.loading_sessions.push(project::LoadingSession {
            id: session_id.clone(),
            label: display_label.clone(),
            status: if self.user_settings.git_pull_before_new_session {
                "Pulling and cloning…".into()
            } else {
                "Cloning workspace…".into()
            },
        });
        cx.notify();

        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        let session_id_for_clone = session_id.clone();
        let session_id_for_session = session_id.clone();
        let display_label_for_task = display_label.clone();
        let agent_id_for_task = agent_id.clone();
        let pull_before_clone = self.user_settings.git_pull_before_new_session;
        let cleanup_paths_for_task = self.user_settings.session_cleanup_paths.clone();
        let branch_slug = custom_branch_slug;
        // The user picked this branch — auto-naming may relabel the session but
        // must never rename the branch out from under them.
        let branch_locked = branch_slug.is_some();
        let prompt = initial_prompt;
        // Branch resolution (which may fetch from a remote) runs inside the
        // background task below so the network call never blocks the UI.
        let branch_slug_for_clone = branch_slug.clone();
        let session_id_for_branch = session_id.clone();

        cx.spawn_in(window, async move |this, cx| {
            let (clone_result, pull_error) = cx
                .background_executor()
                .spawn(async move {
                    let pull_error = if pull_before_clone {
                        match git::pull(&source_for_task) {
                            Ok(()) => None,
                            Err(e) => {
                                let msg = format!("{e}");
                                warn!(
                                    "git pull on {} failed before new session: {msg} \
                                     (continuing with clone)",
                                    source_for_task.display()
                                );
                                Some(msg)
                            }
                        }
                    } else {
                        None
                    };
                    let clone = clone::create_session_clone(
                        &source_for_task,
                        &project_name_for_task,
                        &session_id_for_clone,
                        &cleanup_paths_for_task,
                    );

                    // Resolve the session branch here (off the UI thread):
                    // check out an existing local/remote branch if the user
                    // named one, otherwise create a fresh session branch.
                    if let Ok(ref clone_path) = clone {
                        if clone_path != &source_for_task {
                            if let Err(e) = git::checkout_or_create_session_branch(
                                clone_path,
                                &session_id_for_branch,
                                branch_slug_for_clone.as_deref(),
                            ) {
                                warn!(
                                    "session branch setup failed for \
                                     {session_id_for_branch}: {e}"
                                );
                            }
                        }
                    }

                    (clone, pull_error)
                })
                .await;

            let _ = this.update_in(cx, move |this: &mut Self, window, cx| {
                if let Some(msg) = pull_error {
                    this.pull_warning = Some(msg);
                    cx.notify();
                }

                let clone_path = match clone_result {
                    Ok(p) => {
                        info!("Created APFS clone at: {}", p.display());
                        p
                    }
                    Err(e) => {
                        warn!("Failed to create APFS clone: {e}");
                        source_path.clone()
                    }
                };

                let clone_succeeded = clone_path != source_path;

                if clone_succeeded {
                    clone::cleanup_stale_runtime(
                        &clone_path,
                        &this.user_settings.session_cleanup_paths,
                    );
                }

                let Some(project) = this.projects.get_mut(project_idx) else {
                    let _ = clone::delete_clone(&clone_path);
                    return;
                };

                project.loading_sessions.retain(|l| l.id != session_id);

                if clone_succeeded {
                    // The session branch (existing branch checkout, or a fresh
                    // branch) was already resolved in the background task above.

                    // Write marker file for orphan cleanup identification.
                    let marker_path = clone_path.join(".allele-session");
                    if let Err(e) = std::fs::write(&marker_path, &session_id_for_session) {
                        warn!("failed to write .allele-session marker: {e}");
                    }

                    crate::git::exclude_pattern_in_clone(&clone_path, ".allele-session");
                }

                let initial_font_size = this.user_settings.font_size;
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        window,
                        cx,
                        command,
                        Some(clone_path.clone()),
                        initial_font_size,
                    )
                });

                cx.subscribe(
                    &terminal_view,
                    |this: &mut Self,
                     _tv: Entity<TerminalView>,
                     event: &TerminalEvent,
                     cx: &mut Context<Self>| {
                        match event {
                            TerminalEvent::NewSession => {
                                this.pending_action =
                                    Some(SessionAction::NewSessionInActiveProject.into());
                                cx.notify();
                            }
                            TerminalEvent::CloseSession => {
                                this.pending_action =
                                    Some(SessionAction::CloseActiveSession.into());
                                cx.notify();
                            }
                            TerminalEvent::SwitchSession(target) => {
                                let target = *target;
                                let mut flat_idx = 0;
                                'outer: for (p_idx, project) in this.projects.iter().enumerate() {
                                    for (s_idx, _) in project.sessions.iter().enumerate() {
                                        if flat_idx == target {
                                            this.active = Some(SessionCursor {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            });
                                            this.pending_action =
                                                Some(SessionAction::FocusActive.into());
                                            cx.notify();
                                            break 'outer;
                                        }
                                        flat_idx += 1;
                                    }
                                }
                            }
                            TerminalEvent::PrevSession => {
                                this.navigate_session(-1, cx);
                            }
                            TerminalEvent::NextSession => {
                                this.navigate_session(1, cx);
                            }
                            TerminalEvent::ToggleDrawer => {
                                this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                                cx.notify();
                            }
                            TerminalEvent::ToggleSidebar => {
                                this.pending_action = Some(SidebarAction::ToggleSidebar.into());
                                cx.notify();
                            }
                            TerminalEvent::ToggleRightSidebar => {
                                this.pending_action =
                                    Some(SidebarAction::ToggleRightSidebar.into());
                                cx.notify();
                            }
                            TerminalEvent::OpenScratchPad => {
                                this.pending_action = Some(OverlayAction::OpenScratchPad.into());
                                cx.notify();
                            }
                            TerminalEvent::AdjustFontSize(delta) => {
                                let new_size =
                                    clamp_font_size(this.user_settings.font_size + delta);
                                this.pending_action =
                                    Some(SettingsAction::UpdateFontSize(new_size).into());
                                cx.notify();
                            }
                            TerminalEvent::ResetFontSize => {
                                this.pending_action =
                                    Some(SettingsAction::UpdateFontSize(DEFAULT_FONT_SIZE).into());
                                cx.notify();
                            }
                            TerminalEvent::OpenExternalEditor { path, line_col } => {
                                let cmd = this
                                    .user_settings
                                    .external_editor_command
                                    .as_deref()
                                    .unwrap_or(settings::DEFAULT_EXTERNAL_EDITOR);
                                settings::spawn_external_editor(cmd, path, *line_col);
                            }
                            TerminalEvent::EnterPressed => {
                                this.handle_terminal_enter(&_tv, cx);
                            }
                        }
                    },
                )
                .detach();

                let mut session = Session::new_with_id(
                    session_id_for_session,
                    display_label_for_task,
                    terminal_view.clone(),
                )
                .with_clone(clone_path)
                .with_agent_id(agent_id_for_task.clone());
                session.operation_result = Some(if clone_succeeded {
                    "Workspace cloned successfully.".into()
                } else {
                    "Clone failed; session is running in the project source.".into()
                });

                if skip_auto_naming {
                    session.auto_naming_fired = true;
                }
                session.branch_locked = branch_locked;

                let Some(project) = this.projects.get_mut(project_idx) else {
                    return;
                };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                this.active = Some(cursor);
                this.apply_project_config(cursor, window, cx);
                this.mark_state_dirty();

                // Send the initial prompt if provided.
                if let Some(ref prompt_text) = prompt {
                    if let Some(terminal) = terminal_view.read(cx).pty() {
                        terminal.write(b"\x1b[200~");
                        terminal.write(prompt_text.as_bytes());
                        terminal.write(b"\x1b[201~");
                    }
                    let tv_weak = terminal_view.downgrade();
                    cx.spawn(async move |_this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(80))
                            .await;
                        let _ = cx.update(|cx| {
                            if let Some(tv) = tv_weak.upgrade() {
                                if let Some(terminal) = tv.read(cx).pty() {
                                    terminal.write(b"\r");
                                }
                            }
                        });
                    })
                    .detach();
                }

                cx.notify();
            });

            // Auto-dismiss the pull warning banner after 8 seconds.
            if this
                .read_with(cx, |this, _cx| this.pull_warning.is_some())
                .unwrap_or(false)
            {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(8))
                    .await;
                let _ = this.update_in(cx, |this: &mut Self, _window, cx| {
                    this.pull_warning = None;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Called when the user presses Enter in a terminal. If the owning
    /// session is in `AwaitingInput`, optimistically transition it to
    /// `Running` so the attention bar clears immediately rather than
    /// lingering until the next hook event (PostToolUse).
    pub(crate) fn handle_terminal_enter(
        &mut self,
        tv: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let mut matched_cursor: Option<SessionCursor> = None;
        for (p_idx, project) in self.projects.iter_mut().enumerate() {
            for (s_idx, session) in project.sessions.iter_mut().enumerate() {
                if session.terminal_view.as_ref() == Some(tv)
                    && session.status == SessionStatus::AwaitingInput
                {
                    session.set_status(SessionStatus::Running);
                    session.attention_context = None;
                    matched_cursor = Some(SessionCursor {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    });
                    break;
                }
            }
            if matched_cursor.is_some() {
                break;
            }
        }

        if let Some(cursor) = matched_cursor {
            if self.rich.cursor == Some(cursor) {
                if let Some(view) = self.rich.view.as_ref().cloned() {
                    view.update(cx, |rv, cx| {
                        rv.clear_permission_request(cx);
                    });
                }
            }
            cx.notify();
        }
    }

    /// Close a session without deleting its clone.
    ///
    /// The PTY is killed (dropping the terminal_view entity triggers
    /// `PtyTerminal::drop` → `Msg::Shutdown`), the clone stays on disk,
    /// the session stays in `state.json` with status `Suspended`, and the
    /// sidebar row stays visible with a ⏸ icon. A later click on that row
    /// cold-resumes via `claude --resume <id>`.
    pub(crate) fn close_session_keep_clone(
        &mut self,
        cursor: SessionCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(cursor.project_idx) else {
            return;
        };
        let Some(session) = project.sessions.get_mut(cursor.session_idx) else {
            return;
        };

        // Drop the terminal_view and drawer — Drop impl on PtyTerminal sends
        // Msg::Shutdown, killing the subprocesses. The clone on disk is untouched.
        session.terminal_view = None;
        // Drop the drawer PTYs but preserve the names so the next open
        // restores the same tab layout (matches the rehydration path).
        let names: Vec<String> = session.drawer_tabs.iter().map(|t| t.name.clone()).collect();
        session.drawer_tabs.clear();
        session.pending_drawer_tab_names = names;
        session.drawer_visible = false;
        session.set_status(SessionStatus::Suspended);
        session.last_active = std::time::SystemTime::now();

        // If this was the active session, clear the active cursor — the main
        // area will show the "No active session" placeholder until the user
        // clicks something else.
        if self.active == Some(cursor) {
            self.active = None;
        }

        self.mark_state_dirty();
        cx.notify();
    }

    /// Cycle the active session pointer across all non-Suspended sessions
    /// in the flat order they appear in the sidebar. `delta = -1` = previous,
    /// `delta = 1` = next. Wraps at both ends. Suspended sessions are
    /// deliberately skipped — quick-flicking shouldn't auto-spawn resumed
    /// Claude processes; the user clicks the ⏸ row explicitly to resume.
    pub(crate) fn navigate_session(&mut self, delta: i32, cx: &mut Context<Self>) {
        // Build the flat list of (project_idx, session_idx) for every
        // attached (non-Suspended) session. This is the nav surface.
        let flat: Vec<SessionCursor> = self
            .projects
            .iter()
            .enumerate()
            .flat_map(|(p_idx, project)| {
                project
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.status != SessionStatus::Suspended)
                    .map(move |(s_idx, _)| SessionCursor {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    })
            })
            .collect();

        if flat.is_empty() {
            return;
        }

        // Find the active cursor's position in the flat list. If the current
        // active is None or points at a Suspended session (not in `flat`),
        // treat it as an implicit position before index 0 when moving forward,
        // and after the last index when moving backward.
        let current_pos = self
            .active
            .and_then(|active| flat.iter().position(|c| *c == active));

        let len = flat.len() as i32;
        let new_pos = match current_pos {
            Some(pos) => (pos as i32 + delta).rem_euclid(len) as usize,
            None if delta >= 0 => 0,
            None => (len - 1) as usize,
        };

        self.active = Some(flat[new_pos]);
        self.pending_action = Some(SessionAction::FocusActive.into());
        cx.notify();
    }

    /// Resume a Suspended session by spawning a fresh PTY with
    /// `claude --resume <id>` inside the stored clone_path.
    ///
    /// The session retains its original `id` — Claude picks up the
    /// conversation from its jsonl history.
    pub(crate) fn resume_session(
        &mut self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(cursor.project_idx) else {
            return;
        };
        let Some(session) = project.sessions.get(cursor.session_idx) else {
            return;
        };
        // A pulled session records a clone_path rebased to this Mac's workspace
        // root that was never actually created here — so a missing directory is
        // just as much "no workspace" as a `None` path. Treat both the same:
        // rebuild the workspace from the project + synced transcript, then this
        // resume runs again for real.
        let clone_path = match session.clone_path.clone() {
            Some(path) if path.exists() => path,
            other => {
                if self.user_settings.sync.is_configured() {
                    self.materialize_pulled_session(cursor, window, cx);
                } else {
                    warn!(
                        "Cannot resume session {} — no workspace on this Mac ({}) and \
                         sync isn't configured",
                        session.id,
                        other
                            .as_deref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "none".to_string()),
                    );
                    self.sync_notice = Some(
                        "This session was pulled from another Mac. Configure Settings → Sync \
                         so its workspace can be rebuilt here."
                            .to_string(),
                    );
                    // DEV-27: also surface an actionable per-row failure so the
                    // session shows why it can't resume, not just the global notice.
                    if let Some(session) = self
                        .projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        session.operation_error = Some(OperationError {
                            kind: OperationErrorKind::Resume,
                            message:
                                "Workspace is unavailable. Restore it or discard this session."
                                    .into(),
                        });
                    }
                    cx.notify();
                }
                return;
            }
        };

        // Resume the *current* Claude conversation. For a session that was
        // `/clear`ed, this is the rotated id (persisted on the session), not
        // the original workspace `id` — resuming the latter would replay the
        // pre-clear transcript.
        let session_id = session.claude_session_id().to_string();
        let label = session.label.clone();
        let stored_agent_id = session.agent_id.clone();

        // Resolve the agent. Prefer the session's stored agent_id so a
        // resume always uses whatever spawned the session originally,
        // even if the user has since changed the global default.
        // Falls back to allele.json → global default → first enabled.
        let project_override =
            config::ProjectConfig::load(&project.source_path).and_then(|c| c.agent);
        let agent = agents::resolve(
            &self.user_settings.agents,
            self.user_settings.default_agent.as_deref(),
            project_override.as_deref(),
            stored_agent_id.as_deref(),
        )
        .cloned();

        // Only adapters that understand session ids care about history —
        // for claude this gates `--resume` vs `--session-id`.
        let has_history = claude_session_history_exists(&session_id);
        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let ctx = agents::SpawnCtx {
            session_id: &session_id,
            label: &label,
            hooks_settings_path: hooks_path_str.as_deref(),
            has_history,
        };
        let command = agent
            .as_ref()
            .and_then(|a| agents::build_command(a, &ctx, true));

        // Build the new TerminalView on the main thread with window access.
        let initial_font_size = self.user_settings.font_size;
        let terminal_view = cx.new(|cx| {
            TerminalView::new(
                window,
                cx,
                command,
                Some(clone_path.clone()),
                initial_font_size,
            )
        });

        // Subscribe to terminal events so the resumed session wires up the
        // same shortcut actions (NewSession, CloseSession, SwitchSession)
        // as freshly-created ones.
        cx.subscribe(
            &terminal_view,
            |this: &mut Self,
             _tv: Entity<TerminalView>,
             event: &TerminalEvent,
             cx: &mut Context<Self>| {
                match event {
                    TerminalEvent::NewSession => {
                        this.pending_action = Some(SessionAction::NewSessionInActiveProject.into());
                        cx.notify();
                    }
                    TerminalEvent::CloseSession => {
                        this.pending_action = Some(SessionAction::CloseActiveSession.into());
                        cx.notify();
                    }
                    TerminalEvent::SwitchSession(target) => {
                        // Mirror the fresh-spawn handler so Cmd+1..9 also works
                        // from resumed sessions.
                        let target = *target;
                        let mut flat_idx = 0;
                        'outer: for (p_idx, project) in this.projects.iter().enumerate() {
                            for (s_idx, _) in project.sessions.iter().enumerate() {
                                if flat_idx == target {
                                    this.active = Some(SessionCursor {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    });
                                    this.pending_action = Some(SessionAction::FocusActive.into());
                                    cx.notify();
                                    break 'outer;
                                }
                                flat_idx += 1;
                            }
                        }
                    }
                    TerminalEvent::PrevSession => {
                        this.navigate_session(-1, cx);
                    }
                    TerminalEvent::NextSession => {
                        this.navigate_session(1, cx);
                    }
                    TerminalEvent::ToggleDrawer => {
                        this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                        cx.notify();
                    }
                    TerminalEvent::ToggleSidebar => {
                        this.pending_action = Some(SidebarAction::ToggleSidebar.into());
                        cx.notify();
                    }
                    TerminalEvent::ToggleRightSidebar => {
                        this.pending_action = Some(SidebarAction::ToggleRightSidebar.into());
                        cx.notify();
                    }
                    TerminalEvent::OpenScratchPad => {
                        this.pending_action = Some(OverlayAction::OpenScratchPad.into());
                        cx.notify();
                    }
                    TerminalEvent::AdjustFontSize(delta) => {
                        let new_size = clamp_font_size(this.user_settings.font_size + delta);
                        this.pending_action = Some(SettingsAction::UpdateFontSize(new_size).into());
                        cx.notify();
                    }
                    TerminalEvent::ResetFontSize => {
                        this.pending_action =
                            Some(SettingsAction::UpdateFontSize(DEFAULT_FONT_SIZE).into());
                        cx.notify();
                    }
                    TerminalEvent::OpenExternalEditor { path, line_col } => {
                        let cmd = this
                            .user_settings
                            .external_editor_command
                            .as_deref()
                            .unwrap_or(settings::DEFAULT_EXTERNAL_EDITOR);
                        settings::spawn_external_editor(cmd, path, *line_col);
                    }
                    TerminalEvent::EnterPressed => {
                        this.handle_terminal_enter(&_tv, cx);
                    }
                }
            },
        )
        .detach();

        let resolved_agent_id = agent.as_ref().map(|a| a.id.clone());

        // Attach the new PTY to the existing session entry.
        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            session.terminal_view = Some(terminal_view);
            session.operation_error = None;
            session.set_status(SessionStatus::Running);
            session.last_active = std::time::SystemTime::now();
            // Pin the resolved agent so subsequent resumes pick up the
            // same adapter even if the global default changes. Leaves a
            // previously-stored id alone when nothing could be resolved.
            if resolved_agent_id.is_some() {
                session.agent_id = resolved_agent_id;
            }
            // Grace window: if the PTY exits in the next 3s, the exit
            // watcher reverts to Suspended instead of flipping to Done.
            // Protects against `claude --resume` exiting immediately.
            session.resuming_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
            self.active = Some(cursor);
            self.pending_action = Some(SessionAction::FocusActive.into());
        }

        self.apply_project_config(cursor, window, cx);
        self.mark_state_dirty();
        cx.notify();
    }

    /// Discard a session — kill the PTY, delete the APFS clone, remove from
    /// the sidebar, and drop the corresponding entry from `state.json`.
    ///
    /// This is the *destructive* path, reached only through the explicit
    /// Discard action with confirmation. The plain Close action uses
    /// `close_session_keep_clone` instead.
    pub(crate) fn remove_session(
        &mut self,
        cursor: SessionCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(cursor.project_idx) else {
            return;
        };
        if cursor.session_idx >= project.sessions.len() {
            return;
        }

        // Pull the session out of the list immediately
        let removed = project.sessions.remove(cursor.session_idx);
        let clone_path = removed.clone_path.clone();
        let removed_label = removed.label.clone();
        let already_merged = removed.merged;
        let removed_session_id = removed.id.clone();
        let removed_browser_tab_id = removed.browser_tab_id;
        // Captured before drop(removed) / end of &mut project borrow.
        let canonical_for_task = project.source_path.clone();
        let session_id_for_task = removed.id.clone();

        // Preserve the session's metadata in the archive list so the
        // sidebar archive browser can show a human-readable label —
        // but skip this if the session was already merged (work is in canonical).
        if !already_merged {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            project.archives.push(ArchivedSession {
                id: removed.id.clone(),
                project_id: project.id.clone(),
                label: removed_label.clone(),
                archived_at: now,
                merge_error: None,
            });
        }

        // Register Chrome-tab cleanup as a hook on the PTY: when the
        // terminal is dropped below, the tab closes as part of the same
        // teardown sequence (alongside SIGTERM to any dev servers).
        // Suspended sessions have no terminal_view, so fall back to the
        // direct call. Integration-disabled case: still no-op.
        let close_tab = self
            .user_settings
            .browser_integration_enabled
            .then_some(removed_browser_tab_id)
            .flatten();
        if let Some(id) = close_tab {
            match removed.terminal_view.as_ref() {
                Some(tv) => tv.update(cx, |view, _| {
                    view.on_close(move || {
                        let _ = browser::close_tab(id);
                    });
                }),
                None => {
                    let _ = browser::close_tab(id);
                }
            }
        }

        // Resolve the project-declared shutdown command (if any) up front:
        // a cheap config load plus string substitution, done here while
        // `project` and `removed` are still in scope for the settings and
        // port lookups. The command *itself* is run off-thread in the
        // archive pipeline below — running it in-line would block GPUI's
        // render loop, giving the user a spinning beach ball whenever a
        // shutdown hook is slow (dev-server teardown, `docker compose
        // down`, proxy-route cleanup, …).
        let shutdown_cmd = clone_path.as_ref().and_then(|clone_path| {
            let cfg = config::ProjectConfig::load(clone_path)
                .or_else(|| config::ProjectConfig::from_settings(&project.settings))?;
            cfg.shutdown
                .as_ref()
                .map(|s| config::resolve_script_command(s, &project.name))
                .map(|s| config::substitute(&s, removed.allocated_port, clone_path))
                .filter(|s| !s.trim().is_empty())
        });

        // Drop the Session — this frees the terminal_view entity (if any),
        // which fires cleanup hooks then kills the PTY process group via
        // the Drop impl on PtyTerminal. Suspended sessions have
        // `terminal_view = None` so there's no PTY to kill; only the
        // clone needs cleanup.
        drop(removed);
        let _ = removed_session_id; // reserved for future use

        // Show an "Archiving…" placeholder if there's a clone to clean up
        let placeholder_id = uuid::Uuid::new_v4().to_string();
        if clone_path.is_some() {
            project.loading_sessions.push(project::LoadingSession {
                id: placeholder_id.clone(),
                label: format!("{removed_label} (archiving)"),
                status: "Archiving workspace…".into(),
            });
        }

        // If the removed session was the active one, clear active selection
        // (so the main content area shows the empty state immediately).
        if let Some(active) = self.active {
            if active == cursor {
                // Try to pick another session in the same project first
                let project = &self.projects[cursor.project_idx];
                self.active = if !project.sessions.is_empty() {
                    let new_session_idx = cursor.session_idx.min(project.sessions.len() - 1);
                    Some(SessionCursor {
                        project_idx: cursor.project_idx,
                        session_idx: new_session_idx,
                    })
                } else {
                    // Fall back to any session in any project
                    self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                        if !p.sessions.is_empty() {
                            Some(SessionCursor {
                                project_idx: p_idx,
                                session_idx: 0,
                            })
                        } else {
                            None
                        }
                    })
                };
            } else if active.project_idx == cursor.project_idx
                && active.session_idx > cursor.session_idx
            {
                // Active session in same project shifted down by one
                self.active = Some(SessionCursor {
                    project_idx: active.project_idx,
                    session_idx: active.session_idx - 1,
                });
            }
        }

        // Persist the updated session list now that the entry is gone.
        self.mark_state_dirty();
        cx.notify();

        // Spawn the archive-then-delete pipeline on a background task
        if let Some(clone_path) = clone_path {
            let project_idx = cursor.project_idx;
            let placeholder_id_for_task = placeholder_id.clone();
            cx.spawn(async move |this, cx| {
                let delete_result = cx
                    .background_executor()
                    .spawn(async move {
                        // Run the project-declared shutdown command first —
                        // it needs the clone to still exist as its working
                        // directory, and must run before the clone is
                        // archived/trashed. Off the UI thread, so a slow
                        // hook can't freeze the app. Failure is logged and
                        // teardown continues so a broken hook can't strand
                        // the clone on disk.
                        if let Some(cmd) = shutdown_cmd {
                            match std::process::Command::new("sh")
                                .arg("-c")
                                .arg(&cmd)
                                .current_dir(&clone_path)
                                .status()
                            {
                                Ok(s) if !s.success() => {
                                    warn!("allele: shutdown command exited with {s} — continuing");
                                }
                                Err(e) => {
                                    warn!(
                                        "allele: failed to run shutdown command: {e} — continuing"
                                    );
                                }
                                _ => {}
                            }
                        }

                        // Degenerate case: if the session's "clone path"
                        // is canonical itself (Phase C fallback when the
                        // clonefile syscall failed), skip the archive
                        // pipeline — no session branch exists, the fetch
                        // would be a no-op self-fetch, and trash_clone
                        // will bail on the workspace-dir safety check.
                        if clone_path == canonical_for_task {
                            return clone::delete_clone(&clone_path);
                        }
                        // Archive the session's work into canonical
                        // before the clone is trashed. Order is
                        // load-bearing — archive_session must run while
                        // the clone still exists.
                        if let Err(e) = git::archive_session(
                            &canonical_for_task,
                            &clone_path,
                            &session_id_for_task,
                        ) {
                            warn!("archive_session failed for {session_id_for_task}: {e}");
                        }
                        clone::trash_clone(&clone_path).map(|_| ())
                    })
                    .await;

                if let Err(e) = delete_result {
                    warn!("Failed to delete clone: {e}");
                }

                // Remove the placeholder on the main thread
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if let Some(project) = this.projects.get_mut(project_idx) {
                        project
                            .loading_sessions
                            .retain(|l| l.id != placeholder_id_for_task);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Push a session's bundle (metadata) up to the configured sync store.
    /// Gathers the session/project data on the main thread, then does the git
    /// precondition check + encrypted upload off-thread via the sync bridge.
    pub(crate) fn sync_up_session(
        &mut self,
        cursor: crate::actions::SessionCursor,
        cx: &mut Context<Self>,
    ) {
        // Gather everything the background task needs as owned values, so no
        // borrow of `self.projects` outlives the mutable settings calls below.
        let (persisted, project_name, source_path, clone_path, label) = {
            let Some(project) = self.projects.get(cursor.project_idx) else {
                return;
            };
            let Some(session) = project.sessions.get(cursor.session_idx) else {
                return;
            };
            (
                crate::state::PersistedSession::from_session(session, &project.id),
                project.name.clone(),
                project.source_path.clone(),
                session.clone_path.clone(),
                session.label.clone(),
            )
        };

        let device_id = self.user_settings.sync.ensure_device_id();
        self.mark_settings_dirty();
        let settings = self.user_settings.sync.clone();
        if !settings.is_configured() {
            self.sync_notice = Some("Configure sync in Settings → Sync first.".to_string());
            cx.notify();
            return;
        }

        self.sync_notice = Some(format!("Syncing \u{201c}{label}\u{201d} up\u{2026}"));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    sync_up_blocking(
                        &settings,
                        &persisted,
                        &project_name,
                        &source_path,
                        clone_path.as_deref(),
                        &device_id,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.sync_notice = Some(match outcome {
                    Ok(rev) => format!("Synced \u{201c}{label}\u{201d} up (revision {rev})."),
                    Err(e) => format!("Sync failed: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the remote-session browser: list the bundles in the configured
    /// store off-thread, then present them in a picker overlay.
    pub(crate) fn open_remote_browser(&mut self, cx: &mut Context<Self>) {
        let device_id = self.user_settings.sync.ensure_device_id();
        self.mark_settings_dirty();
        let _ = device_id;
        let settings = self.user_settings.sync.clone();
        if !settings.is_configured() {
            self.sync_notice = Some("Configure sync in Settings → Sync first.".to_string());
            cx.notify();
            return;
        }

        self.sync_notice = Some("Loading remote sessions\u{2026}".to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { list_remote_blocking(&settings) })
                .await;
            let _ = this.update(cx, |this, cx| match listed {
                Ok(sessions) => {
                    this.sync_notice = None;
                    let entity =
                        cx.new(|cx| crate::remote_browser::RemoteBrowser::new(cx, sessions));
                    cx.subscribe(&entity, Self::on_remote_browser_event)
                        .detach();
                    this.remote_browser = Some(entity);
                    cx.notify();
                }
                Err(e) => {
                    this.sync_notice = Some(format!("Couldn't load remote sessions: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Bridge the browser's `Pull` / `Close` events onto AppState.
    fn on_remote_browser_event(
        this: &mut Self,
        _browser: Entity<crate::remote_browser::RemoteBrowser>,
        event: &crate::remote_browser::RemoteBrowserEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::remote_browser::RemoteBrowserEvent::Pull { session_id } => {
                this.pull_remote_session(session_id.clone(), cx);
            }
            crate::remote_browser::RemoteBrowserEvent::Close => {
                this.remote_browser = None;
                cx.notify();
            }
        }
    }

    /// Pull a chosen remote session onto this machine: fetch its bundle, resolve
    /// its project against local projects (the sync gate), and apply it. A first
    /// pull inserts an inert `Suspended` row (cold-resumes on click). A pull of a
    /// *newer* revision of a session already here replaces the local row and
    /// drops its stale clone, so the click re-materializes it at the new
    /// revision — the round-trip (A → B → A) needs no manual discard. A same-or-
    /// older revision is left untouched.
    pub(crate) fn pull_remote_session(&mut self, session_id: String, cx: &mut Context<Self>) {
        let settings = self.user_settings.sync.clone();
        if !settings.is_configured() {
            self.sync_notice = Some("Configure sync in Settings → Sync first.".to_string());
            cx.notify();
            return;
        }

        let projects: Vec<(String, String, std::path::PathBuf)> = self
            .projects
            .iter()
            .map(|p| (p.id.clone(), p.name.clone(), p.source_path.clone()))
            .collect();

        self.sync_notice = Some("Pulling session\u{2026}".to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { pull_blocking(&settings, projects, &session_id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(PullOutcome::Ready(session, revision)) => {
                        this.apply_pulled_session(*session, revision);
                    }
                    Ok(PullOutcome::ProjectMissing(name)) => {
                        this.sync_notice = Some(format!(
                            "Add the project \u{201c}{name}\u{201d} on this Mac first."
                        ));
                    }
                    Ok(PullOutcome::Vanished) => {
                        this.sync_notice =
                            Some("That session is no longer in the store.".to_string());
                    }
                    Err(e) => {
                        this.sync_notice = Some(format!("Pull failed: {e}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a resolved pulled session: insert it (first pull), replace it and
    /// drop the stale clone (newer revision), or leave it (same/older). Records
    /// the pulled revision as this device's base in the ledger on apply.
    fn apply_pulled_session(&mut self, session: crate::state::PersistedSession, revision: u64) {
        let id = session.id.clone();
        let label = session.label.clone();

        let existing = self.projects.iter().enumerate().find_map(|(pi, p)| {
            p.sessions
                .iter()
                .position(|s| s.id == id)
                .map(|si| (pi, si))
        });

        let mut ledger = crate::sync::ledger::SyncLedger::load();
        let base = ledger.base_revision(&id).unwrap_or(0);

        match existing {
            Some(_) if revision <= base => {
                self.sync_notice = Some(format!(
                    "\u{201c}{label}\u{201d} is already up to date (revision {base})."
                ));
                return;
            }
            Some((pi, si)) => {
                // Newer revision — replace in place and drop the stale clone so
                // the next click re-materializes the workspace at this revision.
                let old_clone = self.projects[pi].sessions[si].clone_path.clone();
                let source = self.projects[pi].source_path.clone();
                if let Some(clone_path) = old_clone {
                    if clone_path.exists() && clone_path != source {
                        let _ = clone::delete_clone(&clone_path);
                    }
                }
                self.projects[pi].sessions[si] = session_from_persisted(&session);
                self.sync_notice = Some(format!(
                    "Updated \u{201c}{label}\u{201d} to revision {revision} — open it to load the work."
                ));
            }
            None => {
                self.insert_pulled_session(session);
                self.sync_notice = Some(format!("Pulled \u{201c}{label}\u{201d}."));
            }
        }

        ledger.record_synced(&id, revision);
        let _ = ledger.save();
        self.mark_state_dirty();
        self.remote_browser = None;
    }

    /// Materialize a pulled `PersistedSession` into live state as a `Suspended`
    /// row (mirrors the startup rehydration path). No-op if its owning project
    /// isn't loaded — the sync gate should have blocked that earlier.
    fn insert_pulled_session(&mut self, persisted: crate::state::PersistedSession) {
        let Some(project) = self
            .projects
            .iter_mut()
            .find(|p| p.id == persisted.project_id)
        else {
            warn!(
                "Pulled session {} has no local project {}",
                persisted.id, persisted.project_id
            );
            return;
        };
        project.sessions.push(session_from_persisted(&persisted));
    }

    /// Rebuild a pulled session's workspace on this Mac, then resume it. Clones
    /// the project on the session's branch and restores the synced transcript
    /// off-thread, sets the session's `clone_path`, and hands back to
    /// [`AppState::resume_session`] — which now finds a workspace and proceeds.
    pub(crate) fn materialize_pulled_session(
        &mut self,
        cursor: crate::actions::SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (session_id, conversation_id, branch_name, project_source, project_name) = {
            let Some(project) = self.projects.get(cursor.project_idx) else {
                return;
            };
            let Some(session) = project.sessions.get(cursor.session_idx) else {
                return;
            };
            (
                session.id.clone(),
                session.claude_session_id().to_string(),
                session.branch_name.clone(),
                project.source_path.clone(),
                project.name.clone(),
            )
        };
        let settings = self.user_settings.sync.clone();
        let cleanup_paths = self.user_settings.session_cleanup_paths.clone();

        self.sync_notice = Some("Setting up the pulled session\u{2026}".to_string());
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    materialize_blocking(
                        &settings,
                        &session_id,
                        &conversation_id,
                        branch_name.as_deref(),
                        &project_source,
                        &project_name,
                        &cleanup_paths,
                    )
                })
                .await;
            let _ = this.update_in(cx, move |this: &mut Self, window, cx| match result {
                Ok(clone_path) => {
                    if let Some(session) = this
                        .projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        session.clone_path = Some(clone_path);
                    }
                    this.mark_state_dirty();
                    this.sync_notice = None;
                    // The workspace exists now — resume for real.
                    this.resume_session(cursor, window, cx);
                }
                Err(e) => {
                    this.sync_notice = Some(format!("Couldn't set up the pulled session: {e}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

/// Build a `Suspended` live [`Session`] from a pulled `PersistedSession`,
/// mirroring the startup rehydration path. Shared by first-pull insert and
/// newer-revision replace.
fn session_from_persisted(persisted: &crate::state::PersistedSession) -> Session {
    let mut session = Session::suspended_from_persisted(
        persisted.id.clone(),
        persisted.label.clone(),
        persisted.started_at,
        persisted.last_active,
        std::time::Duration::from_secs(persisted.active_runtime_secs),
        persisted.clone_path.clone(),
        persisted.merged,
    )
    .with_drawer_tabs(
        persisted.drawer_tab_names.clone(),
        persisted.drawer_active_tab,
    )
    .with_browser(persisted.browser_tab_id, persisted.browser_last_url.clone())
    .with_agent_id(persisted.agent_id.clone())
    .with_claude_session_id(persisted.claude_session_id.clone());
    session.pinned = persisted.pinned;
    session.comment = persisted.comment.clone();
    session.branch_name = persisted.branch_name.clone();
    session.merge_strategy_override = persisted.merge_strategy_override;
    session.branch_locked = persisted.branch_locked;
    session
}

/// Owned result of a background pull, handed back to the main thread.
enum PullOutcome {
    /// Resolved to a local project; ready to insert as this session.
    Ready(Box<crate::state::PersistedSession>, u64),
    /// The bundle's project isn't present locally — the sync gate blocked it.
    ProjectMissing(String),
    /// The bundle disappeared from the store between listing and pulling.
    Vanished,
}

/// Background worker: list every remote session header from the store.
fn list_remote_blocking(
    settings: &crate::settings::SyncSettings,
) -> anyhow::Result<Vec<crate::sync::pull::RemoteSession>> {
    let store = crate::sync::config::build_store_from_settings(settings)?;
    crate::sync::rt::block_on(crate::sync::pull::list_remote_sessions(store.as_ref()))
}

/// Background worker: fetch a bundle, resolve it against local projects, and —
/// on a match — record the pulled revision in the ledger. The live-state insert
/// happens back on the main thread.
fn pull_blocking(
    settings: &crate::settings::SyncSettings,
    projects: Vec<(String, String, std::path::PathBuf)>,
    session_id: &str,
) -> anyhow::Result<PullOutcome> {
    let store = crate::sync::config::build_store_from_settings(settings)?;
    let meta = match crate::sync::rt::block_on(crate::sync::pull::fetch_bundle(
        store.as_ref(),
        session_id,
    ))? {
        Some(meta) => meta,
        None => return Ok(PullOutcome::Vanished),
    };

    let idents: Vec<(String, crate::sync::meta::ProjectIdentity)> = projects
        .into_iter()
        .map(|(pid, name, path)| (pid, crate::sync::identity::project_identity(&name, &path)))
        .collect();
    let candidates: Vec<crate::sync::identity::Candidate> = idents
        .iter()
        .map(|(pid, ident)| crate::sync::identity::Candidate {
            project_id: pid,
            identity: ident,
        })
        .collect();

    match crate::sync::pull::resolve_pull(&meta, &candidates) {
        crate::sync::pull::PullResolution::Ready(session) => {
            // The ledger is recorded on the main thread once we've decided
            // whether this is a first pull or a newer-revision replace — the
            // comparison needs the current base before it's overwritten.
            Ok(PullOutcome::Ready(Box::new(session), meta.sync.revision))
        }
        crate::sync::pull::PullResolution::ProjectMissing { name } => {
            Ok(PullOutcome::ProjectMissing(name))
        }
    }
}

/// Blocking worker for [`AppState::materialize_pulled_session`]: create the
/// APFS clone, check out the session's branch (fetching it from the remote if
/// needed), and restore the synced Claude transcript into the clone. Returns
/// the new workspace path.
fn materialize_blocking(
    settings: &crate::settings::SyncSettings,
    session_id: &str,
    conversation_id: &str,
    branch_name: Option<&str>,
    project_source: &std::path::Path,
    project_name: &str,
    cleanup_paths: &[String],
) -> anyhow::Result<std::path::PathBuf> {
    // 1. Copy-on-write clone of the project source.
    let clone_path =
        clone::create_session_clone(project_source, project_name, session_id, cleanup_paths)?;
    if clone_path == project_source {
        anyhow::bail!("workspace clone did not produce a separate directory");
    }
    clone::cleanup_stale_runtime(&clone_path, cleanup_paths);

    // 2. Check out the session's branch at exactly what was pushed. Reset the
    //    clone's local branch to origin's tip so we get the *synced* commit, not
    //    a stale local copy of the same branch name (which bites the round-trip
    //    back to a machine that already has this branch at an older commit). If
    //    the branch isn't on origin (an ephemeral one never pushed), fall back
    //    to creating/checking-out a fresh session branch.
    if let Some(branch) = branch_name.map(str::trim).filter(|b| !b.is_empty()) {
        let on_remote =
            git::fetch_and_reset_to_remote_branch(&clone_path, "origin", branch).unwrap_or(false);
        if !on_remote {
            if let Err(e) =
                git::checkout_or_create_session_branch(&clone_path, session_id, Some(branch))
            {
                warn!("pulled session {session_id}: branch '{branch}' checkout failed: {e}");
            }
        }
    }

    // Marker file for orphan cleanup, excluded from git (mirrors new sessions).
    let _ = std::fs::write(clone_path.join(".allele-session"), session_id);
    crate::git::exclude_pattern_in_clone(&clone_path, ".allele-session");

    // 3. Restore the synced transcript so `claude --resume` replays the
    //    conversation. Best-effort — a metadata-only bundle just yields a
    //    fresh Claude session in the materialized workspace.
    let store = crate::sync::config::build_store_from_settings(settings)?;
    if let Some(bytes) = crate::sync::rt::block_on(crate::sync::pull::fetch_transcript(
        store.as_ref(),
        session_id,
    ))? {
        if let Some(transcript_path) =
            crate::transcript::expected_session_jsonl(&clone_path, conversation_id)
        {
            if let Some(parent) = transcript_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&transcript_path, bytes)?;
        }
    }

    Ok(clone_path)
}

/// Blocking worker for [`AppState::sync_up_session`] — runs on a background
/// thread: enforce the git precondition, build the encrypted store, and push
/// the bundle. Returns the pushed revision.
fn sync_up_blocking(
    settings: &crate::settings::SyncSettings,
    session: &crate::state::PersistedSession,
    project_name: &str,
    source_path: &std::path::Path,
    clone_path: Option<&std::path::Path>,
    device_id: &str,
) -> anyhow::Result<u64> {
    // The bundle carries the transcript + metadata but not the code; refuse to
    // push a session whose branch isn't fully on the remote (design §2.5).
    if let Some(clone) = clone_path {
        let readiness = crate::sync::push::check_push_ready(clone);
        if let Some(warning) = readiness.warning() {
            anyhow::bail!("{warning}");
        }
    }

    // Sync the branch the session is ACTUALLY on right now. `branch_name` is set
    // once at creation and goes stale the moment the user `git checkout`s a
    // different branch inside the session — but the readiness check above
    // validated the *live* branch, and the other Mac rebuilds the workspace from
    // whatever we record here, so the two must agree. On a detached HEAD (no
    // branch) we keep the recorded name.
    let mut synced = session.clone();
    if let Some(live_branch) = clone_path.and_then(crate::git::current_branch) {
        synced.branch_name = Some(live_branch);
    }

    let git_remote = crate::git::remote_url(source_path, "origin");
    let project = crate::sync::meta::ProjectIdentity {
        name: project_name.to_string(),
        git_remote,
    };
    let store = crate::sync::config::build_store_from_settings(settings)?;
    let mut ledger = crate::sync::ledger::SyncLedger::load();
    let revision = crate::sync::rt::block_on(crate::sync::push::push_session_bundle(
        store.as_ref(),
        &mut ledger,
        &synced,
        project,
        device_id,
        std::time::SystemTime::now(),
    ))?;

    // Upload the Claude transcript so the conversation replays on another Mac.
    // Best-effort: a session whose transcript file doesn't exist yet (never
    // opened, or already cleaned) simply syncs its metadata.
    if let Some(clone) = clone_path {
        let conversation_id = synced.claude_session_id.as_deref().unwrap_or(&synced.id);
        if let Some(transcript_path) =
            crate::transcript::expected_session_jsonl(clone, conversation_id)
        {
            if let Ok(bytes) = std::fs::read(&transcript_path) {
                crate::sync::rt::block_on(crate::sync::push::push_transcript(
                    store.as_ref(),
                    &synced.id,
                    bytes,
                ))?;
            }
        }
    }

    ledger.save()?;
    Ok(revision)
}
