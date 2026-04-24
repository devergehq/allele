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

use gpui::*;

use crate::actions::{DrawerAction, SessionCursor, SettingsAction};
use crate::app_state::{AppState, DRAWER_MIN_HEIGHT};
use crate::session::DrawerTab;
use crate::terminal::{clamp_font_size, ShellCommand, TerminalEvent, TerminalView, DEFAULT_FONT_SIZE};
use crate::SimpleTooltip;

impl AppState {
    /// Spawn one drawer terminal tab in the given session with an optional
    /// pre-chosen name and optional shell command. Default name is
    /// "Terminal N" where N is 1-based; default command drops into the
    /// user's shell.
    pub(crate) fn spawn_drawer_tab(
        &mut self,
        cursor: SessionCursor,
        name: Option<String>,
        command: Option<ShellCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let working_dir = self.projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .and_then(|s| s.clone_path.clone());
        let initial_font_size = self.user_settings.font_size;
        let drawer_tv =
            cx.new(|cx| TerminalView::new(window, cx, command, working_dir, initial_font_size));
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

        if let Some(session) = self.projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            let tab_name = name.unwrap_or_else(|| {
                format!("Terminal {}", session.drawer_tabs.len() + 1)
            });
            session.drawer_tabs.push(DrawerTab {
                view: drawer_tv,
                name: tab_name,
            });
        }
    }

    /// Materialise drawer tabs for a session that has none yet. Uses saved
    /// names from `pending_drawer_tab_names` if present, else creates one
    /// default "Terminal 1" tab.
    pub(crate) fn ensure_drawer_tabs(
        &mut self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (needs_default, pending) = {
            let session = self.projects
                .get(cursor.project_idx)
                .and_then(|p| p.sessions.get(cursor.session_idx));
            match session {
                Some(s) if !s.drawer_tabs.is_empty() => (false, Vec::new()),
                Some(s) => {
                    if s.pending_drawer_tab_names.is_empty() {
                        (true, Vec::new())
                    } else {
                        (false, s.pending_drawer_tab_names.clone())
                    }
                }
                None => return,
            }
        };

        if needs_default {
            self.spawn_drawer_tab(cursor, None, None, window, cx);
        } else if !pending.is_empty() {
            for name in pending {
                self.spawn_drawer_tab(cursor, Some(name), None, window, cx);
            }
            if let Some(session) = self.projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.pending_drawer_tab_names.clear();
                if session.drawer_active_tab >= session.drawer_tabs.len() {
                    session.drawer_active_tab = session.drawer_tabs.len().saturating_sub(1);
                }
            }
        }
    }

    /// Focus the currently active drawer tab's terminal view (if any).
    pub(crate) fn focus_active_drawer_tab(
        &self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.projects
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
            .cursor_row_resize()
            .bg(rgb(0x313244))
            .hover(|s| s.bg(rgb(0x45475a)))
            .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut AppState, _event, _window, cx| {
                this.drawer.resizing = true;
                cx.notify();
            }))
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
        let tab_bg = if is_active { 0x313244 } else { 0x1e1e2e };
        let tab_fg = if is_active { 0xcdd6f4 } else { 0xa6adc8 };

        let mut tab_el = div()
            .id(("drawer-tab", idx))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .bg(rgb(tab_bg))
            .text_size(px(11.0))
            .text_color(rgb(tab_fg))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(0x45475a)));

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
                .border_color(rgb(0x89b4fa))
                .rounded(px(2.0))
                .bg(rgb(0x181825))
                .text_color(rgb(0xcdd6f4))
                .child(format!("{display}▎"));
            if let Some(fh) = rename_focus.clone() {
                label = label
                    .track_focus(&fh)
                    .on_key_down(cx.listener(
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
                                    if let Some((_, _, buf)) =
                                        this.drawer.rename.as_mut()
                                    {
                                        buf.pop();
                                        cx.notify();
                                    }
                                }
                                _ => {
                                    if let Some(ref ch) = event.keystroke.key_char {
                                        if !mods.control && !mods.platform {
                                            if let Some((_, _, buf)) =
                                                this.drawer.rename.as_mut()
                                            {
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
                .child(
                    div()
                        .child(name)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut AppState, event: &MouseDownEvent, _window, cx| {
                                if event.click_count >= 2 {
                                    this.pending_action =
                                        Some(DrawerAction::StartRenameDrawerTab(idx).into());
                                } else {
                                    this.pending_action =
                                        Some(DrawerAction::SwitchDrawerTab(idx).into());
                                }
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .id(("drawer-tab-close", idx))
                        .px(px(4.0))
                        .rounded(px(3.0))
                        .text_size(px(11.0))
                        .text_color(rgb(0x6c7086))
                        .hover(|s| {
                            s.bg(rgb(0x585b70))
                                .text_color(rgb(0xf38ba8))
                        })
                        .child("×")
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip { text: "Close tab".into() }).into()
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
            .rounded(px(4.0))
            .text_size(px(13.0))
            .text_color(rgb(0x6c7086))
            .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
            .child("+")
            .tooltip(|_window, cx| {
                cx.new(|_| SimpleTooltip { text: "New terminal tab".into() }).into()
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
            .bg(rgb(0x181825))
            .border_b_1()
            .border_color(rgb(0x313244))
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
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .text_color(rgb(0x6c7086))
                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                    .child("×")
                    .tooltip(|_window, cx| {
                        cx.new(|_| SimpleTooltip { text: "Close drawer".into() }).into()
                    })
                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut AppState, _event, _window, cx| {
                        this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                        cx.notify();
                    })),
            )
            .into_any_element(),
    );

    // Drawer content — active tab's terminal view
    let mut drawer_panel = div()
        .w_full()
        .h(px(drawer_h))
        .flex_shrink_0()
        .bg(rgb(0x1e1e2e));

    if let Some(dt) = active_tab_view {
        drawer_panel = drawer_panel.child(dt);
    } else {
        drawer_panel = drawer_panel.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.0))
                .text_color(rgb(0x45475a))
                .child("Terminal drawer"),
        );
    }
    items.push(drawer_panel.into_any_element());

    items
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
        .on_mouse_move(cx.listener(|this: &mut AppState, event: &MouseMoveEvent, window, cx| {
            let viewport_h = f32::from(window.viewport_size().height);
            let mouse_y = f32::from(event.position.y);
            // Drawer height = distance from bottom of viewport to mouse
            let new_height = (viewport_h - mouse_y).clamp(DRAWER_MIN_HEIGHT, viewport_h - 200.0);
            if (new_height - this.drawer.height).abs() > 0.5 {
                this.drawer.height = new_height;
                window.refresh();
                cx.notify();
            }
        }))
        .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut AppState, _event: &MouseUpEvent, _window, cx| {
            this.drawer.resizing = false;
            this.save_settings();
            cx.notify();
        }))
        .into_any_element()
}
