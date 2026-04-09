mod terminal;
mod sidebar;
mod clone;
mod session;
mod state;

use gpui::*;
use session::{Session, SessionStatus};
use terminal::{ShellCommand, TerminalEvent, TerminalView};
use terminal::pty_terminal::PtyTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug)]
enum PendingAction {
    NewSession,
    CloseSession,
    FocusActive,
}

struct AppState {
    sessions: Vec<Session>,
    active_session_idx: usize,
    claude_path: Option<String>,
    next_session_num: usize,
    pending_action: Option<PendingAction>,
}

impl AppState {
    fn active_session(&self) -> Option<&Session> {
        self.sessions.get(self.active_session_idx)
    }

    fn add_session(
        &mut self,
        working_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = self.claude_path.as_ref().map(|p| ShellCommand::new(p.clone()));
        let num = self.next_session_num;
        self.next_session_num += 1;
        let display_label = if command.is_some() {
            format!("Claude {num}")
        } else {
            format!("Shell {num}")
        };

        let terminal_view = cx.new(|cx| {
            TerminalView::new(window, cx, command, working_dir)
        });

        // Subscribe to terminal events (Cmd shortcuts)
        // Note: subscribe doesn't give us Window access, so we queue actions
        // and handle window-dependent ops (like focus) in the next render
        cx.subscribe(&terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
            match event {
                TerminalEvent::NewSession => {
                    this.pending_action = Some(PendingAction::NewSession);
                    cx.notify();
                }
                TerminalEvent::CloseSession => {
                    this.pending_action = Some(PendingAction::CloseSession);
                    cx.notify();
                }
                TerminalEvent::SwitchSession(target) => {
                    let target = *target;
                    if target < this.sessions.len() {
                        this.active_session_idx = target;
                        this.pending_action = Some(PendingAction::FocusActive);
                        cx.notify();
                    }
                }
            }
        }).detach();

        let session = Session::new(display_label, terminal_view);
        self.sessions.push(session);
        self.active_session_idx = self.sessions.len() - 1;
        cx.notify();
    }

    fn close_session(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.sessions.is_empty() || idx >= self.sessions.len() {
            return;
        }
        self.sessions.remove(idx);
        // Adjust active index
        if self.sessions.is_empty() {
            self.active_session_idx = 0;
        } else if self.active_session_idx >= self.sessions.len() {
            self.active_session_idx = self.sessions.len() - 1;
        } else if idx < self.active_session_idx {
            self.active_session_idx -= 1;
        }
        // Re-focus the now-active session's terminal
        if let Some(session) = self.sessions.get(self.active_session_idx) {
            let fh = session.terminal_view.read(cx).focus_handle.clone();
            fh.focus(window, cx);
        }
        cx.notify();
    }

    fn add_cloned_session(
        &mut self,
        project_path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace_name = format!("workspace-{}", self.next_session_num);

        match clone::create_clone(project_path, &workspace_name) {
            Ok(clone_path) => {
                eprintln!("Created APFS clone at: {}", clone_path.display());
                self.add_session(Some(clone_path), window, cx);
            }
            Err(e) => {
                eprintln!("Failed to create APFS clone: {e}");
                self.add_session(Some(project_path.to_path_buf()), window, cx);
            }
        }
    }
}

fn main() {
    let application = Application::new();

    application.run(move |cx: &mut App| {
        let claude_path = PtyTerminal::find_claude()
            .map(|p| p.to_string_lossy().to_string());

        if let Some(ref path) = claude_path {
            eprintln!("Found Claude Code at: {path}");
        } else {
            eprintln!("Claude Code not found — falling back to default shell");
        }

        let claude_path_clone = claude_path.clone();

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("CC Multiplex".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(800.0), px(600.0))),
                ..Default::default()
            },
            move |window, cx| {
                let command = claude_path_clone.as_ref().map(|p| ShellCommand::new(p.clone()));
                let label = if command.is_some() {
                    "Claude 1".to_string()
                } else {
                    "Shell 1".to_string()
                };

                let terminal_view = cx.new(|cx| {
                    TerminalView::new(window, cx, command, None)
                });

                let first_session = Session::new(label, terminal_view.clone());
                let tv_for_sub = terminal_view.clone();

                cx.new(|cx: &mut Context<AppState>| {
                    // Subscribe to first session's terminal events
                    cx.subscribe(&tv_for_sub, |this: &mut AppState, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<AppState>| {
                        match event {
                            TerminalEvent::NewSession => {
                                this.pending_action = Some(PendingAction::NewSession);
                                cx.notify();
                            }
                            TerminalEvent::CloseSession => {
                                this.pending_action = Some(PendingAction::CloseSession);
                                cx.notify();
                            }
                            TerminalEvent::SwitchSession(target) => {
                                let target = *target;
                                if target < this.sessions.len() {
                                    this.active_session_idx = target;
                                    this.pending_action = Some(PendingAction::FocusActive);
                                    cx.notify();
                                }
                            }
                        }
                    }).detach();

                    AppState {
                        sessions: vec![first_session],
                        active_session_idx: 0,
                        claude_path: claude_path_clone,
                        next_session_num: 2,
                        pending_action: None,
                    }
                })
            },
        )
        .unwrap();
    });
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process pending actions from keyboard shortcuts
        if let Some(action) = self.pending_action.take() {
            match action {
                PendingAction::NewSession => {
                    self.add_session(None, window, cx);
                }
                PendingAction::CloseSession => {
                    let idx = self.active_session_idx;
                    self.close_session(idx, window, cx);
                }
                PendingAction::FocusActive => {
                    if let Some(session) = self.sessions.get(self.active_session_idx) {
                        let fh = session.terminal_view.read(cx).focus_handle.clone();
                        fh.focus(window, cx);
                    }
                }
            }
        }

        // Update session statuses from PTY state
        for session in &mut self.sessions {
            if session.status == SessionStatus::Running {
                if session.terminal_view.read(cx).has_exited() {
                    session.status = SessionStatus::Done;
                }
            }
        }

        // Build sidebar session list
        let mut session_items: Vec<AnyElement> = Vec::new();

        for (idx, session) in self.sessions.iter().enumerate() {
            let is_active = idx == self.active_session_idx;
            let status_color = session.status.color();
            let status_icon = session.status.icon();
            let label = session.label.clone();
            let elapsed = session.elapsed_display();
            let is_done = session.status == SessionStatus::Done;

            session_items.push(
                div()
                    .id(SharedString::from(format!("session-{idx}")))
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(if is_active { rgb(0x313244) } else { rgb(0x181825) })
                    .hover(|s| s.bg(rgb(0x313244)))
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .items_center()
                    .justify_between()
                    .child(
                        // Left: clickable area for session selection
                        div()
                            .id(SharedString::from(format!("select-{idx}")))
                            .flex_1()
                            .cursor_pointer()
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
                                    .min_w(px(45.0))  // Fixed min width to prevent jumping
                                    .child(elapsed),
                            )
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, window, cx| {
                                this.active_session_idx = idx;
                                if let Some(session) = this.sessions.get(idx) {
                                    let fh = session.terminal_view.read(cx).focus_handle.clone();
                                    fh.focus(window, cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        // Right: close button (separate click target)
                        div()
                            .id(SharedString::from(format!("close-{idx}")))
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(0x45475a))
                            .hover(|s| s.text_color(rgb(0xf38ba8)))
                            .child("✕")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, window, cx| {
                                this.close_session(idx, window, cx);
                            })),
                    )
                    .into_any_element(),
            );
        }

        // Status summary
        let running = self.sessions.iter().filter(|s| s.status == SessionStatus::Running).count();
        let done = self.sessions.iter().filter(|s| s.status == SessionStatus::Done).count();
        let total = self.sessions.len();

        let fps = self.active_session()
            .map(|s| s.terminal_view.read(cx).current_fps)
            .unwrap_or(0);

        // Check if active session has ended (for overlay)
        let active_is_done = self.active_session()
            .map(|s| s.status == SessionStatus::Done)
            .unwrap_or(false);

        div()
            .id("app-root")
            .flex()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                // Sidebar
                div()
                    .w(px(240.0))
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
                                div()
                                    .id("new-session-btn")
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .text_size(px(16.0))
                                    .text_color(rgb(0x6c7086))
                                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xa6e3a1)))
                                    .child("+")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, window, cx| {
                                        this.add_session(None, window, cx);
                                    })),
                            ),
                    )
                    // Session list
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .children(session_items),
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
                            .child(format!("{total} sessions · {running} running · {done} done · {fps} fps")),
                    ),
            )
            .child({
                // Main terminal area with optional "session ended" overlay
                let mut main_area = div()
                    .flex_1()
                    .h_full()
                    .relative();

                if let Some(session) = self.active_session() {
                    main_area = main_area.child(session.terminal_view.clone());
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
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, window, cx| {
                                        this.add_session(None, window, cx);
                                    })),
                            ),
                    );
                }

                main_area
            })
    }
}
