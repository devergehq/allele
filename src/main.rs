mod terminal;
mod sidebar;
mod clone;
mod project;
mod session;
mod settings;
mod state;

use gpui::*;
use project::Project;
use session::{Session, SessionStatus};
use settings::{ProjectSave, Settings};
use terminal::{ShellCommand, TerminalEvent, TerminalView};
use terminal::pty_terminal::PtyTerminal;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
enum PendingAction {
    NewSessionInActiveProject,
    CloseActiveSession,
    FocusActive,
    OpenProjectAtPath(PathBuf),
    AddSessionToProject(usize), // project index
    RemoveProject(usize),
    RemoveSession { project_idx: usize, session_idx: usize },
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
        };
        settings.save();
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
        let command = self.claude_path.as_ref().map(|p| ShellCommand::new(p.clone()));
        let display_label = if command.is_some() {
            format!("Claude {session_count}")
        } else {
            format!("Shell {session_count}")
        };

        // Add a loading placeholder immediately so the user sees feedback
        project.loading_sessions.push(project::LoadingSession {
            id: session_id.clone(),
            label: display_label.clone(),
        });
        cx.notify();

        // Spawn the clone on a background task, then finish on the main thread
        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        let session_id_for_task = session_id.clone();
        let display_label_for_task = display_label.clone();

        cx.spawn_in(window, async move |this, cx| {
            // Run the clonefile() syscall on the background executor
            let clone_result = cx
                .background_executor()
                .spawn(async move {
                    clone::create_session_clone(&source_for_task, &project_name_for_task, &session_id_for_task)
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
                    }
                }).detach();

                let session = Session::new(display_label_for_task, terminal_view).with_clone(clone_path);
                let Some(project) = this.projects.get_mut(project_idx) else { return; };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                this.active = Some(SessionCursor { project_idx, session_idx });
                cx.notify();
            });
        })
        .detach();
    }

    /// Remove a session at the given cursor. The session is immediately
    /// removed from the sidebar and replaced with a "Deleting…" placeholder,
    /// while the APFS clone is deleted on a background task.
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
        // Drop the Session — this frees the terminal_view entity, which in turn
        // kills the PTY via the Drop impl on PtyTerminal.
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
        // Log to ~/.config/cc-multiplex/crash.log
        if let Some(home) = dirs::home_dir() {
            let log_dir = home.join(".config").join("cc-multiplex");
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

            eprintln!("\n*** cc-multiplex crashed ***");
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

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("CC Multiplex".into()),
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
                        };
                        settings.save();
                    }).detach();

                    // Rehydrate projects from settings (without sessions — those are runtime-only)
                    let projects: Vec<Project> = settings_for_window.projects.iter().map(|p| {
                        let mut proj = Project::new(p.name.clone(), p.source_path.clone());
                        proj.id = p.id.clone();
                        proj
                    }).collect();

                    AppState {
                        projects,
                        active: None,
                        claude_path: claude_path_clone,
                        pending_action: None,
                        sidebar_width: settings_for_window.sidebar_width
                            .max(SIDEBAR_MIN_WIDTH),
                        sidebar_resizing: false,
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
                    if let Some(active) = self.active {
                        self.remove_session(active, window, cx);
                    }
                }
                PendingAction::FocusActive => {
                    if let Some(session) = self.active_session() {
                        let fh = session.terminal_view.read(cx).focus_handle.clone();
                        fh.focus(window, cx);
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
                PendingAction::RemoveSession { project_idx, session_idx } => {
                    self.remove_session(SessionCursor { project_idx, session_idx }, window, cx);
                }
                PendingAction::SelectSession { project_idx, session_idx } => {
                    self.active = Some(SessionCursor { project_idx, session_idx });
                    if let Some(session) = self.active_session() {
                        let fh = session.terminal_view.read(cx).focus_handle.clone();
                        fh.focus(window, cx);
                    }
                }
            }
        }

        // Update session statuses from PTY state
        for project in &mut self.projects {
            for session in &mut project.sessions {
                if session.status == SessionStatus::Running
                    && session.terminal_view.read(cx).has_exited()
                {
                    session.status = SessionStatus::Done;
                }
            }
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
                let status_color = session.status.color();
                let status_icon = session.status.icon();
                let label = session
                    .terminal_view
                    .read(cx)
                    .title()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| session.label.clone());
                let elapsed = session.elapsed_display();

                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("session-{p_idx}-{s_idx}")))
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(5.0))
                        .bg(if is_active { rgb(0x313244) } else { rgb(0x181825) })
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
                                        .text_color(if is_active { rgb(0xcdd6f4) } else { rgb(0x9399b2) })
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(0x585b70))
                                        .min_w(px(60.0))
                                        .child(elapsed),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("close-{p_idx}-{s_idx}")))
                                .cursor_pointer()
                                .px(px(4.0))
                                .text_size(px(11.0))
                                .text_color(rgb(0x45475a))
                                .hover(|s| s.text_color(rgb(0xf38ba8)))
                                .child("✕")
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                    // Stop the row's click handler from overriding us
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::RemoveSession {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    });
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }

        // Status summary
        let total_projects = self.projects.len();
        let total_sessions: usize = self.projects.iter().map(|p| p.sessions.len()).sum();
        let running: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::Running)
            .count();

        let fps = self.active_session()
            .map(|s| s.terminal_view.read(cx).current_fps)
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
                                    .child("CC Multiplex"),
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
                    // Status bar
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(8.0))
                            .border_t_1()
                            .border_color(rgb(0x313244))
                            .text_size(px(10.0))
                            .text_color(rgb(0x6c7086))
                            .child(format!("{total_projects} projects · {total_sessions} sessions · {running} running · {fps} fps")),
                    ),
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

                if let Some(session) = self.active_session() {
                    main_area = main_area.child(session.terminal_view.clone());
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
