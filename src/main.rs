mod terminal;
mod sidebar;
mod clone;
mod session;
mod state;

use gpui::*;
use session::{Session, SessionStatus};
use terminal::{ShellCommand, TerminalView};
use terminal::pty_terminal::PtyTerminal;

struct AppState {
    sessions: Vec<Session>,
    active_session_idx: usize,
    claude_path: Option<String>,
}

impl AppState {
    fn active_session(&self) -> Option<&Session> {
        self.sessions.get(self.active_session_idx)
    }

    fn add_session(&mut self, label: String, window: &mut Window, cx: &mut Context<Self>) {
        let command = self.claude_path.as_ref().map(|p| ShellCommand::new(p.clone()));
        let display_label = if command.is_some() {
            format!("Claude {}", self.sessions.len() + 1)
        } else {
            format!("Shell {}", self.sessions.len() + 1)
        };

        let terminal_view = cx.new(|cx| {
            TerminalView::new(window, cx, command, None)
        });

        let session = Session::new(display_label, terminal_view);
        self.sessions.push(session);
        self.active_session_idx = self.sessions.len() - 1;
        cx.notify();
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
                // Create the first session
                let command = claude_path_clone.as_ref().map(|p| ShellCommand::new(p.clone()));
                let label = if command.is_some() {
                    "Claude 1".to_string()
                } else {
                    "Shell 1".to_string()
                };

                let terminal_view = cx.new(|cx| {
                    TerminalView::new(window, cx, command, None)
                });

                let first_session = Session::new(label, terminal_view);

                cx.new(|_cx| AppState {
                    sessions: vec![first_session],
                    active_session_idx: 0,
                    claude_path: claude_path_clone,
                })
            },
        )
        .unwrap();
    });
}

impl Render for AppState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Build sidebar session list
        let mut session_items: Vec<AnyElement> = Vec::new();

        for (idx, session) in self.sessions.iter().enumerate() {
            let is_active = idx == self.active_session_idx;
            let status_color = session.status.color();
            let status_icon = session.status.icon();
            let label = session.label.clone();

            session_items.push(
                div()
                    .id(SharedString::from(format!("session-{idx}")))
                    .px(px(12.0))
                    .py(px(6.0))
                    .cursor_pointer()
                    .bg(if is_active { rgb(0x313244) } else { rgb(0x181825) })
                    .hover(|s| s.bg(rgb(0x313244)))
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
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
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        this.active_session_idx = idx;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }

        // Status summary
        let running = self.sessions.iter().filter(|s| s.status == SessionStatus::Running).count();
        let done = self.sessions.iter().filter(|s| s.status == SessionStatus::Done).count();
        let total = self.sessions.len();

        div()
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
                            // New session button
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
                                        this.add_session("New".to_string(), window, cx);
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
                    // Status bar at bottom of sidebar
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(8.0))
                            .border_t_1()
                            .border_color(rgb(0x313244))
                            .text_size(px(10.0))
                            .text_color(rgb(0x6c7086))
                            .child(format!("{total} sessions · {running} running · {done} done")),
                    ),
            )
            .child(
                // Main terminal area — show active session
                div()
                    .flex_1()
                    .h_full()
                    .children(
                        self.active_session()
                            .map(|s| s.terminal_view.clone().into_any_element())
                    ),
            )
    }
}
