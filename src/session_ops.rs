//! Session lifecycle operations: creating, resuming, and terminal event wiring.

use gpui::*;

use crate::actions::{PendingAction, SessionCursor};
use crate::app_state::AppState;
use crate::{agents, clone, config, git, project, session, settings, terminal};
use session::{Session, SessionStatus};
use terminal::{clamp_font_size, TerminalEvent, TerminalView, DEFAULT_FONT_SIZE};

impl AppState {
    /// Wire up the standard terminal event subscription for a session's
    /// primary TerminalView. Both `add_session_to_project` and
    /// `resume_session` need the same routing — this helper deduplicates it.
    pub(crate) fn subscribe_terminal_events(
        &self,
        terminal_view: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe(terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
            match event {
                TerminalEvent::NewSession => {
                    this.pending_action = Some(PendingAction::NewSessionInActiveProject);
                    cx.notify();
                }
                TerminalEvent::CloseSession => {
                    this.pending_action = Some(PendingAction::CloseActiveSession);
                    cx.notify();
                }
                TerminalEvent::SwitchSession(target) => {
                    let target = *target;
                    let mut flat_idx = 0;
                    'outer: for (p_idx, project) in this.projects.iter().enumerate() {
                        for (s_idx, _) in project.sessions.iter().enumerate() {
                            if flat_idx == target {
                                this.active = Some(SessionCursor { project_idx: p_idx, session_idx: s_idx });
                                this.pending_action = Some(PendingAction::FocusActive);
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
                    this.pending_action = Some(PendingAction::ToggleDrawer);
                    cx.notify();
                }
                TerminalEvent::ToggleSidebar => {
                    this.pending_action = Some(PendingAction::ToggleSidebar);
                    cx.notify();
                }
                TerminalEvent::ToggleRightSidebar => {
                    this.pending_action = Some(PendingAction::ToggleRightSidebar);
                    cx.notify();
                }
                TerminalEvent::OpenScratchPad => {
                    this.pending_action = Some(PendingAction::OpenScratchPad);
                    cx.notify();
                }
                TerminalEvent::AdjustFontSize(delta) => {
                    let new_size = clamp_font_size(this.user_settings.font_size + delta);
                    this.pending_action = Some(PendingAction::UpdateFontSize(new_size));
                    cx.notify();
                }
                TerminalEvent::ResetFontSize => {
                    this.pending_action =
                        Some(PendingAction::UpdateFontSize(DEFAULT_FONT_SIZE));
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
            }
        }).detach();
    }

    /// Create a new session inside a project. Runs the APFS clone on a
    /// background task so the UI stays responsive. A "Cloning..." placeholder
    /// appears in the sidebar while the clone is in flight.
    pub(crate) fn add_session_to_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else { return; };

        // Guard: if the source directory no longer exists (e.g. repo was
        // moved), prompt the user to relocate rather than failing mid-clone.
        if !project.source_path.exists() {
            eprintln!(
                "Project source path missing: {} — prompting for relocation",
                project.source_path.display()
            );
            self.pending_action = Some(PendingAction::RelocateProject(project_idx));
            cx.notify();
            return;
        }

        // If the working tree has uncommitted changes, prompt the user
        // before creating a session. The user can choose to proceed (the
        // dirty state will be present in the clone) or cancel to clean up.
        if git::is_working_tree_dirty(&project.source_path) && self.confirming_dirty_session.is_none() {
            self.confirming_dirty_session = Some(project_idx);
            cx.notify();
            return;
        }
        // Clear any prior dirty confirmation (user chose to proceed).
        self.confirming_dirty_session = None;

        let source_path = project.source_path.clone();
        let project_name = project.name.clone();
        let session_count = project.sessions.len() + project.loading_sessions.len() + 1;

        // Pick the agent for this session: allele.json override first,
        // then the global default. Falls through to the first enabled
        // agent with a resolved path. `None` here means "no agent
        // available" — the PTY drops into the user's default shell.
        let project_override = config::ProjectConfig::load(&project.source_path)
            .and_then(|c| c.agent);
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
        });
        cx.notify();

        // Spawn the clone on a background task, then finish on the main thread
        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        let pull_before_clone = self.user_settings.git_pull_before_new_session;
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
                                eprintln!(
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
                        eprintln!("Created APFS clone at: {}", p.display());
                        p
                    }
                    Err(e) => {
                        eprintln!("Failed to create APFS clone: {e}");
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
                    if let Err(e) = git::create_session_branch(
                        &clone_path,
                        &session_id_for_session,
                    ) {
                        eprintln!(
                            "create_session_branch failed for {session_id_for_session}: {e}"
                        );
                    }
                }

                // Create the terminal view with the clone as PWD
                let initial_font_size = this.user_settings.font_size;
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(window, cx, command, Some(clone_path.clone()), initial_font_size)
                });

                // Subscribe to terminal events
                this.subscribe_terminal_events(&terminal_view, cx);

                let session = Session::new_with_id(
                    session_id_for_session,
                    display_label_for_task,
                    terminal_view,
                )
                .with_clone(clone_path)
                .with_agent_id(agent_id_for_task.clone());
                let Some(project) = this.projects.get_mut(project_idx) else { return; };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                let cursor = SessionCursor { project_idx, session_idx };
                this.active = Some(cursor);
                this.apply_project_config(cursor, window, cx);
                this.save_state();
                cx.notify();
            });

            // Auto-dismiss the pull warning banner after 8 seconds.
            if this.read_with(cx, |this, _cx| this.pull_warning.is_some()).unwrap_or(false) {
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
        let Some(project) = self.projects.get(cursor.project_idx) else { return; };
        let Some(session) = project.sessions.get(cursor.session_idx) else { return; };
        let Some(clone_path) = session.clone_path.clone() else {
            eprintln!(
                "Cannot resume session {} — no clone_path on record",
                session.id
            );
            return;
        };

        if !clone_path.exists() {
            eprintln!(
                "Cannot resume session {} — clone_path is missing on disk: {}",
                session.id,
                clone_path.display()
            );
            return;
        }

        let session_id = session.id.clone();
        let label = session.label.clone();
        let stored_agent_id = session.agent_id.clone();

        // Resolve the agent. Prefer the session's stored agent_id so a
        // resume always uses whatever spawned the session originally,
        // even if the user has since changed the global default.
        // Falls back to allele.json → global default → first enabled.
        let project_override = config::ProjectConfig::load(&project.source_path)
            .and_then(|c| c.agent);
        let agent = agents::resolve(
            &self.user_settings.agents,
            self.user_settings.default_agent.as_deref(),
            project_override.as_deref(),
            stored_agent_id.as_deref(),
        )
        .cloned();

        // Only adapters that understand session ids care about history —
        // for claude this gates `--resume` vs `--session-id`.
        let has_history = crate::claude_session_history_exists(&session_id);
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
            TerminalView::new(window, cx, command, Some(clone_path.clone()), initial_font_size)
        });

        // Subscribe to terminal events so the resumed session wires up the
        // same shortcut actions (NewSession, CloseSession, SwitchSession)
        // as freshly-created ones.
        self.subscribe_terminal_events(&terminal_view, cx);

        let resolved_agent_id = agent.as_ref().map(|a| a.id.clone());

        // Attach the new PTY to the existing session entry.
        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            session.terminal_view = Some(terminal_view);
            session.status = SessionStatus::Running;
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
            self.pending_action = Some(PendingAction::FocusActive);
        }

        self.apply_project_config(cursor, window, cx);
        self.save_state();
        cx.notify();
    }
}
