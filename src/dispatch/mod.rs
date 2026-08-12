//! Off-thread access to the GPUI foreground thread (DEV-415).
//!
//! Groundwork for agent session dispatch. Everything that will eventually
//! drive allele from outside the process — the control socket, the MCP
//! server — runs on a background executor and has to get back onto the
//! foreground thread to touch `AppState`.
//!
//! For most of that, `Entity::update` is enough: it hands back a
//! `&mut AppState` and a `&mut Context<AppState>`, which is exactly what
//! the hook-event poller in `src/main.rs` uses to fold events into session
//! status every 250ms.
//!
//! Session creation is the exception, and it is the one thing dispatch
//! exists to do. [`AppState::add_session_to_project_with_details`] also
//! needs a `&mut Window` — it calls `apply_project_config` and
//! `cx.spawn_in(window, …)` — and `Entity::update` cannot supply one. So
//! the poller's route is structurally insufficient here, and off-thread
//! callers go through the stashed `AppState::main_window` handle instead.
//!
//! That is the whole purpose of this module: one primitive,
//! [`update_in_main_window`], so no caller has to rediscover the
//! distinction.

use gpui::{AsyncApp, Context, WeakEntity, Window};

use crate::app_state::AppState;
use crate::errors::{AlleleError, Result};

/// Run `f` on the GPUI foreground thread with main-window access.
///
/// `this` is the weak handle every `cx.spawn` closure already receives.
/// Returns `Err` if the window is gone (app shutting down) or if the handle
/// has not been stashed yet — both of which a dispatch caller must surface
/// as a failed request rather than a hang.
#[allow(dead_code)] // first consumer lands with the control socket
pub(crate) fn update_in_main_window<R>(
    this: &WeakEntity<AppState>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut AppState, &mut Window, &mut Context<AppState>) -> R,
) -> Result<R> {
    let handle = this
        .read_with(cx, |state, _cx| state.main_window)
        .map_err(|_| AlleleError::Dispatch("app is shutting down".into()))?
        .ok_or_else(|| AlleleError::Dispatch("main window handle not stashed yet".into()))?;

    handle
        .update(cx, f)
        .map_err(|e| AlleleError::Dispatch(format!("main window is gone: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{div, IntoElement, Render, TestAppContext, WindowHandle};

    /// Stand-in for `AppState`: does the one thing the real `render` does.
    struct Probe {
        handle: Option<WindowHandle<Probe>>,
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            if self.handle.is_none() {
                self.handle = window.window_handle().downcast::<Probe>();
            }
            div()
        }
    }

    /// The assumption `AppState::render` relies on to stash `main_window`:
    /// a root view can recover its own *typed* window handle mid-render.
    /// `downcast` compares `TypeId`s against the window's recorded root-view
    /// type, so this would silently yield `None` — leaving dispatch
    /// permanently unavailable — if the type were not yet registered by the
    /// time the first frame runs.
    #[gpui::test]
    fn root_view_recovers_its_typed_window_handle_during_render(cx: &mut TestAppContext) {
        let window = cx.add_window(|_window, _cx| Probe { handle: None });
        cx.run_until_parked();

        let handle = window
            .update(cx, |probe, _window, _cx| probe.handle)
            .expect("window is live");

        assert_eq!(
            handle,
            Some(window),
            "render must recover the same typed handle the window was created with"
        );
    }
}
