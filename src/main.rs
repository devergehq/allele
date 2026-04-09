mod terminal;
mod sidebar;
mod clone;
mod session;
mod state;

use gpui::*;
use terminal::TerminalView;

struct AppState {
    terminal_view: Entity<TerminalView>,
}

fn main() {
    let application = Application::new();

    application.run(move |cx: &mut App| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("CC Multiplex".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(800.0), px(600.0))),
                ..Default::default()
            },
            |window, cx| {
                let terminal_view = cx.new(|cx| TerminalView::new(window, cx));
                cx.new(|_cx| AppState { terminal_view })
            },
        )
        .unwrap();
    });
}

impl Render for AppState {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(
                        div()
                            .p(px(12.0))
                            .child("CC Multiplex"),
                    ),
            )
            .child(
                // Main terminal area
                div()
                    .flex_1()
                    .h_full()
                    .child(self.terminal_view.clone()),
            )
    }
}
