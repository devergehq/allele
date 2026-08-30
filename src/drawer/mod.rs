//! Drawer — terminal drawer tabs, lifecycle, and rendering.
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 5.
//! Owns the per-session drawer lifecycle helpers (spawn / ensure / focus) and
//! the render helpers used by the top-level `Render for AppState` impl.
//!
//! The drawer is the bottom panel that hosts one or more `TerminalView`
//! entities per session. It has its own tab strip, resize handle, and a
//! full-viewport drag overlay that services resize drags. All drawer-related
//! UI lives here; the caller just asks for the list of elements to drop into
//! the content column plus the optional drag overlay sibling.

use std::path::Path;

use crate::icon::{icon, name as icons};
use crate::theme::theme;
use gpui::*;

use crate::actions::{DrawerAction, SessionCursor, SettingsAction};
use crate::app_state::{AppState, DRAWER_MIN_HEIGHT, MAIN_AREA_MIN_HEIGHT};
use crate::session::DrawerTab;
use crate::terminal::{
    clamp_font_size, ShellCommand, TerminalEvent, TerminalView, DEFAULT_FONT_SIZE,
};
use crate::SimpleTooltip;

impl AppState {
    /// Spawn one drawer terminal tab in the given session with an optional
    /// pre-chosen name and optional shell command. Default name is
    /// "Terminal N" where N is 1-based; default command drops into the
    /// user's shell.
    ///
    /// `replay` is the project-configured command line for this tab. It is
    /// pushed into the fresh shell's stdin — the shell executes it once its rc
    /// files finish loading and stays interactive afterwards — and retained on
    /// the tab so the same tab can be parked and brought back running the same
    /// thing (DEV-445).
    pub(crate) fn spawn_drawer_tab(
        &mut self,
        cursor: SessionCursor,
        name: Option<String>,
        command: Option<ShellCommand>,
        replay: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_ref = self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx));
        let working_dir = session_ref.and_then(|s| s.clone_path.clone());
        let port = session_ref.and_then(|s| s.allocated_port);

        // The project's declared environment (DEV-485). Resolved per spawn so
        // an edit to allele.json or project settings is picked up by the next
        // tab without restarting the app, matching how `replay` is handled.
        let project_env = self
            .projects
            .get(cursor.project_idx)
            .map(|p| crate::config::ProjectEnv::resolve(&p.source_path, &p.settings))
            .unwrap_or_default();
        let inherited_path = std::env::var("PATH").ok();
        let extra_env = project_env.materialise(
            port,
            working_dir.as_deref().unwrap_or_else(|| Path::new(".")),
            inherited_path.as_deref(),
        );

        // Blank commands are configuration noise, not something to replay — a
        // tab declared with an empty command is just a named shell, and giving
        // it a `Some("")` replay would make it look parkable when there is
        // nothing to bring back.
        let replay = replay.filter(|r| !r.trim().is_empty());

        // `replay` is stored UNSUBSTITUTED and re-substituted on every spawn.
        // Storing the substituted text instead would bake this materialisation's
        // `{{unique_port}}` into the tab, and a session's port is re-allocated on
        // every materialisation — so an unpark would replay a claim on a port
        // that may since have gone to somebody else, and fail as a dead server
        // in a tab that looks restored.
        let substituted = replay.as_ref().map(|raw| {
            crate::config::substitute(
                raw,
                port,
                working_dir.as_deref().unwrap_or_else(|| Path::new(".")),
            )
        });
        let initial_font_size = self.user_settings.font_size;
        let drawer_tv = cx.new(|cx| {
            TerminalView::new(
                window,
                cx,
                command,
                working_dir,
                initial_font_size,
                extra_env,
            )
        });
        cx.subscribe(
            &drawer_tv,
            |this: &mut Self,
             _tv: Entity<TerminalView>,
             event: &TerminalEvent,
             cx: &mut Context<Self>| {
                match event {
                    TerminalEvent::ToggleDrawer => {
                        this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                        cx.notify();
                    }
                    TerminalEvent::PrevSession | TerminalEvent::NextSession => {
                        if let Some(cursor) = this.active {
                            if let Some(session) = this
                                .projects
                                .get(cursor.project_idx)
                                .and_then(|p| p.sessions.get(cursor.session_idx))
                            {
                                let len = session.drawer_tabs.len();
                                if len > 1 {
                                    let cur = session.drawer_active_tab;
                                    let target = match event {
                                        TerminalEvent::NextSession => (cur + 1) % len,
                                        _ => (cur + len - 1) % len,
                                    };
                                    this.pending_action =
                                        Some(DrawerAction::SwitchDrawerTab(target).into());
                                    cx.notify();
                                }
                            }
                        }
                    }
                    TerminalEvent::AdjustFontSize(delta) => {
                        let new_size = clamp_font_size(this.user_settings.font_size + delta);
                        this.pending_action = Some(SettingsAction::UpdateFontSize(new_size).into());
                        cx.notify();
                    }
                    TerminalEvent::ResetFontSize => {
                        this.pending_action =
                            Some(SettingsAction::UpdateFontSize(DEFAULT_FONT_SIZE).into());
                        cx.notify();
                    }
                    _ => {}
                }
            },
        )
        .detach();

        let user_typed = drawer_tv.read(cx).user_typed_handle();

        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            let tab_name =
                name.unwrap_or_else(|| format!("Terminal {}", session.drawer_tabs.len() + 1));
            session.drawer_tabs.push(DrawerTab {
                view: drawer_tv.clone(),
                name: tab_name,
                replay: replay.clone(),
                user_typed,
            });
        } else {
            // No session to attach to — drop the terminal rather than leaking
            // a PTY nothing owns.
            return;
        }

        if let Some(cmd) = substituted {
            let mut line = cmd.into_bytes();
            line.push(b'\n');
            drawer_tv.read(cx).send_input(&line);
        }
    }

    /// Materialise drawer tabs for a session that has none live. Restores from
    /// `parked_drawer_tabs` if present — replaying each tab's command — else
    /// creates one default "Terminal 1" tab.
    ///
    /// This is the unpark path as well as the first-open path: a drawer parked
    /// by the idle reaper (DEV-445) and one rehydrated from `state.json` are
    /// the same state, and come back the same way.
    pub(crate) fn ensure_drawer_tabs(
        &mut self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (needs_default, parked, active_tab) = {
            let session = self
                .projects
                .get(cursor.project_idx)
                .and_then(|p| p.sessions.get(cursor.session_idx));
            match session {
                // Tabs are already live. Historically this returned without
                // touching the parked list, so a session that ended up with
                // both kept both forever — and the parked copy persisted to
                // `state.json` and came back as phantom tabs. Clearing here
                // makes "live wins" an enforced outcome rather than a
                // convention (see the invariant on `parked_drawer_tabs`).
                Some(s) if !s.drawer_tabs.is_empty() => (false, Vec::new(), 0),
                Some(s) => {
                    if s.parked_drawer_tabs.is_empty() {
                        (true, Vec::new(), 0)
                    } else {
                        (false, s.parked_drawer_tabs.clone(), s.drawer_active_tab)
                    }
                }
                None => return,
            }
        };

        if !needs_default && parked.is_empty() {
            if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.parked_drawer_tabs.clear();
                session.drawer_parked_at = None;
            }
            return;
        }

        if needs_default {
            self.spawn_drawer_tab(cursor, None, None, None, window, cx);
        } else if !parked.is_empty() {
            for tab in parked {
                self.spawn_drawer_tab(cursor, Some(tab.name), None, tab.replay, window, cx);
            }
            if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.parked_drawer_tabs.clear();
                session.drawer_parked_at = None;
                // Restore the tab the user was last on. Spawning does not touch
                // `drawer_active_tab`, but a park that raced a tab close could
                // leave it past the end.
                session.drawer_active_tab =
                    active_tab.min(session.drawer_tabs.len().saturating_sub(1));
            }
        }
    }

    /// Park the drawer terminals of sessions that have sat unfocused past the
    /// configured threshold, reclaiming the memory their dev servers hold
    /// (DEV-445).
    ///
    /// Disabled unless the user sets `drawer_park_idle_mins`. Even then this is
    /// deliberately conservative: see [`crate::session::should_park_drawer`] for
    /// what disqualifies a session, and note that a drawer is parked only when
    /// *every* tab in it can be faithfully restored.
    pub(crate) fn reap_idle_drawers(&mut self, cx: &mut Context<Self>) {
        let now = std::time::SystemTime::now();
        let active = self.active;

        // Refresh the focused session's stamp first, and unconditionally —
        // before the disabled check, so that turning parking on does not
        // immediately park whatever the user is looking at on the strength of a
        // stamp that stopped being maintained. Doing it here rather than at the
        // several places that assign `self.active` is what keeps the stamp from
        // drifting out of sync with what is actually on screen.
        if let Some(cursor) = active {
            if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.last_focused_at = now;
            }
        }

        let Some(threshold) = self.user_settings.drawer_park_idle() else {
            return;
        };

        let mut to_park: Vec<SessionCursor> = Vec::new();
        for (project_idx, project) in self.projects.iter().enumerate() {
            for (session_idx, session) in project.sessions.iter().enumerate() {
                let cursor = SessionCursor {
                    project_idx,
                    session_idx,
                };
                if Some(cursor) == active {
                    continue;
                }
                if crate::session::should_park_drawer(
                    session.status,
                    session.startup_in_flight(),
                    &session.tab_parkability(),
                    session.unfocused_for(now),
                    Some(threshold),
                ) {
                    to_park.push(cursor);
                }
            }
        }

        if to_park.is_empty() {
            return;
        }

        for cursor in &to_park {
            // A rename in flight holds a raw tab index into a list we are about
            // to empty. Committing it afterwards would write into an index that
            // no longer exists — and after an unpark, into a different tab.
            if self
                .drawer
                .rename
                .as_ref()
                .is_some_and(|(c, _, _)| c == cursor)
            {
                self.drawer.rename = None;
            }

            if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.park_drawer_tabs();
                session.drawer_parked_at = Some(now);
                // Hide the drawer, matching every other path that empties it.
                // `ensure_drawer_tabs` — the unpark path — is only reached by
                // toggling the drawer open, so a parked session left with
                // `drawer_visible == true` would render an empty drawer forever
                // and need two toggles to come back.
                session.drawer_visible = false;
                tracing::info!(session = %session.label, "parked idle drawer");
            }
        }

        self.mark_state_dirty();
        cx.notify();
    }

    /// Focus the currently active drawer tab's terminal view (if any).
    pub(crate) fn focus_active_drawer_tab(
        &self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
        {
            if let Some(tab) = session.drawer_tabs.get(session.drawer_active_tab) {
                let fh = tab.view.read(cx).focus_handle.clone();
                fh.focus(window, cx);
            }
        }
    }
}

/// Build the drawer's UI children (resize handle, tab-strip header bar,
/// and terminal content panel) for insertion into the app's content column.
///
/// Returns an empty vec when the active session has no drawer visible — the
/// caller wraps this in its existing `if drawer_visible` guard. All three
/// elements are siblings at the content-column level.
pub(crate) fn build_drawer_items(
    state: &mut AppState,
    _window: &mut Window,
    cx: &mut Context<AppState>,
) -> Vec<AnyElement> {
    let drawer_h = state.drawer.height;
    let mut items: Vec<AnyElement> = Vec::new();

    // Resize handle — 6px tall invisible hover zone above drawer
    items.push(
        div()
            .id("drawer-resize-handle")
            .w_full()
            .h(px(6.0))
            // Never let the flex column reclaim these 6px. Without this the
            // handle is the column's only child with neither `flex_shrink_0`
            // nor a min height, so any overflow shrinks it to nothing — and a
            // zero-height handle cannot be grabbed to undo the overflow.
            .flex_shrink_0()
            .cursor_row_resize()
            // Invisible at rest (matches the content background); the hover
            // tint is the affordance.
            .bg(theme().bg_base)
            .hover(|s| s.bg(theme().bg_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut AppState, _event, _window, cx| {
                    this.drawer.resizing = true;
                    cx.notify();
                }),
            )
            .into_any_element(),
    );

    // --- Drawer header bar with tab strip ---
    let active_cursor = state.active;
    let (tabs_meta, active_tab_idx, active_tab_view): (
        Vec<(usize, String)>,
        usize,
        Option<Entity<TerminalView>>,
    ) = if let Some(session) = state.active_session() {
        let data = session
            .drawer_tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.name.clone()))
            .collect();
        let view = session
            .drawer_tabs
            .get(session.drawer_active_tab)
            .map(|t| t.view.clone());
        (data, session.drawer_active_tab, view)
    } else {
        (Vec::new(), 0, None)
    };

    let renaming_idx = state
        .drawer
        .rename
        .as_ref()
        .filter(|(c, _, _)| Some(*c) == active_cursor)
        .map(|(_, i, _)| *i);
    let rename_buf = state
        .drawer
        .rename
        .as_ref()
        .filter(|(c, _, _)| Some(*c) == active_cursor)
        .map(|(_, _, buf)| buf.clone())
        .unwrap_or_default();
    let rename_focus = state.drawer.rename_focus.clone();

    let mut tab_strip = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .flex_1()
        .overflow_hidden();

    for (idx, name) in tabs_meta {
        let is_active = idx == active_tab_idx;
        let is_renaming = renaming_idx == Some(idx);
        let tab_bg = if is_active {
            theme().bg_raised
        } else {
            theme().bg_base
        };
        let tab_fg = if is_active {
            theme().text_primary
        } else {
            theme().text_secondary
        };

        let mut tab_el = div()
            .id(("drawer-tab", idx))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(3.0))
            .rounded(px(6.0))
            .bg(tab_bg)
            .text_size(px(11.0))
            .text_color(tab_fg)
            .cursor_pointer()
            .hover(|s| s.bg(theme().bg_hover));

        if is_renaming {
            let display = if rename_buf.is_empty() {
                " ".to_string()
            } else {
                rename_buf.clone()
            };
            let mut label = div()
                .min_w(px(40.0))
                .px(px(4.0))
                .border_1()
                .border_color(theme().accent)
                .rounded(px(6.0))
                .bg(theme().bg_surface)
                .text_color(theme().text_primary)
                .child(format!("{display}▎"));
            if let Some(fh) = rename_focus.clone() {
                label = label.track_focus(&fh).on_key_down(cx.listener(
                    |this: &mut AppState, event: &KeyDownEvent, _window, cx| {
                        let key = event.keystroke.key.as_str();
                        let mods = &event.keystroke.modifiers;
                        match key {
                            "enter" => {
                                this.pending_action =
                                    Some(DrawerAction::CommitRenameDrawerTab.into());
                                cx.notify();
                            }
                            "escape" => {
                                this.pending_action =
                                    Some(DrawerAction::CancelRenameDrawerTab.into());
                                cx.notify();
                            }
                            "backspace" => {
                                if let Some((_, _, buf)) = this.drawer.rename.as_mut() {
                                    buf.pop();
                                    cx.notify();
                                }
                            }
                            _ => {
                                if let Some(ref ch) = event.keystroke.key_char {
                                    if !mods.control && !mods.platform {
                                        if let Some((_, _, buf)) = this.drawer.rename.as_mut() {
                                            buf.push_str(ch);
                                            cx.notify();
                                        }
                                    }
                                }
                            }
                        }
                    },
                ));
            }
            tab_el = tab_el.child(label);
        } else {
            tab_el = tab_el
                .child(div().child(name).on_mouse_down(
                    MouseButton::Left,
                    cx.listener(
                        move |this: &mut AppState, event: &MouseDownEvent, _window, cx| {
                            if event.click_count >= 2 {
                                this.pending_action =
                                    Some(DrawerAction::StartRenameDrawerTab(idx).into());
                            } else {
                                this.pending_action =
                                    Some(DrawerAction::SwitchDrawerTab(idx).into());
                            }
                            cx.notify();
                        },
                    ),
                ))
                .child(
                    div()
                        .id(("drawer-tab-close", idx))
                        .px(px(4.0))
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme().bg_active))
                        .child(icon(icons::X, 11.0, theme().text_faint))
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip {
                                text: "Close tab".into(),
                            })
                            .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                this.pending_action =
                                    Some(DrawerAction::CloseDrawerTab(idx).into());
                                cx.notify();
                            }),
                        ),
                );
        }

        tab_strip = tab_strip.child(tab_el);
    }

    // New tab button
    tab_strip = tab_strip.child(
        div()
            .id("drawer-new-tab-btn")
            .cursor_pointer()
            .px(px(8.0))
            .py(px(3.0))
            .rounded(px(6.0))
            .text_size(px(13.0))
            .text_color(theme().text_faint)
            .hover(|s| s.bg(theme().bg_raised).text_color(theme().text_primary))
            .child("+")
            .tooltip(|_window, cx| {
                cx.new(|_| SimpleTooltip {
                    text: "New terminal tab".into(),
                })
                .into()
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut AppState, _event, _window, cx| {
                    this.pending_action = Some(DrawerAction::NewDrawerTab.into());
                    cx.notify();
                }),
            ),
    );

    items.push(
        div()
            .w_full()
            .flex_shrink_0()
            .px(px(8.0))
            .py(px(4.0))
            .bg(theme().bg_surface)
            .border_b_1()
            .border_color(theme().border_subtle)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(tab_strip)
            .child(
                div()
                    .id("drawer-close-btn")
                    .cursor_pointer()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(6.0))
                    .hover(|s| s.bg(theme().bg_raised))
                    .child(icon(icons::X, 12.0, theme().text_faint))
                    .tooltip(|_window, cx| {
                        cx.new(|_| SimpleTooltip {
                            text: "Close drawer".into(),
                        })
                        .into()
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut AppState, _event, _window, cx| {
                            this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                            cx.notify();
                        }),
                    ),
            )
            .into_any_element(),
    );

    // Drawer content — active tab's terminal view
    // Deliberately shrinkable: when the column runs out of room the drawer
    // gives the space back, rather than the handle above it being crushed.
    // `min_h(0)` is required — flex items default to `min-height: auto`, which
    // would resolve to the terminal's min-content size and block the shrink.
    let mut drawer_panel = div()
        .w_full()
        .h(px(drawer_h))
        .min_h(px(0.0))
        .bg(theme().bg_base);

    if let Some(dt) = active_tab_view {
        // Drawer PTYs lose the same horizontal space to the right changes
        // panel as the main terminal does — keep their column math honest.
        let right_inset = if state.right_panel.visible {
            6.0 + state.right_panel.width
        } else {
            0.0
        };
        dt.update(cx, |tv, _cx| {
            tv.right_inset = right_inset;
        });
        drawer_panel = drawer_panel.child(dt);
    } else {
        drawer_panel = drawer_panel.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.0))
                .text_color(theme().text_ghost)
                .child("Terminal drawer"),
        );
    }
    items.push(
        drawer_panel
            .with_animation(
                "drawer-panel-in",
                Animation::new(std::time::Duration::from_millis(140)),
                |panel, delta| panel.opacity(delta),
            )
            .into_any_element(),
    );

    items
}

/// Resolve the drawer height for a resize drag: the drawer's top edge follows
/// the mouse, bounded below by `DRAWER_MIN_HEIGHT` and above by the space the
/// content column can actually spare.
///
/// The ceiling is measured rather than guessed. `main_area_top` is the window-space
/// Y of the main area's top edge, so everything above it is chrome the drawer can
/// never have — and that chrome is dynamic, growing a row per session awaiting
/// input. The main area then keeps `MAIN_AREA_MIN_HEIGHT` of what remains; without
/// that reserve the column overflows and the flex layout crushes the resize handle,
/// leaving no way to drag the drawer back down.
fn drag_height(viewport_h: f32, main_area_top: f32, mouse_y: f32) -> f32 {
    // `.max()` also keeps the range from inverting on very short windows, where
    // `clamp` would panic rather than merely misbehave.
    let max_height = (viewport_h - main_area_top - MAIN_AREA_MIN_HEIGHT).max(DRAWER_MIN_HEIGHT);
    (viewport_h - mouse_y).clamp(DRAWER_MIN_HEIGHT, max_height)
}

/// Full-viewport drag overlay shown while the user is actively resizing the
/// drawer. Tracks mouse movement, clamps the new height, and persists
/// settings on mouse-up. Caller is responsible for only attaching this when
/// `drawer_resizing` is true.
pub(crate) fn build_drawer_drag_overlay(
    _state: &mut AppState,
    _window: &mut Window,
    cx: &mut Context<AppState>,
) -> AnyElement {
    div()
        .id("drawer-drag-overlay")
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .cursor_row_resize()
        .on_mouse_move(
            cx.listener(|this: &mut AppState, event: &MouseMoveEvent, window, cx| {
                let viewport_h = f32::from(window.viewport_size().height);
                let mouse_y = f32::from(event.position.y);
                let new_height = drag_height(viewport_h, this.drawer.main_area_top.get(), mouse_y);
                if (new_height - this.drawer.height).abs() > 0.5 {
                    this.drawer.height = new_height;
                    window.refresh();
                    cx.notify();
                }
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this: &mut AppState, _event: &MouseUpEvent, _window, cx| {
                this.drawer.resizing = false;
                this.mark_settings_dirty();
                cx.notify();
            }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // Deliberately not `use super::*` — this module glob-imports `gpui::*`,
    // whose `test` attribute macro would shadow the one from libtest.
    use super::{drag_height, DRAWER_MIN_HEIGHT};

    /// Chrome above the drawer is dynamic — each session awaiting input adds an
    /// attention row — so the ceiling has to follow the measured main-area top
    /// rather than a fixed viewport offset.
    #[test]
    fn drag_ceiling_follows_measured_chrome() {
        // Bare column: chrome ends 64px down, so 700 - 64 - 100 = 536 is the cap.
        assert_eq!(drag_height(700.0, 64.0, 0.0), 536.0);
        // Two attention rows push the main area down; the cap drops with it.
        assert_eq!(drag_height(700.0, 150.0, 0.0), 450.0);
    }

    #[test]
    fn drag_tracks_the_mouse_between_the_bounds() {
        assert_eq!(drag_height(700.0, 64.0, 400.0), 300.0);
    }

    #[test]
    fn drag_respects_the_minimum() {
        // Mouse dragged to the very bottom of the window.
        assert_eq!(drag_height(700.0, 64.0, 700.0), DRAWER_MIN_HEIGHT);
    }

    /// The old ceiling was `viewport_h - 200.0`, which inverts the clamp range
    /// on a short window and panics inside `f32::clamp`.
    #[test]
    fn drag_does_not_panic_when_the_window_cannot_fit_the_minimum() {
        assert_eq!(drag_height(180.0, 64.0, 0.0), DRAWER_MIN_HEIGHT);
        assert_eq!(drag_height(180.0, 64.0, 180.0), DRAWER_MIN_HEIGHT);
    }

    /// Reproduces the reported stuck state: a 739.6px drawer height persisted
    /// from a taller window, reloaded into a ~660px viewport. The drag must
    /// resolve to something the column can actually render.
    #[test]
    fn drag_recovers_a_height_persisted_from_a_taller_window() {
        let recovered = drag_height(660.0, 150.0, 200.0);
        assert!(recovered < 739.6);
        assert_eq!(recovered, 410.0);
    }
}
