//! App-scope action handlers, registered once at launch (DEV-624).
//!
//! Extracted verbatim from `fn main()`. Registering at `App` scope rather than
//! on the element tree is deliberate and load-bearing: it keeps the menu items
//! enabled regardless of what currently holds focus.
//!
//! Every handler here is a thin adapter — it queues a `PendingAction` or flips
//! a flag on `AppState` and notifies. The work itself lives in
//! `pending_actions.rs`, which is why this file is wiring rather than logic.

use gpui::{App, Context};
use tracing::warn;

use crate::actions::{DrawerAction, OverlayAction, SessionAction, SessionCursor, SidebarAction};
use crate::app_state::{AppState, MainTab};
use crate::session::{self, SessionStatus};
use crate::{
    dispatch, settings_window, CaptureUi, CycleAttentionSession, OpenCommandPaletteAction,
    OpenFilePaletteAction, OpenScratchPadAction, OpenSearchAction, OpenSettings, Quit,
    ToggleActiveOnlyAction, ToggleDrawerAction, ToggleSidebarAction, ToggleTranscriptTabAction,
};

/// Register every app-scope action handler. Called once, from `main`.
pub(crate) fn register_all(cx: &mut Context<AppState>) {
    // App-level handlers for menu-dispatched actions. Registering
    // at App scope (not on the element tree) guarantees the
    // menu items stay enabled regardless of focus state.
    let toggle_handle = cx.entity().downgrade();

    // Quit interception — confirm before quitting when
    // sessions are still running.
    App::on_action::<Quit>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            let should_quit = handle
                .update(cx, |state: &mut AppState, cx| {
                    let active_count = state
                        .projects
                        .iter()
                        .flat_map(|p| &p.sessions)
                        .filter(|s| {
                            matches!(s.status, SessionStatus::Running | SessionStatus::Idle)
                        })
                        .count();
                    if active_count > 0 {
                        state.confirming.quit = true;
                        cx.notify();
                        false
                    } else {
                        // Writes are debounced, so up to half a
                        // second of state may not be on disk
                        // and the process is about to go away
                        // (DEV-609).
                        state.flush_persistence_blocking();
                        true
                    }
                })
                .unwrap_or(true);
            if should_quit {
                // Remove the control socket so the next run
                // binds without having to reclaim a stale
                // file. Best-effort: a crash skips this, which
                // the reclaim path in `server` handles.
                dispatch::server::cleanup();
                cx.quit();
            }
        }
    });
    App::on_action::<ToggleSidebarAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(SidebarAction::ToggleSidebar.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<ToggleActiveOnlyAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(SidebarAction::ToggleActiveOnly.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<ToggleDrawerAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<OpenScratchPadAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(OverlayAction::OpenScratchPad.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<OpenFilePaletteAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(OverlayAction::OpenFilePalette.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<OpenSearchAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(OverlayAction::OpenSearch.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<OpenCommandPaletteAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(OverlayAction::OpenCommandPalette.into());
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<ToggleTranscriptTabAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.main_tab = match this.main_tab {
                        MainTab::Transcript => MainTab::Claude,
                        _ => MainTab::Transcript,
                    };
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<CycleAttentionSession>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    // Collect all AwaitingInput sessions.
                    let mut attention: Vec<SessionCursor> = Vec::new();
                    for (p_idx, project) in this.projects.iter().enumerate() {
                        for (s_idx, session) in project.sessions.iter().enumerate() {
                            if session.status == session::SessionStatus::AwaitingInput {
                                attention.push(SessionCursor {
                                    project_idx: p_idx,
                                    session_idx: s_idx,
                                });
                            }
                        }
                    }
                    if attention.is_empty() {
                        return;
                    }
                    // Find the next one after the current active session.
                    let current_pos = this
                        .active
                        .and_then(|c| attention.iter().position(|a| *a == c))
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    let next = attention[current_pos % attention.len()];
                    this.pending_action = Some(
                        SessionAction::SelectSession {
                            project_idx: next.project_idx,
                            session_idx: next.session_idx,
                        }
                        .into(),
                    );
                    cx.notify();
                })
                .ok();
        }
    });
    App::on_action::<OpenSettings>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            // Must happen here (not via PendingAction) — the
            // pending-action dispatch runs inside render(),
            // and cx.open_window() during a render tears
            // GPUI's element arena apart with
            // "attempted to dereference an ArenaRef after
            // its Arena was cleared".
            let Some(strong) = handle.upgrade() else {
                return;
            };
            // The window reads its per-section state from
            // `user_settings` itself; here we only need the
            // handle to an already-open window.
            let existing = strong.update(cx, |state: &mut AppState, _cx| state.settings_window);

            if let Some(win) = existing {
                if win
                    .update(cx, |_state, window, _cx| {
                        window.activate_window();
                    })
                    .is_ok()
                {
                    return;
                }
            }

            let weak = handle.clone();
            match settings_window::open_settings_window(cx, weak) {
                Ok(new_handle) => {
                    strong.update(cx, |state: &mut AppState, _cx| {
                        state.settings_window = Some(new_handle);
                    });
                }
                Err(e) => {
                    warn!("Failed to open settings window: {e}");
                }
            }
        }
    });
    App::on_action::<CaptureUi>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |state: &mut AppState, cx| {
                    state.capture_ui_requested = true;
                    cx.notify();
                })
                .ok();
        }
    });
}
