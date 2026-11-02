//! A remote session's right-click menu: Interrupt and Discard on the host
//! (DEV-1168). Only opened on a row whose host granted this Mac `control`
//! (see `render_remote_session_row`), so a `view`-only Mac never sees it.
//!
//! Discard asks first, in the menu itself: the session is on another Mac, and
//! the person clicking may not be looking at it.

use gpui::*;

use crate::actions::SessionAction;
use crate::app_state::AppState;
use crate::remote_sessions::{RemoteMenu, RemoteSessionAction};
use crate::theme::theme;

impl AppState {
    /// Empty when no remote menu is open, so callers can attach it always.
    pub(crate) fn render_remote_session_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div();
        let Some(menu) = self.remote_ui.menu.clone() else {
            return root;
        };
        let item = |id: &'static str, label: String, color: Hsla| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(color)
                .cursor_pointer()
                .hover(|s| s.bg(theme().bg_hover))
                .child(label)
        };
        let caption = |text: String| {
            div()
                .px(px(14.0))
                .py(px(4.0))
                .max_w(px(260.0))
                .text_size(px(11.0))
                .text_color(theme().text_dim)
                .child(text)
        };

        let mut body = div()
            .flex()
            .flex_col()
            .min_w(px(200.0))
            .py(px(4.0))
            .bg(theme().bg_surface)
            .border_1()
            .border_color(theme().border_default)
            .rounded(px(6.0))
            .shadow_lg();

        if menu.confirming_discard {
            body = body
                .child(caption(format!(
                    "Discard \u{201c}{}\u{201d} on {}? Its work is committed and its branch \
                     archived there, then the workspace is removed.",
                    menu.label, menu.host_name
                )))
                .child(
                    item(
                        "remote-ctx-discard-confirm",
                        "Discard".to_string(),
                        theme().danger,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let menu = menu.clone();
                            move |this: &mut Self, _e, _w, cx| {
                                cx.stop_propagation();
                                this.remote_ui.menu = None;
                                this.queue_remote(discard(&menu), cx);
                            }
                        }),
                    ),
                )
                .child(
                    item(
                        "remote-ctx-discard-cancel",
                        "Cancel".to_string(),
                        theme().text_primary,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _e, _w, cx| {
                            cx.stop_propagation();
                            this.remote_ui.menu = None;
                            cx.notify();
                        }),
                    ),
                );
        } else {
            body = body
                .child(caption(format!("On {}", menu.host_name)))
                .child(
                    item(
                        "remote-ctx-interrupt",
                        "Interrupt".to_string(),
                        theme().text_primary,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let menu = menu.clone();
                            move |this: &mut Self, _e, _w, cx| {
                                cx.stop_propagation();
                                this.remote_ui.menu = None;
                                this.queue_remote(interrupt(&menu), cx);
                            }
                        }),
                    ),
                )
                .child(div().w_full().h(px(1.0)).my(px(4.0)).bg(theme().bg_raised))
                .child(
                    item("remote-ctx-discard", "Discard…".to_string(), theme().danger)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this: &mut Self, _e, _w, cx| {
                                cx.stop_propagation();
                                if let Some(menu) = this.remote_ui.menu.as_mut() {
                                    menu.confirming_discard = true;
                                }
                                cx.notify();
                            }),
                        ),
                );
        }

        root.child(self.dismissable_popover(
            menu.position,
            body,
            |this: &mut Self, cx| {
                this.remote_ui.menu = None;
                cx.notify();
            },
            cx,
        ))
    }

    fn queue_remote(&mut self, action: RemoteSessionAction, cx: &mut Context<Self>) {
        self.pending_action = Some(SessionAction::Remote(action).into());
        cx.notify();
    }
}

fn interrupt(menu: &RemoteMenu) -> RemoteSessionAction {
    RemoteSessionAction::Interrupt {
        key: menu.key.clone(),
        label: menu.label.clone(),
        host_name: menu.host_name.clone(),
    }
}

fn discard(menu: &RemoteMenu) -> RemoteSessionAction {
    RemoteSessionAction::Discard {
        key: menu.key.clone(),
        label: menu.label.clone(),
        host_name: menu.host_name.clone(),
    }
}
