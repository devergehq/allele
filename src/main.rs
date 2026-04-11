mod terminal;
mod sidebar;
mod clone;
mod hooks;
mod project;
mod session;
mod settings;
mod state;

use gpui::*;
use project::Project;
use session::{Session, SessionStatus};
use settings::{ProjectSave, Settings};
use state::{PersistedSession, PersistedState};
use terminal::{ShellCommand, TerminalEvent, TerminalView};
use terminal::pty_terminal::PtyTerminal;
use std::path::PathBuf;

#[derive(Debug)]
enum PendingAction {
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
}

/// Position of a session in the project tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SessionCursor {
    project_idx: usize,
    session_idx: usize,
}

struct AppState {
    projects: Vec<Project>,
    active: Option<SessionCursor>,
    claude_path: Option<String>,
    pending_action: Option<PendingAction>,
    // Sidebar resize state
    sidebar_width: f32,
    sidebar_resizing: bool,
    /// Inline confirmation gate for the Discard action. When `Some(cursor)`
    /// the sidebar row at that cursor shows a confirm/cancel prompt instead
    /// of the usual buttons.
    confirming_discard: Option<SessionCursor>,
    /// Absolute path to the Allele hooks.json, passed to claude via
    /// `--settings <path>` at every spawn. `None` if install_if_missing
    /// failed — in that case hooks are silently disabled and the app still
    /// functions normally.
    hooks_settings_path: Option<PathBuf>,
    /// Current user settings (sound/notification preferences).
    user_settings: Settings,
}

const SIDEBAR_MIN_WIDTH: f32 = 160.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 240.0;

impl AppState {
    /// Get the currently active session, if any.
    fn active_session(&self) -> Option<&Session> {
        let cursor = self.active?;
        self.projects
            .get(cursor.project_idx)?
            .sessions
            .get(cursor.session_idx)
    }

    fn save_settings(&self) {
        // Start from the live user_settings so attention preferences
        // (sound/notification opt-ins) are preserved on every write, then
        // override only the fields that the AppState is the source of truth
        // for (sidebar width, project list, etc.).
        let settings = Settings {
            sidebar_width: self.sidebar_width,
            font_size: 13.0,
            window_x: None,
            window_y: None,
            window_width: None,
            window_height: None,
            projects: self.projects.iter().map(|p| ProjectSave {
                id: p.id.clone(),
                name: p.name.clone(),
                source_path: p.source_path.clone(),
            }).collect(),
            ..self.user_settings.clone()
        };
        settings.save();
    }

    /// Persist every session across every project to `~/.allele/state.json`.
    /// Called after any mutation that creates, removes, or transitions a session.
    /// Errors are logged but not surfaced — losing a state write is survivable,
    /// the orphan sweep will clean up any mismatch on next startup.
    fn save_state(&self) {
        let mut persisted = PersistedState::default();
        for project in &self.projects {
            for session in &project.sessions {
                persisted
                    .sessions
                    .push(PersistedSession::from_session(session, &project.id));
            }
        }
        if let Err(e) = persisted.save() {
            eprintln!("Failed to save state.json: {e}");
        }
    }

    /// Open the native folder picker and queue an action to create a project.
    fn open_folder_picker(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select project folder".into()),
        });

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this: &mut Self, cx| {
                        this.pending_action = Some(PendingAction::OpenProjectAtPath(path));
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Create a new project from a source path. Does NOT auto-create a session.
    /// Returns the index of the new project.
    fn create_project(&mut self, source_path: PathBuf, cx: &mut Context<Self>) -> usize {
        let name = Project::name_from_path(&source_path);
        let project = Project::new(name, source_path);
        self.projects.push(project);
        let idx = self.projects.len() - 1;
        self.save_settings();
        cx.notify();
        idx
    }

    /// Create a new session inside a project. Runs the APFS clone on a
    /// background task so the UI stays responsive. A "Cloning..." placeholder
    /// appears in the sidebar while the clone is in flight.
    fn add_session_to_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else { return; };
        let source_path = project.source_path.clone();
        let project_name = project.name.clone();
        let session_count = project.sessions.len() + project.loading_sessions.len() + 1;

        let session_id = uuid::Uuid::new_v4().to_string();
        let display_label = if self.claude_path.is_some() {
            format!("Claude {session_count}")
        } else {
            format!("Shell {session_count}")
        };

        // Build the claude command with --session-id + --name so our internal
        // UUID *is* Claude's session ID. This is what enables cold-resume later:
        // `claude --resume <same-uuid>` picks up the conversation in the same
        // clone path. --settings injects Allele's attention-routing
        // hooks so the Notification/Stop events flow back into the sidebar.
        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let command = self.claude_path.as_ref().map(|path| {
            let mut args = vec![
                "--session-id".to_string(),
                session_id.clone(),
                "--name".to_string(),
                display_label.clone(),
            ];
            if let Some(hooks) = hooks_path_str {
                args.push("--settings".to_string());
                args.push(hooks);
            }
            ShellCommand::with_args(path.clone(), args)
        });

        // Add a loading placeholder immediately so the user sees feedback
        project.loading_sessions.push(project::LoadingSession {
            id: session_id.clone(),
            label: display_label.clone(),
        });
        cx.notify();

        // Spawn the clone on a background task, then finish on the main thread
        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        // Two copies: one moves into the background clonefile closure (where
        // it's used as the short-ID source), the other is captured by the
        // main-thread update_in closure to set Session.id.
        let session_id_for_clone = session_id.clone();
        let session_id_for_session = session_id.clone();
        let display_label_for_task = display_label.clone();

        cx.spawn_in(window, async move |this, cx| {
            // Run the clonefile() syscall on the background executor
            let clone_result = cx
                .background_executor()
                .spawn(async move {
                    clone::create_session_clone(&source_for_task, &project_name_for_task, &session_id_for_clone)
                })
                .await;

            // Back on the main thread with window access
            let _ = this.update_in(cx, move |this: &mut Self, window, cx| {
                // Resolve clone path (or fall back to source on failure)
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

                // Find the project again (indices may have shifted if user removed projects)
                let Some(project) = this.projects.get_mut(project_idx) else {
                    // Project was removed while we were cloning — clean up the clone
                    let _ = clone::delete_clone(&clone_path);
                    return;
                };

                // Remove the loading placeholder
                project.loading_sessions.retain(|l| l.id != session_id);

                // Create the terminal view with the clone as PWD
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(window, cx, command, Some(clone_path.clone()))
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
                    }
                }).detach();

                let session = Session::new_with_id(
                    session_id_for_session,
                    display_label_for_task,
                    terminal_view,
                )
                .with_clone(clone_path);
                let Some(project) = this.projects.get_mut(project_idx) else { return; };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                this.active = Some(SessionCursor { project_idx, session_idx });
                this.save_state();
                cx.notify();
            });
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
    fn close_session_keep_clone(
        &mut self,
        cursor: SessionCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(cursor.project_idx) else { return; };
        let Some(session) = project.sessions.get_mut(cursor.session_idx) else { return; };

        // Drop the terminal_view — Drop impl on PtyTerminal sends Msg::Shutdown,
        // killing the subprocess. The clone on disk is untouched.
        session.terminal_view = None;
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

    /// Apply a single hook event to the matching session.
    ///
    /// Transition rules:
    /// - `Notification` → `AwaitingInput` (permission prompt / idle wait)
    /// - `Stop` → `ResponseReady` (Claude finished a response turn)
    /// - `PreToolUse` / `PostToolUse` → `Running` (Claude is actively executing
    ///   a tool, which means any prior permission prompt has been resolved)
    /// - `UserPromptSubmit` → `Running` (user submitted new input)
    /// - `SessionStart` → `Running`
    /// - `SessionEnd` → `Done`
    ///
    /// Note: `Stop` no longer has special handling for `AwaitingInput`.
    /// In practice Claude doesn't emit `Stop` while still blocked on a
    /// prompt — `Stop` means the response turn completed, which implies
    /// any prompt was resolved. The earlier "don't stomp" rule was
    /// overly defensive and caused stuck AwaitingInput states in the wild.
    fn apply_hook_event(&mut self, event: hooks::HookEvent, cx: &mut Context<Self>) {
        // Find the matching session by its internal ID (= Claude session ID).
        let Some((p_idx, s_idx)) = self.projects.iter().enumerate().find_map(|(p_idx, p)| {
            p.sessions
                .iter()
                .position(|s| s.id == event.session_id)
                .map(|s_idx| (p_idx, s_idx))
        }) else {
            // Event for an unknown session — probably stale, drop it.
            return;
        };

        let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        else {
            return;
        };

        let prior = session.status;
        let now = std::time::SystemTime::now();
        session.last_active = now;

        use hooks::HookKind;
        use session::SessionStatus;

        let new_status = match event.kind {
            HookKind::Notification => Some(SessionStatus::AwaitingInput),
            HookKind::Stop => Some(SessionStatus::ResponseReady),
            HookKind::PreToolUse | HookKind::PostToolUse => {
                // Tool execution is the key clearing signal. If Claude is
                // running a tool, any prior permission prompt has been
                // resolved and we should be back in Running. If we were
                // already Running, this is a no-op (the prior==new guard
                // below drops it).
                Some(SessionStatus::Running)
            }
            HookKind::UserPromptSubmit => Some(SessionStatus::Running),
            HookKind::SessionStart => Some(SessionStatus::Running),
            HookKind::SessionEnd => Some(SessionStatus::Done),
            HookKind::Other => None,
        };

        let Some(new_status) = new_status else { return };
        if new_status == prior { return; }

        session.status = new_status;

        // Capture the label for notifications BEFORE we drop the borrow.
        let session_label = session.label.clone();
        let project_name = self
            .projects
            .get(p_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        // Persist the updated status.
        self.save_state();

        // Fire sound + notification affordances — ONLY on transitions INTO
        // an attention state, never on transitions out of one.
        match new_status {
            SessionStatus::AwaitingInput => {
                if self.user_settings.sound_on_awaiting_input {
                    let sound_path = self
                        .user_settings
                        .awaiting_input_sound_path
                        .clone()
                        .unwrap_or_else(|| settings::DEFAULT_AWAITING_INPUT_SOUND.to_string());
                    hooks::play_sound(&sound_path);
                }
                if self.user_settings.notifications_enabled {
                    hooks::show_notification(
                        &format!("{project_name} — needs input"),
                        &format!("{session_label} is blocked and waiting for you"),
                    );
                }
            }
            SessionStatus::ResponseReady => {
                if self.user_settings.sound_on_response_ready {
                    let sound_path = self
                        .user_settings
                        .response_ready_sound_path
                        .clone()
                        .unwrap_or_else(|| settings::DEFAULT_RESPONSE_READY_SOUND.to_string());
                    hooks::play_sound(&sound_path);
                }
                if self.user_settings.notifications_enabled {
                    hooks::show_notification(
                        &format!("{project_name} — response ready"),
                        &format!("{session_label} finished responding"),
                    );
                }
            }
            _ => {}
        }

        cx.notify();
    }

    /// Cycle the active session pointer across all non-Suspended sessions
    /// in the flat order they appear in the sidebar. `delta = -1` = previous,
    /// `delta = 1` = next. Wraps at both ends. Suspended sessions are
    /// deliberately skipped — quick-flicking shouldn't auto-spawn resumed
    /// Claude processes; the user clicks the ⏸ row explicitly to resume.
    fn navigate_session(&mut self, delta: i32, cx: &mut Context<Self>) {
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
    fn resume_session(
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

        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let command = self.claude_path.as_ref().map(|path| {
            let mut args = vec![
                "--resume".to_string(),
                session_id.clone(),
                "--name".to_string(),
                label.clone(),
            ];
            if let Some(hooks) = hooks_path_str {
                args.push("--settings".to_string());
                args.push(hooks);
            }
            ShellCommand::with_args(path.clone(), args)
        });

        // Build the new TerminalView on the main thread with window access.
        let terminal_view = cx.new(|cx| {
            TerminalView::new(window, cx, command, Some(clone_path.clone()))
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
            }
        }).detach();

        // Attach the new PTY to the existing session entry.
        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            session.terminal_view = Some(terminal_view);
            session.status = SessionStatus::Running;
            session.last_active = std::time::SystemTime::now();
            self.active = Some(cursor);
            self.pending_action = Some(PendingAction::FocusActive);
        }

        self.save_state();
        cx.notify();
    }

    /// Discard a session — kill the PTY, delete the APFS clone, remove from
    /// the sidebar, and drop the corresponding entry from `state.json`.
    ///
    /// This is the *destructive* path, reached only through the explicit
    /// Discard action with confirmation. The plain Close action uses
    /// `close_session_keep_clone` instead.
    fn remove_session(
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
        // Drop the Session — this frees the terminal_view entity (if any),
        // which in turn kills the PTY via the Drop impl on PtyTerminal.
        // Suspended sessions have `terminal_view = None` so there's no PTY
        // to kill; only the clone needs cleanup.
        drop(removed);

        // Show a "Deleting…" placeholder if there's a clone to clean up
        let placeholder_id = uuid::Uuid::new_v4().to_string();
        if clone_path.is_some() {
            project.loading_sessions.push(project::LoadingSession {
                id: placeholder_id.clone(),
                label: format!("{removed_label} (deleting)"),
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

        // Spawn the filesystem cleanup on a background task
        if let Some(clone_path) = clone_path {
            let project_idx = cursor.project_idx;
            let placeholder_id_for_task = placeholder_id.clone();
            cx.spawn(async move |this, cx| {
                let delete_result = cx
                    .background_executor()
                    .spawn(async move { clone::delete_clone(&clone_path) })
                    .await;

                if let Err(e) = delete_result {
                    eprintln!("Failed to delete clone: {e}");
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

    /// Remove a project and all its sessions (deleting all clones asynchronously).
    fn remove_project(&mut self, project_idx: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if project_idx >= self.projects.len() { return; }

        // Remove the project from the list immediately. The terminal entities
        // are dropped, which kills the PTYs.
        let project = self.projects.remove(project_idx);

        // Collect all clone paths for background deletion
        let clone_paths: Vec<PathBuf> = project
            .sessions
            .iter()
            .filter_map(|s| s.clone_path.clone())
            .collect();

        // Adjust the active cursor — if the removed project was active or
        // before the active one, shift accordingly.
        self.active = match self.active {
            Some(active) if active.project_idx == project_idx => {
                // Active was in the removed project — pick any other session
                self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                    if !p.sessions.is_empty() {
                        Some(SessionCursor { project_idx: p_idx, session_idx: 0 })
                    } else {
                        None
                    }
                })
            }
            Some(active) if active.project_idx > project_idx => {
                Some(SessionCursor {
                    project_idx: active.project_idx - 1,
                    session_idx: active.session_idx,
                })
            }
            other => other,
        };

        self.save_settings();
        self.save_state();
        cx.notify();

        // Spawn background cleanup for all clones
        if !clone_paths.is_empty() {
            cx.spawn(async move |_this, cx| {
                cx.background_executor()
                    .spawn(async move {
                        for path in clone_paths {
                            if let Err(e) = clone::delete_clone(&path) {
                                eprintln!("Failed to delete clone at {}: {e}", path.display());
                            }
                        }
                    })
                    .await;
            })
            .detach();
        }
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Log to ~/.config/allele/crash.log
        if let Some(home) = dirs::home_dir() {
            let log_dir = home.join(".config").join("allele");
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join("crash.log");
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let location = info.location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());

            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic>".to_string()
            };

            let entry = format!(
                "\n=== PANIC @ {timestamp} ===\nLocation: {location}\nMessage: {payload}\n",
            );

            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(entry.as_bytes())
                });

            eprintln!("\n*** allele crashed ***");
            eprintln!("{entry}");
            eprintln!("Crash log: {}", log_path.display());
        }

        // Call the default hook to print the normal backtrace too
        default_hook(info);
    }));
}

fn main() {
    install_panic_hook();
    let application = Application::new();

    application.run(move |cx: &mut App| {
        // Load bundled fonts so we have a deterministic monospace font
        // regardless of what's installed on the system.
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice()),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf").as_slice()),
            ])
            .expect("failed to load bundled fonts");

        // Load persisted settings
        let loaded_settings = Settings::load();
        eprintln!(
            "Loaded settings: sidebar_width={}, font_size={}",
            loaded_settings.sidebar_width, loaded_settings.font_size
        );

        // Load persisted session state (may be empty on first run).
        let loaded_state = PersistedState::load();
        eprintln!("Loaded persisted state: {} sessions", loaded_state.sessions.len());

        // Install the Allele hook receiver and settings file so every
        // claude spawn can route attention signals back into the UI. Failure
        // is non-fatal — the app still runs, it just won't get hook events.
        let hooks_settings_path: Option<PathBuf> = match hooks::install_if_missing() {
            Ok(path) => {
                eprintln!("Installed Allele hooks at {}", path.display());
                Some(path)
            }
            Err(e) => {
                eprintln!("Failed to install Allele hooks: {e} (attention routing disabled)");
                None
            }
        };

        // Conservative orphan sweep: move any on-disk clone not referenced by
        // the loaded state into ~/.allele/trash/, then purge trash
        // entries older than TRASH_TTL_DAYS. This runs before the window
        // opens so the user never sees stale placeholders.
        let referenced = state::referenced_clone_paths(&loaded_state);
        match clone::sweep_orphans(&referenced) {
            Ok(0) => {}
            Ok(n) => eprintln!("Orphan sweep trashed {n} unreferenced clone(s)"),
            Err(e) => eprintln!("Orphan sweep failed: {e}"),
        }
        match clone::purge_trash_older_than_days(clone::TRASH_TTL_DAYS) {
            Ok(0) => {}
            Ok(n) => eprintln!("Trash purge removed {n} expired entry/entries"),
            Err(e) => eprintln!("Trash purge failed: {e}"),
        }

        let claude_path = PtyTerminal::find_claude()
            .map(|p| p.to_string_lossy().to_string());

        if let Some(ref path) = claude_path {
            eprintln!("Found Claude Code at: {path}");
        } else {
            eprintln!("Claude Code not found — falling back to default shell");
        }

        let claude_path_clone = claude_path.clone();

        let window_bounds = match (
            loaded_settings.window_x,
            loaded_settings.window_y,
            loaded_settings.window_width,
            loaded_settings.window_height,
        ) {
            (Some(x), Some(y), Some(w), Some(h)) => Some(WindowBounds::Windowed(Bounds::new(
                point(px(x), px(y)),
                size(px(w), px(h)),
            ))),
            _ => None,
        };

        let settings_for_window = loaded_settings.clone();
        let loaded_state_for_window = loaded_state.clone();
        let hooks_settings_path_for_window = hooks_settings_path.clone();

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Allele".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(800.0), px(600.0))),
                window_bounds,
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx: &mut Context<AppState>| {
                    // Observe window bounds changes and persist them.
                    cx.observe_window_bounds(window, |this: &mut AppState, window, _cx| {
                        let viewport = window.viewport_size();
                        let settings = Settings {
                            sidebar_width: this.sidebar_width,
                            font_size: 13.0,
                            window_x: None,
                            window_y: None,
                            window_width: Some(f32::from(viewport.width)),
                            window_height: Some(f32::from(viewport.height)),
                            projects: this.projects.iter().map(|p| ProjectSave {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                source_path: p.source_path.clone(),
                            }).collect(),
                            ..this.user_settings.clone()
                        };
                        settings.save();
                    }).detach();

                    // Rehydrate projects from settings.
                    let mut projects: Vec<Project> = settings_for_window.projects.iter().map(|p| {
                        let mut proj = Project::new(p.name.clone(), p.source_path.clone());
                        proj.id = p.id.clone();
                        proj
                    }).collect();

                    // Rehydrate sessions from state.json as Suspended entries
                    // (no PTY, ⏸ icon). They show up in the sidebar immediately
                    // and cold-resume on click via `claude --resume <id>`.
                    // Sessions whose owning project no longer exists are
                    // silently dropped — on the next save_state the entries
                    // will be removed from disk too.
                    for persisted in &loaded_state_for_window.sessions {
                        let Some(project) = projects
                            .iter_mut()
                            .find(|p| p.id == persisted.project_id)
                        else {
                            eprintln!(
                                "Dropping persisted session {} — owning project {} is gone",
                                persisted.id, persisted.project_id
                            );
                            continue;
                        };

                        let session = Session::suspended_from_persisted(
                            persisted.id.clone(),
                            persisted.label.clone(),
                            persisted.started_at,
                            persisted.last_active,
                            persisted.clone_path.clone(),
                        );
                        project.sessions.push(session);
                    }

                    // Spawn the hook-event polling task. Runs for the life
                    // of the app, reads ~/.allele/events/*.jsonl every
                    // 250ms, and routes each new event into apply_hook_event.
                    //
                    // Fast-forward existing files so we don't flood the user
                    // with pre-existing events from a previous app session.
                    cx.spawn(async move |this, cx| {
                        let mut watcher = hooks::EventWatcher::new();
                        watcher.initialize_offsets();

                        loop {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(250))
                                .await;

                            let events = watcher.poll();
                            if events.is_empty() {
                                continue;
                            }

                            if this
                                .update(cx, |this: &mut AppState, cx| {
                                    for event in events {
                                        this.apply_hook_event(event, cx);
                                    }
                                })
                                .is_err()
                            {
                                break; // AppState dropped — app is exiting
                            }
                        }
                    })
                    .detach();

                    AppState {
                        projects,
                        active: None,
                        claude_path: claude_path_clone,
                        pending_action: None,
                        sidebar_width: settings_for_window.sidebar_width
                            .max(SIDEBAR_MIN_WIDTH),
                        sidebar_resizing: false,
                        confirming_discard: None,
                        hooks_settings_path: hooks_settings_path_for_window,
                        user_settings: settings_for_window.clone(),
                    }
                })
            },
        )
        .unwrap();
    });
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process pending actions
        if let Some(action) = self.pending_action.take() {
            match action {
                PendingAction::NewSessionInActiveProject => {
                    if let Some(active) = self.active {
                        self.add_session_to_project(active.project_idx, window, cx);
                    }
                }
                PendingAction::CloseActiveSession => {
                    // Keyboard/menu "close" — preserve the clone so the user
                    // can cold-resume later. Discard is an explicit gesture only.
                    if let Some(active) = self.active {
                        self.close_session_keep_clone(active, window, cx);
                    }
                }
                PendingAction::FocusActive => {
                    if let Some(session) = self.active_session() {
                        if let Some(tv) = session.terminal_view.as_ref() {
                            let fh = tv.read(cx).focus_handle.clone();
                            fh.focus(window, cx);
                        }
                    }
                }
                PendingAction::OpenProjectAtPath(path) => {
                    let idx = self.create_project(path, cx);
                    // Auto-create first session for the new project
                    self.add_session_to_project(idx, window, cx);
                }
                PendingAction::AddSessionToProject(project_idx) => {
                    self.add_session_to_project(project_idx, window, cx);
                }
                PendingAction::RemoveProject(project_idx) => {
                    self.remove_project(project_idx, window, cx);
                }
                PendingAction::CloseSessionKeepClone { project_idx, session_idx } => {
                    self.close_session_keep_clone(
                        SessionCursor { project_idx, session_idx },
                        window,
                        cx,
                    );
                }
                PendingAction::RequestDiscardSession { project_idx, session_idx } => {
                    // Arm the inline confirmation gate. The sidebar row will
                    // render Confirm/Cancel buttons on the next frame.
                    self.confirming_discard = Some(SessionCursor { project_idx, session_idx });
                    cx.notify();
                }
                PendingAction::CancelDiscard => {
                    self.confirming_discard = None;
                    cx.notify();
                }
                PendingAction::DiscardSession { project_idx, session_idx } => {
                    self.confirming_discard = None;
                    self.remove_session(
                        SessionCursor { project_idx, session_idx },
                        window,
                        cx,
                    );
                }
                PendingAction::SelectSession { project_idx, session_idx } => {
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
                }
            }
        }

        // Update session statuses from PTY state.
        // Any attached session (Running, Idle, AwaitingInput, ResponseReady)
        // can transition to Done when its PTY actually exits. Done and
        // Suspended sessions are already terminal/attached-less and are
        // skipped.
        let mut state_dirty = false;
        for project in &mut self.projects {
            for session in &mut project.sessions {
                if matches!(
                    session.status,
                    SessionStatus::Done | SessionStatus::Suspended
                ) {
                    continue;
                }
                let Some(tv) = session.terminal_view.as_ref() else { continue; };
                if tv.read(cx).has_exited() {
                    session.status = SessionStatus::Done;
                    session.last_active = std::time::SystemTime::now();
                    state_dirty = true;
                }
            }
        }
        if state_dirty {
            self.save_state();
        }

        // Build sidebar items: for each project, a header then its sessions
        let mut sidebar_items: Vec<AnyElement> = Vec::new();
        let active_cursor = self.active;

        for (p_idx, project) in self.projects.iter().enumerate() {
            let project_name = project.name.clone();
            // Project header
            sidebar_items.push(
                div()
                    .id(SharedString::from(format!("project-{p_idx}")))
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(rgb(0x11111b))
                    .border_b_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(0x6c7086))
                                    .child("▾"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(0xcdd6f4))
                                    .child(project_name),
                            ),
                    )
                    .child(
                        // New session button
                        div()
                            .id(SharedString::from(format!("new-session-{p_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .text_size(px(14.0))
                            .text_color(rgb(0x6c7086))
                            .hover(|s| s.text_color(rgb(0xa6e3a1)))
                            .child("+")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action = Some(PendingAction::AddSessionToProject(p_idx));
                                cx.notify();
                            })),
                    )
                    .child(
                        // Remove project button
                        div()
                            .id(SharedString::from(format!("remove-project-{p_idx}")))
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(0x45475a))
                            .hover(|s| s.text_color(rgb(0xf38ba8)))
                            .child("✕")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action = Some(PendingAction::RemoveProject(p_idx));
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );

            // Loading placeholders (sessions mid-clone)
            for loading in &project.loading_sessions {
                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("loading-{}", loading.id)))
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(5.0))
                        .bg(rgb(0x181825))
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_row()
                                .gap(px(6.0))
                                .items_center()
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(0xf9e2af)) // yellow
                                        .child("◐"),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x9399b2))
                                        .child(loading.label.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(0x585b70))
                                        .child("Cloning…"),
                                ),
                        )
                        .into_any_element(),
                );
            }

            // Sessions under this project
            for (s_idx, session) in project.sessions.iter().enumerate() {
                let is_active = active_cursor
                    .map(|c| c.project_idx == p_idx && c.session_idx == s_idx)
                    .unwrap_or(false);
                let is_suspended = session.status == SessionStatus::Suspended;
                let status_color = session.status.color();
                let status_icon = session.status.icon();
                // Title only exists if a PTY is attached; for Suspended
                // sessions we always fall back to the stored label.
                let label = session
                    .terminal_view
                    .as_ref()
                    .and_then(|tv| tv.read(cx).title())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| session.label.clone());
                let elapsed = session.elapsed_display();
                let is_confirming = self.confirming_discard
                    == Some(SessionCursor { project_idx: p_idx, session_idx: s_idx });

                let label_color = if is_suspended {
                    rgb(0x6c7086) // greyed out for Suspended
                } else if is_active {
                    rgb(0xcdd6f4)
                } else {
                    rgb(0x9399b2)
                };

                let row_bg = if is_confirming {
                    rgb(0x3b1f28) // subtle red tint while confirming discard
                } else if is_active {
                    rgb(0x313244)
                } else {
                    rgb(0x181825)
                };

                let mut row = div()
                    .id(SharedString::from(format!("session-{p_idx}-{s_idx}")))
                    .pl(px(24.0))
                    .pr(px(12.0))
                    .py(px(5.0))
                    .bg(row_bg)
                    .hover(|s| s.bg(rgb(0x313244)))
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .items_center()
                    .justify_between()
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        this.pending_action = Some(PendingAction::SelectSession {
                            project_idx: p_idx,
                            session_idx: s_idx,
                        });
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(status_color))
                                    .child(status_icon.to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(label_color)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(0x585b70))
                                    .min_w(px(60.0))
                                    .child(elapsed),
                            ),
                    );

                if is_confirming {
                    // Replace the normal buttons with a two-button confirm
                    // prompt: Discard (destructive) + Cancel.
                    row = row.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .items_center()
                            .child(
                                div()
                                    .id(SharedString::from(format!("confirm-discard-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(3.0))
                                    .bg(rgb(0x45475a))
                                    .text_size(px(10.0))
                                    .text_color(rgb(0xf38ba8))
                                    .hover(|s| s.bg(rgb(0x58303a)))
                                    .child("Discard")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(PendingAction::DiscardSession {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("cancel-discard-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(3.0))
                                    .text_size(px(10.0))
                                    .text_color(rgb(0x9399b2))
                                    .hover(|s| s.text_color(rgb(0xcdd6f4)))
                                    .child("Cancel")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(PendingAction::CancelDiscard);
                                        cx.notify();
                                    })),
                            ),
                    );
                } else {
                    // Normal state: a Close button (keeps clone) and a
                    // Discard button (opens the confirmation prompt).
                    row = row.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(2.0))
                            .items_center()
                            .child(
                                div()
                                    .id(SharedString::from(format!("close-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x45475a))
                                    .hover(|s| s.text_color(rgb(0x89b4fa)))
                                    .child("✕")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(PendingAction::CloseSessionKeepClone {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("discard-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x45475a))
                                    .hover(|s| s.text_color(rgb(0xf38ba8)))
                                    .child("🗑")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(PendingAction::RequestDiscardSession {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        });
                                        cx.notify();
                                    })),
                            ),
                    );
                }

                sidebar_items.push(row.into_any_element());
            }
        }

        // Status summary
        let total_projects = self.projects.len();
        let total_sessions: usize = self.projects.iter().map(|p| p.sessions.len()).sum();
        let running: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::Running)
            .count();
        let awaiting: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::AwaitingInput)
            .count();
        let response_ready: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::ResponseReady)
            .count();

        let fps = self.active_session()
            .and_then(|s| s.terminal_view.as_ref())
            .map(|tv| tv.read(cx).current_fps)
            .unwrap_or(0);

        let active_is_done = self.active_session()
            .map(|s| s.status == SessionStatus::Done)
            .unwrap_or(false);

        let sidebar_w = self.sidebar_width;
        let is_resizing = self.sidebar_resizing;

        // Outer non-flex container that hosts the flex row AND the drag overlay.
        // Keeping the overlay OUTSIDE the flex container ensures Taffy's layout
        // engine doesn't try to allocate flex space to an absolutely-positioned element.
        let flex_row = div()
            .id("app-root")
            .flex()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                // Sidebar
                div()
                    .w(px(sidebar_w))
                    .flex_shrink_0()
                    .h_full()
                    .bg(rgb(0x181825))
                    .border_r_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_col()
                    // Header
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(10.0))
                            .border_b_1()
                            .border_color(rgb(0x313244))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Allele"),
                            )
                            .child(
                                // "Open project" button
                                div()
                                    .id("new-project-btn")
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .text_size(px(16.0))
                                    .text_color(rgb(0x6c7086))
                                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xa6e3a1)))
                                    .child("+")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        this.open_folder_picker(cx);
                                    })),
                            ),
                    )
                    // Session list
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .children(sidebar_items),
                    )
                    // Status bar — attention summary lives here.
                    // Counts are rendered as coloured children so the user
                    // can glance at them and know which attention buckets
                    // have something in them.
                    .child({
                        let mut bar = div()
                            .px(px(12.0))
                            .py(px(8.0))
                            .border_t_1()
                            .border_color(rgb(0x313244))
                            .text_size(px(10.0))
                            .text_color(rgb(0x6c7086))
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .items_center()
                            .child(format!(
                                "{total_projects}p · {total_sessions}s · {running} running · {fps} fps"
                            ));

                        if awaiting > 0 {
                            bar = bar.child(
                                div()
                                    .text_color(rgb(SessionStatus::AwaitingInput.color()))
                                    .child(format!("⚠ {awaiting} need input")),
                            );
                        }
                        if response_ready > 0 {
                            bar = bar.child(
                                div()
                                    .text_color(rgb(SessionStatus::ResponseReady.color()))
                                    .child(format!("★ {response_ready} ready")),
                            );
                        }
                        bar
                    }),
            )
            // Resize handle — 6px wide invisible hover zone over the sidebar border.
            // Sits between sidebar and main area, captures drag events.
            .child(
                div()
                    .id("sidebar-resize-handle")
                    .w(px(6.0))
                    .h_full()
                    .cursor_col_resize()
                    .hover(|s| s.bg(rgb(0x45475a)))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                        this.sidebar_resizing = true;
                        cx.notify();
                    })),
            )
            .child({
                // Main terminal area with optional "session ended" overlay
                let mut main_area = div()
                    .flex_1()
                    .h_full()
                    .relative();

                if let Some(tv) = self.active_session().and_then(|s| s.terminal_view.clone()) {
                    main_area = main_area.child(tv);
                } else {
                    // Empty-state placeholder
                    main_area = main_area.child(
                        div()
                            .size_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(16.0))
                            .bg(rgb(0x1e1e2e))
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .text_color(rgb(0x6c7086))
                                    .child("No active session"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(0x45475a))
                                    .child("Click + in the sidebar to open a project"),
                            ),
                    );
                }

                if active_is_done {
                    main_area = main_area.child(
                        // "Session ended" overlay bar at bottom
                        div()
                            .absolute()
                            .bottom(px(0.0))
                            .left(px(0.0))
                            .right(px(0.0))
                            .px(px(16.0))
                            .py(px(10.0))
                            .bg(rgb(0x313244))
                            .border_t_1()
                            .border_color(rgb(0x45475a))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(0x6c7086))
                                    .child("Session ended"),
                            )
                            .child(
                                div()
                                    .id("restart-btn")
                                    .cursor_pointer()
                                    .px(px(10.0))
                                    .py(px(4.0))
                                    .rounded(px(4.0))
                                    .bg(rgb(0x45475a))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0xcdd6f4))
                                    .hover(|s| s.bg(rgb(0x585b70)))
                                    .child("New Session")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        if let Some(active) = this.active {
                                            this.pending_action = Some(PendingAction::AddSessionToProject(active.project_idx));
                                            cx.notify();
                                        }
                                    })),
                            ),
                    );
                }

                main_area
            });

        // Outer wrapper: non-flex, relative-positioned container hosting both
        // the flex row and the optional drag overlay as siblings.
        div()
            .size_full()
            .relative()
            .child(flex_row)
            .children(if is_resizing {
                vec![div()
                    .id("sidebar-drag-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor_col_resize()
                    .on_mouse_move(cx.listener(|this: &mut Self, event: &MouseMoveEvent, window, cx| {
                        let viewport_w = f32::from(window.viewport_size().width);
                        let max = (viewport_w - 100.0).max(SIDEBAR_MIN_WIDTH);
                        let new_width = f32::from(event.position.x).clamp(SIDEBAR_MIN_WIDTH, max);
                        if (new_width - this.sidebar_width).abs() > 0.5 {
                            this.sidebar_width = new_width;
                            window.refresh();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                        this.sidebar_resizing = false;
                        this.save_settings();
                        cx.notify();
                    }))
                    .into_any_element()]
            } else {
                vec![]
            })
    }
}
