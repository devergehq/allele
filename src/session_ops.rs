//! Session lifecycle operations on AppState.
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 3.
//! Add/resume/close/remove/navigate session methods live here; see
//! ARCHITECTURE.md §2 for the module map and §3.5 for the eventual
//! AppState sub-struct composition.

use gpui::*;
use tracing::{info, warn};

use crate::actions::{PendingAction, SessionCursor};
use crate::app_state::AppState;
use crate::session::{Session, SessionStatus};
use crate::state::ArchivedSession;
use crate::terminal::{clamp_font_size, TerminalEvent, TerminalView, DEFAULT_FONT_SIZE};
use crate::{agents, browser, claude_session_history_exists, clone, config, git, project, settings};

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
        let Some(project) = self.projects.get_mut(project_idx) else { return; };

        // Guard: if the source directory no longer exists (e.g. repo was
        // moved), prompt the user to relocate rather than failing mid-clone.
        if !project.source_path.exists() {
            warn!(
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
                    if let Err(e) = git::create_session_branch(
                        &clone_path,
                        &session_id_for_session,
                    ) {
                        warn!(
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
                cx.subscribe(&terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
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
        let Some(project) = self.projects.get_mut(project_idx) else { return; };

        if !project.source_path.exists() {
            self.pending_action = Some(PendingAction::RelocateProject(project_idx));
            cx.notify();
            return;
        }

        let source_path = project.source_path.clone();
        let project_name = project.name.clone();
        let session_count = project.sessions.len() + project.loading_sessions.len() + 1;

        let project_override = config::ProjectConfig::load(&project.source_path)
            .and_then(|c| c.agent);
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
        });
        cx.notify();

        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        let session_id_for_clone = session_id.clone();
        let session_id_for_session = session_id.clone();
        let display_label_for_task = display_label.clone();
        let agent_id_for_task = agent_id.clone();
        let pull_before_clone = self.user_settings.git_pull_before_new_session;
        let branch_slug = custom_branch_slug;
        let prompt = initial_prompt;

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
                    );
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
                    if let Err(e) = git::create_session_branch(
                        &clone_path,
                        &session_id_for_session,
                    ) {
                        warn!(
                            "create_session_branch failed for {session_id_for_session}: {e}"
                        );
                    }

                    // Rename the branch if the user provided a custom name.
                    // Use the name directly (no allele/session/ prefix) since
                    // the user explicitly chose it.
                    if let Some(ref name) = branch_slug {
                        let sanitised = git::sanitise_branch_name(name, 100);
                        if !sanitised.is_empty() {
                            if let Err(e) = git::rename_current_branch(
                                &clone_path,
                                &sanitised,
                            ) {
                                warn!("branch rename failed for custom name: {e}");
                            }
                        }
                    }
                }

                let initial_font_size = this.user_settings.font_size;
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(window, cx, command, Some(clone_path.clone()), initial_font_size)
                });

                cx.subscribe(&terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
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

                let mut session = Session::new_with_id(
                    session_id_for_session,
                    display_label_for_task,
                    terminal_view.clone(),
                )
                .with_clone(clone_path)
                .with_agent_id(agent_id_for_task.clone());

                if skip_auto_naming {
                    session.auto_naming_fired = true;
                }

                let Some(project) = this.projects.get_mut(project_idx) else { return; };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                let cursor = SessionCursor { project_idx, session_idx };
                this.active = Some(cursor);
                this.apply_project_config(cursor, window, cx);
                this.save_state();

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
        let Some(project) = self.projects.get_mut(cursor.project_idx) else { return; };
        let Some(session) = project.sessions.get_mut(cursor.session_idx) else { return; };

        // Drop the terminal_view and drawer — Drop impl on PtyTerminal sends
        // Msg::Shutdown, killing the subprocesses. The clone on disk is untouched.
        session.terminal_view = None;
        // Drop the drawer PTYs but preserve the names so the next open
        // restores the same tab layout (matches the rehydration path).
        let names: Vec<String> = session.drawer_tabs.iter().map(|t| t.name.clone()).collect();
        session.drawer_tabs.clear();
        session.pending_drawer_tab_names = names;
        session.drawer_visible = false;
        session.status = SessionStatus::Suspended;
        session.last_active = std::time::SystemTime::now();

        // If this was the active session, clear the active cursor — the main
        // area will show the "No active session" placeholder until the user
        // clicks something else.
        if self.active == Some(cursor) {
            self.active = None;
        }

        self.save_state();
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
        self.pending_action = Some(PendingAction::FocusActive);
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
        let Some(project) = self.projects.get(cursor.project_idx) else { return; };
        let Some(session) = project.sessions.get(cursor.session_idx) else { return; };
        let Some(clone_path) = session.clone_path.clone() else {
            warn!(
                "Cannot resume session {} — no clone_path on record",
                session.id
            );
            return;
        };

        if !clone_path.exists() {
            warn!(
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
            TerminalView::new(window, cx, command, Some(clone_path.clone()), initial_font_size)
        });

        // Subscribe to terminal events so the resumed session wires up the
        // same shortcut actions (NewSession, CloseSession, SwitchSession)
        // as freshly-created ones.
        cx.subscribe(&terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
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
                    // Mirror the fresh-spawn handler so Cmd+1..9 also works
                    // from resumed sessions.
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
                    this.pending_action = Some(PendingAction::UpdateFontSize(DEFAULT_FONT_SIZE));
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
        let Some(project) = self.projects.get_mut(cursor.project_idx) else { return; };
        if cursor.session_idx >= project.sessions.len() { return; }

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
            });
        }

        // Register Chrome-tab cleanup as a hook on the PTY: when the
        // terminal is dropped below, the tab closes as part of the same
        // teardown sequence (alongside SIGTERM to any dev servers).
        // Suspended sessions have no terminal_view, so fall back to the
        // direct call. Integration-disabled case: still no-op.
        let close_tab = self.user_settings.browser_integration_enabled
            .then_some(removed_browser_tab_id)
            .flatten();
        if let Some(id) = close_tab {
            match removed.terminal_view.as_ref() {
                Some(tv) => tv.update(cx, |view, _| {
                    view.on_close(move || { let _ = browser::close_tab(id); });
                }),
                None => { let _ = browser::close_tab(id); }
            }
        }

        // Run the project-declared shutdown command (if any) before we
        // drop the PTY and archive/trash the clone. Runs in-line — the
        // Discard action is already destructive and user-confirmed, and
        // the archive pipeline below is async, so a brief block here is
        // acceptable. Failure is logged and teardown continues so a
        // broken hook can't strand the clone on disk.
        if let Some(clone_path) = clone_path.as_ref() {
            if let Some(cfg) = config::ProjectConfig::load(clone_path) {
                let shutdown = cfg
                    .shutdown
                    .as_ref()
                    .map(|s| config::substitute(s, removed.allocated_port, clone_path))
                    .filter(|s| !s.trim().is_empty());
                if let Some(cmd) = shutdown {
                    match std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .current_dir(clone_path)
                        .status()
                    {
                        Ok(s) if !s.success() => {
                            warn!("allele: shutdown command exited with {s} — continuing");
                        }
                        Err(e) => {
                            warn!("allele: failed to run shutdown command: {e} — continuing");
                        }
                        _ => {}
                    }
                }
            }
        }

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
                    Some(SessionCursor { project_idx: cursor.project_idx, session_idx: new_session_idx })
                } else {
                    // Fall back to any session in any project
                    self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                        if !p.sessions.is_empty() {
                            Some(SessionCursor { project_idx: p_idx, session_idx: 0 })
                        } else {
                            None
                        }
                    })
                };
            } else if active.project_idx == cursor.project_idx && active.session_idx > cursor.session_idx {
                // Active session in same project shifted down by one
                self.active = Some(SessionCursor {
                    project_idx: active.project_idx,
                    session_idx: active.session_idx - 1,
                });
            }
        }

        // Persist the updated session list now that the entry is gone.
        self.save_state();
        cx.notify();

        // Spawn the archive-then-delete pipeline on a background task
        if let Some(clone_path) = clone_path {
            let project_idx = cursor.project_idx;
            let placeholder_id_for_task = placeholder_id.clone();
            cx.spawn(async move |this, cx| {
                let delete_result = cx
                    .background_executor()
                    .spawn(async move {
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
                            warn!(
                                "archive_session failed for {session_id_for_task}: {e}"
                            );
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
                        project.loading_sessions.retain(|l| l.id != placeholder_id_for_task);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }
}
