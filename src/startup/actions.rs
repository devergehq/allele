//! App-scope action handlers, registered once at launch (DEV-624).
//!
//! Extracted verbatim from `fn main()`. Registering at `App` scope rather than
//! on the element tree is deliberate and load-bearing: it keeps the menu items
//! enabled regardless of what currently holds focus.
//!
//! Every handler here is a thin adapter — it queues a `PendingAction` or flips
//! a flag on `AppState` and notifies. The work itself lives in
//! `pending_actions.rs`, which is why this file is wiring rather than logic.

use gpui::{App, Context, WeakEntity, WindowHandle};
use tracing::warn;

use crate::actions::{DrawerAction, OverlayAction, SessionAction, SessionCursor, SidebarAction};
use crate::app_state::{AppState, MainTab};
use crate::commands::{
    DiscardSessionAction, GoToBrowserTab, GoToClaudeTab, GoToReaderTab, GoToTranscriptTab,
    NewSessionAction, ToggleChangesPanel,
};
use crate::pairing_alert::OpenPairingApproval;
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
    // Commands that were command-palette cases until DEV-1264.
    on_state::<GoToClaudeTab>(cx, &toggle_handle, |this, _| {
        this.main_tab = MainTab::Claude
    });
    on_state::<GoToReaderTab>(cx, &toggle_handle, |this, _| {
        this.main_tab = MainTab::Reader
    });
    on_state::<GoToTranscriptTab>(cx, &toggle_handle, |this, _| {
        this.main_tab = MainTab::Transcript
    });
    on_state::<GoToBrowserTab>(cx, &toggle_handle, |this, _| {
        if this.browser_tab_available() {
            this.main_tab = MainTab::Browser;
        }
    });
    on_state::<ToggleChangesPanel>(cx, &toggle_handle, |this, _| {
        this.pending_action = Some(SidebarAction::ToggleRightSidebar.into());
    });
    on_state::<NewSessionAction>(cx, &toggle_handle, |this, _| {
        if let Some(cursor) = this.active {
            this.pending_action =
                Some(SessionAction::OpenNewSessionModal(cursor.project_idx).into());
        }
    });
    on_state::<DiscardSessionAction>(cx, &toggle_handle, |this, _| {
        // Arms the sidebar's confirm prompt; nothing is removed until the
        // user confirms there.
        if let Some(cursor) = this.active {
            this.sidebar.visible = true;
            this.pending_action = Some(
                SessionAction::RequestDiscardSession {
                    project_idx: cursor.project_idx,
                    session_idx: cursor.session_idx,
                }
                .into(),
            );
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
            open_or_focus_settings(&handle, cx);
        }
    });
    // The pairing banner's *Review* button (DEV-1188): Settings, at the
    // Machines section's approval card.
    App::on_action::<OpenPairingApproval>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            let Some(win) = open_or_focus_settings(&handle, cx) else {
                return;
            };
            win.update(cx, |settings, _window, cx| settings.show_pairing(cx))
                .ok();
        }
    });
    App::on_action::<crate::away::AwayAction>(cx, {
        let handle = toggle_handle.clone();
        move |_, cx| {
            handle
                .update(cx, |this: &mut AppState, cx| {
                    this.pending_action = Some(SessionAction::RequestRemoteControlAll.into());
                    cx.notify();
                })
                .ok();
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

/// Register an App-scope handler for `A` that runs `apply` on `AppState` and
/// re-renders.
fn on_state<A: gpui::Action>(
    cx: &mut Context<AppState>,
    handle: &WeakEntity<AppState>,
    apply: fn(&mut AppState, &mut Context<AppState>),
) {
    let handle = handle.clone();
    App::on_action::<A>(cx, move |_, cx| {
        handle
            .update(cx, |this: &mut AppState, cx| {
                apply(this, cx);
                cx.notify();
            })
            .ok();
    });
}

/// Focus the Settings window, opening it if needed, and return its handle.
///
/// Must run from an action handler, not via `PendingAction`: the
/// pending-action dispatch runs inside render(), and cx.open_window() during
/// a render tears GPUI's element arena apart with "attempted to dereference an
/// ArenaRef after its Arena was cleared".
fn open_or_focus_settings(
    handle: &WeakEntity<AppState>,
    cx: &mut App,
) -> Option<WindowHandle<settings_window::SettingsWindowState>> {
    let strong = handle.upgrade()?;
    // The window reads its per-section state from `user_settings` itself;
    // here we only need the handle to an already-open window.
    let existing = strong.update(cx, |state: &mut AppState, _cx| state.settings_window);

    if let Some(win) = existing {
        if win
            .update(cx, |_state, window, _cx| {
                window.activate_window();
            })
            .is_ok()
        {
            return Some(win);
        }
    }

    match settings_window::open_settings_window(cx, handle.clone()) {
        Ok(new_handle) => {
            strong.update(cx, |state: &mut AppState, _cx| {
                state.settings_window = Some(new_handle);
            });
            Some(new_handle)
        }
        Err(e) => {
            warn!("Failed to open settings window: {e}");
            None
        }
    }
}
