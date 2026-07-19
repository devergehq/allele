//! Remote-session browser modal (DEV-195).
//!
//! A list-picker overlay showing the session bundles present in the configured
//! sync store. The heavy lifting — fetching the list, resolving a pick against
//! local projects, and upserting the pulled row — lives on `AppState`
//! (`open_remote_browser` / the `Pull` event handler). This entity is purely
//! presentational: it renders the pre-fetched headers and emits which one the
//! user chose, mirroring the `NamingModal` pattern.

use gpui::*;
use std::time::SystemTime;

use crate::sync::pull::RemoteSession;
use crate::theme::theme;

/// Events emitted by the browser that `AppState` listens for.
#[derive(Debug, Clone)]
pub enum RemoteBrowserEvent {
    /// Pull the session with this id onto this machine.
    Pull { session_id: String },
    /// Dismiss the browser without pulling.
    Close,
}

impl EventEmitter<RemoteBrowserEvent> for RemoteBrowser {}

pub struct RemoteBrowser {
    sessions: Vec<RemoteSession>,
    focus_handle: FocusHandle,
}

impl RemoteBrowser {
    pub fn new(cx: &mut Context<Self>, sessions: Vec<RemoteSession>) -> Self {
        Self {
            sessions,
            focus_handle: cx.focus_handle(),
        }
    }

    fn pull(&mut self, session_id: String, cx: &mut Context<Self>) {
        cx.emit(RemoteBrowserEvent::Pull { session_id });
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(RemoteBrowserEvent::Close);
    }
}

impl Focusable for RemoteBrowser {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RemoteBrowser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div()
            .id("remote-browser-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.5))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.dismiss(cx);
                }),
            );

        let mut card = div()
            .id("remote-browser-card")
            .w(px(420.0))
            .max_h(px(460.0))
            .bg(theme().bg_base)
            .rounded(px(12.0))
            .border_1()
            .border_color(theme().border_default)
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| {
                cx.stop_propagation();
            })
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _w, cx| {
                if event.keystroke.key == "escape" {
                    this.dismiss(cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme().text_primary)
                            .child("Pull a session"),
                    )
                    .child(
                        div()
                            .id("remote-browser-close")
                            .cursor_pointer()
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .bg(theme().bg_hover)
                            .hover(|s| s.bg(theme().bg_active))
                            .text_size(px(11.0))
                            .text_color(theme().text_secondary)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.dismiss(cx);
                                }),
                            )
                            .child("Close"),
                    ),
            );

        if self.sessions.is_empty() {
            card = card.child(
                div()
                    .py(px(16.0))
                    .text_size(px(12.0))
                    .text_color(theme().text_faint)
                    .child("No sessions found in the remote store."),
            );
        }

        let mut list = div()
            .id("remote-browser-list")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(6.0));

        for (i, session) in self.sessions.iter().enumerate() {
            let session_id = session.id.clone();
            let subtitle = format!(
                "{} · rev {} · {} · {}",
                session.project_name,
                session.revision,
                session.last_writer_device,
                relative_time(session.updated_at),
            );
            list = list.child(
                div()
                    .id(SharedString::from(format!("remote-session-{i}")))
                    .cursor_pointer()
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .bg(theme().bg_raised)
                    .hover(|s| s.bg(theme().bg_hover))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.pull(session_id.clone(), cx);
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme().text_primary)
                            .child(SharedString::from(session.label.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme().text_faint)
                            .child(SharedString::from(subtitle)),
                    ),
            );
        }

        card = card.child(list);

        // Return the full-window backdrop as the root element so its
        // `absolute inset_0` centers the card over the whole window. Wrapping
        // it in another div would reparent the absolute positioning and drop
        // the modal to the bottom of the layout flow.
        backdrop.child(card)
    }
}

/// Coarse "3d ago"-style label for a bundle's last-write time.
fn relative_time(then: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(then) else {
        return "just now".to_string();
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 2 {
        "yesterday".to_string()
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else {
        format!("{}mo ago", secs / (86_400 * 30))
    }
}
