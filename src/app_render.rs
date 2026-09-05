//! The root `Render` impl for [`AppState`] — the whole window tree.
//!
//! Split out of `main.rs` (DEV-507), which had reached 4,979 lines against the
//! 5,000-line gate in `tests/architecture.rs`. This is a verbatim move: the impl
//! is unchanged, only relocated.
//!
//! `render()` is deliberately **not** pure. Per frame it also dispatches pending
//! actions, transitions PTY status, opens modals and checkpoints persistence.
//! That is load-bearing — GPUI re-renders on state change, and these are the
//! hooks that keep the tree in step with it. Do not "clean it up" into a pure
//! render without understanding what drives each side effect.
//!
//! Finer extraction (one function per panel) was tried and abandoned: it runs
//! straight into borrow conflicts between `&mut AppState`, `Window`,
//! `Context<AppState>` and the `'static` listener closures the tree installs.

use crate::actions::{SessionAction, SidebarAction};
use crate::app_state::{
    AppState, MainTab, MAIN_AREA_MIN_HEIGHT, RIGHT_SIDEBAR_MIN_WIDTH, SIDEBAR_MIN_WIDTH,
};
use crate::icon::{icon, name as icons};
use crate::session::SessionStatus;
use crate::theme::{theme, with_alpha};
use crate::{
    claude_session_history_exists, debug_capture, git, naming, new_session_modal, SimpleTooltip,
};
use gpui::*;
use tracing::warn;

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Stash the main window handle for off-thread callers (DEV-415).
        // Done here rather than at `open_window` because the downcast needs
        // the root view to exist, and because binding that call's result
        // makes rustfmt reflow the whole 800-line window-construction block.
        // See `src/dispatch/mod.rs` for what needs it.
        if self.main_window.is_none() {
            self.main_window = window.window_handle().downcast::<AppState>();
        }

        // Process pending actions — dispatcher lives in src/pending_actions.rs.
        self.dispatch_pending_action(window, cx);

        // Keep Reader state scoped to the active session (DEV-66).
        self.sync_reader_session();

        // Remember which project the overview should describe (DEV-310).
        self.track_overview_project();

        if self.capture_ui_requested {
            self.capture_ui_requested = false;
            let active = self.active;
            let active_project = active.and_then(|c| self.projects.get(c.project_idx));
            let active_session =
                active_project.and_then(|p| active.and_then(|c| p.sessions.get(c.session_idx)));
            let metadata = debug_capture::CaptureMetadata {
                status: "ok",
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                image_path: debug_capture::debug_dir()
                    .map(|p| p.join("latest.png").to_string_lossy().into_owned())
                    .unwrap_or_default(),
                width: f32::from(window.viewport_size().width) as f64,
                height: f32::from(window.viewport_size().height) as f64,
                active_project: active_project.map(|p| p.name.as_str()),
                active_session: active_session.map(|s| s.label.as_str()),
                main_tab: match self.main_tab {
                    MainTab::Claude => "claude",
                    MainTab::Reader => "reader",
                    MainTab::Browser => "browser",
                    MainTab::Transcript => "transcript",
                },
                sidebar_visible: self.sidebar.visible,
                changes_visible: self.right_panel.visible,
                drawer_visible: active_session.map(|s| s.drawer_visible).unwrap_or(false),
                error: None,
            };
            if let Err(error) = debug_capture::capture(metadata) {
                let fallback = debug_capture::CaptureMetadata {
                    status: "error",
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                    image_path: String::new(),
                    width: 0.0,
                    height: 0.0,
                    active_project: None,
                    active_session: None,
                    main_tab: "unknown",
                    sidebar_visible: self.sidebar.visible,
                    changes_visible: self.right_panel.visible,
                    drawer_visible: false,
                    error: None,
                };
                debug_capture::write_error(fallback, error);
            }
        }

        // If the user is on the Browser tab but it's no longer eligible
        // (flag turned off, switched to a session without a preview URL,
        // or project config lost the preview entry), fall back to Claude
        // so the main pane keeps showing something useful.
        if self.main_tab == MainTab::Browser && !self.browser_tab_available() {
            self.main_tab = MainTab::Claude;
        }

        // Update session statuses from PTY state.
        // Any attached session (Running, Idle, AwaitingInput, ResponseReady)
        // can transition to Done when its PTY actually exits. Done and
        // Suspended sessions are already terminal/attached-less and are
        // skipped.
        let mut pty_state_dirty = false;
        let now = std::time::Instant::now();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                if matches!(
                    session.status,
                    SessionStatus::Done | SessionStatus::Suspended
                ) {
                    continue;
                }
                let Some(tv) = session.terminal_view.as_ref() else {
                    continue;
                };
                if tv.read(cx).has_exited() {
                    // If we're still inside the resume grace window, treat
                    // this as a resume failure — revert to Suspended and
                    // drop the PTY so the user can try again or the UI can
                    // prompt them — rather than silently locking them into
                    // the "Session ended" overlay.
                    let resume_failed = session
                        .resuming_until
                        .map(|deadline| now < deadline)
                        .unwrap_or(false);
                    if resume_failed {
                        warn!(
                            "Resume failed for session {} — PTY exited inside grace window",
                            session.id
                        );
                        session.terminal_view = None;
                        session.set_status(SessionStatus::Suspended);
                    } else {
                        warn!(
                            "PTY exited for session {} ({}) — marking Done",
                            session.id, session.label
                        );
                        session.set_status(SessionStatus::Done);
                    }
                    session.last_active = std::time::SystemTime::now();
                    session.resuming_until = None;
                    pty_state_dirty = true;
                } else if let Some(deadline) = session.resuming_until {
                    if now >= deadline {
                        session.resuming_until = None;
                    }
                }
            }
        }
        if pty_state_dirty {
            self.mark_state_dirty();
        }

        // Active-only view mode (DEV-295) — what the filter is holding back.
        // Computed with the same predicates the sidebar renders with, so the
        // hint row can't claim a count the tree disagrees with.
        let active_only = self.user_settings.sidebar_active_only;
        let (hidden_sessions, hidden_projects) =
            crate::sidebar::render::active_only_hidden_counts(&self.projects, self.active);

        // Build sidebar items: for each project, a header then its sessions
        let sidebar_items = crate::sidebar::render::build_sidebar_items(self, window, cx);

        // Active-only hint row — rendered only when the filter is actually
        // holding something back. `None` collapses to zero children.
        let active_only_hint: Option<AnyElement> = (active_only && hidden_sessions > 0).then(|| {
            div()
                .px(px(12.0))
                .py(px(4.0))
                .bg(with_alpha(theme().accent, 0.08))
                .border_b_1()
                .border_color(theme().border_subtle)
                .flex()
                .flex_row()
                .gap(px(6.0))
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(11.0))
                        .text_color(theme().text_dim)
                        .child(if hidden_projects > 0 {
                            format!(
                                "Active only · {hidden_sessions} sessions, {hidden_projects} projects hidden"
                            )
                        } else {
                            format!("Active only · {hidden_sessions} sessions hidden")
                        }),
                )
                .child(
                    div()
                        .id("show-all-sessions")
                        .flex_shrink_0()
                        .cursor_pointer()
                        .px(px(4.0))
                        .rounded(px(4.0))
                        .min_h(px(crate::accessibility::DENSE_CONTROL_MIN_HEIGHT))
                        .text_size(px(11.0))
                        .text_color(theme().accent)
                        .hover(|s| s.bg(theme().bg_raised))
                        .child("Show all")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this: &mut Self, _event, _window, cx| {
                                this.pending_action =
                                    Some(SidebarAction::ShowAllSessions.into());
                                cx.notify();
                            }),
                        ),
                )
                .into_any_element()
        });

        // Status summary
        let total_projects = self.projects.len();
        let total_sessions: usize = self.projects.iter().map(|p| p.sessions.len()).sum();
        let running: usize = self
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::Running)
            .count();
        let awaiting: usize = self
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::AwaitingInput)
            .count();
        let response_ready: usize = self
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::ResponseReady)
            .count();

        let fps = self
            .active_session()
            .and_then(|s| s.terminal_view.as_ref())
            .map(|tv| tv.read(cx).current_fps)
            .unwrap_or(0);

        let active_is_done = self
            .active_session()
            .map(|s| s.status == SessionStatus::Done)
            .unwrap_or(false);

        // Can the currently-Done session be revived with its prior conversation?
        // Needs both the clone directory still on disk *and* Claude's history
        // jsonl for this session id. When true, the "Session ended" bar shows
        // a primary "Resume" button; otherwise it falls back to "New Session".
        let active_is_resumable = self
            .active_session()
            .map(|s| {
                s.clone_path.as_ref().map(|p| p.exists()).unwrap_or(false)
                    && claude_session_history_exists(&s.id)
            })
            .unwrap_or(false);

        // Changes panel staleness check — kicks a background `git status`
        // when the panel is visible but showing data for a different
        // session's clone (first open, session switch). Guarded inside.
        self.ensure_changes_fresh(cx);

        let sidebar_w = self.sidebar.width;
        let sidebar_visible = self.sidebar.visible;
        let is_resizing = self.sidebar.resizing;
        let drawer_is_resizing = self.drawer.resizing;
        let drawer_visible = self
            .active_session()
            .map(|s| s.drawer_visible)
            .unwrap_or(false);
        let right_sidebar_visible = self.right_panel.visible;
        let right_sidebar_w = self.right_panel.width;
        let right_sidebar_resizing = self.right_panel.resizing;

        // Outer non-flex container that hosts the flex row AND the drag overlay.
        // Keeping the overlay OUTSIDE the flex container ensures Taffy's layout
        // engine doesn't try to allocate flex space to an absolutely-positioned element.
        // No window-wide opaque bg: the blurred window background shows
        // through the translucent sidebar; the content column paints its own
        // opaque base so terminal/content legibility is untouched.
        let mut flex_row = div()
            .id("app-root")
            .flex()
            .size_full()
            .text_color(theme().text_primary);

        // --- Left sidebar (conditional on sidebar_visible) ---
        if sidebar_visible {
            flex_row = flex_row.child(
                // Sidebar
                div()
                    .w(px(sidebar_w))
                    .flex_shrink_0()
                    .h_full()
                    .bg(with_alpha(theme().bg_surface, 0.85))
                    .border_r_1()
                    .border_color(theme().border_subtle)
                    .font_family(crate::theme::FONT_UI)
                    .flex()
                    .flex_col()
                    // Header — one 38px titlebar row beside the traffic
                    // lights; empty space drags the window.
                    .child(
                        div()
                            .h(px(38.0))
                            .flex_shrink_0()
                            .pl(px(78.0))
                            .pr(px(12.0))
                            .window_control_area(WindowControlArea::Drag)
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .font_weight(FontWeight::BOLD)
                                            .child("Allele"),
                                    )
                                    // Two identical windows side by side is its
                                    // own hazard, so a redirected root says so
                                    // where the eye already is (DEV-487).
                                    .children(crate::paths::is_redirected().then(|| {
                                        div()
                                            .px(px(6.0))
                                            .py(px(1.0))
                                            .rounded(px(4.0))
                                            .bg(theme().warning)
                                            .text_size(px(9.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme().bg_base)
                                            .child("SANDBOX")
                                    })),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child({
                                        // Active-only view toggle (DEV-295).
                                        // Tinted accent while engaged so the
                                        // filtered view can't be mistaken for
                                        // a sidebar that lost its sessions.
                                        let mut btn = div()
                                            .id("active-only-btn")
                                            .cursor_pointer()
                                            .p(px(4.0))
                                            .rounded(px(6.0))
                                            .hover(|s| s.bg(theme().bg_raised))
                                            .child(icon(
                                                icons::FILTER,
                                                14.0,
                                                if active_only {
                                                    theme().accent
                                                } else {
                                                    theme().text_faint
                                                },
                                            ))
                                            .tooltip(move |_window, cx| {
                                                cx.new(|_| SimpleTooltip {
                                                    text: if active_only {
                                                        "Showing active sessions only — click to show all (⌘⇧E)".into()
                                                    } else {
                                                        "Show active sessions only (⌘⇧E)".into()
                                                    },
                                                })
                                                .into()
                                            })
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this: &mut Self, _event, _window, cx| {
                                                    this.pending_action =
                                                        Some(SidebarAction::ToggleActiveOnly.into());
                                                    cx.notify();
                                                }),
                                            );
                                        if active_only {
                                            btn = btn.bg(theme().bg_raised);
                                        }
                                        btn
                                    })
                                    .child(
                                        // "Pull session from remote" button
                                        div()
                                            .id("pull-remote-btn")
                                            .cursor_pointer()
                                            .p(px(4.0))
                                            .rounded(px(6.0))
                                            .hover(|s| s.bg(theme().bg_raised))
                                            .child(icon(
                                                crate::icon::name::CLOUD_DOWNLOAD,
                                                15.0,
                                                theme().text_faint,
                                            ))
                                            .tooltip(|_window, cx| {
                                                cx.new(|_| SimpleTooltip {
                                                    text: "Pull session from remote".into(),
                                                })
                                                .into()
                                            })
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this: &mut Self, _event, _window, cx| {
                                                    this.open_remote_browser(cx);
                                                }),
                                            ),
                                    )
                                    .child(
                                        // "Open project" button
                                        div()
                                            .id("new-project-btn")
                                            .cursor_pointer()
                                            .px(px(6.0))
                                            .py(px(2.0))
                                            .rounded(px(6.0))
                                            .text_size(px(16.0))
                                            .text_color(theme().text_faint)
                                            .hover(|s| {
                                                s.bg(theme().bg_raised).text_color(theme().success)
                                            })
                                            .child("+")
                                            .tooltip(|_window, cx| {
                                                cx.new(|_| SimpleTooltip {
                                                    text: "Open project".into(),
                                                })
                                                .into()
                                            })
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this: &mut Self, _event, _window, cx| {
                                                    this.open_folder_picker(cx);
                                                }),
                                            ),
                                    ),
                            ),
                    )
                    // Search filter
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .child(
                                div()
                                    .w_full()
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .rounded(px(6.0))
                                    .bg(theme().bg_sunken)
                                    .text_size(px(12.0))
                                    .text_color(theme().text_primary)
                                    .overflow_hidden()
                                    .child(self.sidebar_filter_input.clone()),
                            ),
                    )
                    // Active-only hint row — states what the filter is holding
                    // back and offers a one-click way out, so a sidebar that
                    // filters down to nothing is never a dead end. Sits
                    // outside the scroll container so it can't scroll away.
                    .children(active_only_hint)
                    // Session list
                    .child(
                        div()
                            .id("sidebar-session-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .children(sidebar_items),
                    )
                    // Status bar — attention summary lives here.
                    .child({
                        let mut bar = div()
                            .px(px(12.0))
                            .py(px(8.0))
                            .text_size(px(12.0))
                            .text_color(theme().text_faint)
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .items_center()
                            .child(format!(
                                "{total_projects}p · {total_sessions}s · {running} running · {fps} fps"
                            ));

                        if awaiting > 0 {
                            bar = bar.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(4.0))
                                    .text_color(SessionStatus::AwaitingInput.color())
                                    .child(icon(
                                        icons::ALERT_TRIANGLE,
                                        11.0,
                                        SessionStatus::AwaitingInput.color(),
                                    ))
                                    .child(format!("{awaiting} need input")),
                            );
                        }
                        if response_ready > 0 {
                            bar = bar.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(4.0))
                                    .text_color(SessionStatus::ResponseReady.color())
                                    .child(icon(
                                        icons::STAR_FILL,
                                        11.0,
                                        SessionStatus::ResponseReady.color(),
                                    ))
                                    .child(format!("{response_ready} ready")),
                            );
                        }
                        bar
                    }),
            );
            // Resize handle — 6px wide invisible hover zone over the sidebar border.
            flex_row = flex_row.child(
                div()
                    .id("sidebar-resize-handle")
                    .w(px(6.0))
                    .h_full()
                    // Same reason as the drawer handle: never let the flex row
                    // reclaim these 6px. A crushed handle cannot be grabbed to
                    // undo the overflow that crushed it.
                    .flex_shrink_0()
                    // Opaque at rest — an unpainted strip would show the raw
                    // blurred desktop through the vibrant window background.
                    .bg(theme().bg_base)
                    .cursor_col_resize()
                    .hover(|s| s.bg(theme().bg_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _event, _window, cx| {
                            this.sidebar.resizing = true;
                            cx.notify();
                        }),
                    ),
            );
        }

        flex_row = flex_row.child({
            // Right-hand content column: main terminal + optional drawer
            let mut content_col = div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .h_full()
                .bg(theme().bg_base)
                .flex()
                .flex_col();

            // The chrome above the main area is conditional, so its height is
            // not knowable up front. Count the children as they go in; the
            // prepaint listener below uses the index to read back the main
            // area's real top edge for the drawer's resize clamp.
            let mut main_area_idx = 0usize;

            // --- Attention bar (sessions needing input) — above tabs for visibility ---
            if let Some(attention_bar) = self.render_attention_bar(cx) {
                content_col = content_col.child(attention_bar);
                main_area_idx += 1;
            }

            if let Some(summary) = self.render_session_summary_header(active_is_resumable, cx) {
                content_col = content_col.child(summary);
                main_area_idx += 1;
            }

            // --- Main-area tab strip: Claude / Reader ---
            content_col = content_col.child(self.render_main_tab_strip(cx));
            main_area_idx += 1;

            // --- Main terminal area (flex_1, takes remaining space) ---
            {
                let mut main_area = div()
                    .flex_1()
                    .min_h(px(MAIN_AREA_MIN_HEIGHT))
                    .overflow_hidden()
                    .relative();

                match self.main_tab {
                    MainTab::Claude => {
                        main_area = main_area.pt(px(6.0));
                        if let Some(tv) =
                            self.active_session().and_then(|s| s.terminal_view.clone())
                        {
                            // Tell the main terminal how much space the drawer
                            // reserves below it so the PTY resize is correct.
                            let inset = if drawer_visible {
                                // 6px resize handle + ~30px header + drawer panel
                                6.0 + 30.0 + self.drawer.height
                            } else {
                                0.0
                            };
                            // Same for the right changes panel: its width +
                            // 6px resize handle are unavailable to the PTY.
                            let right_inset = if right_sidebar_visible {
                                6.0 + right_sidebar_w
                            } else {
                                0.0
                            };
                            tv.update(cx, |tv, _cx| {
                                tv.bottom_inset = inset;
                                tv.right_inset = right_inset;
                            });
                            main_area = main_area.child(tv);
                        } else if self.projects.is_empty() {
                            main_area = main_area.child(
                                div()
                                    .size_full()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(16.0))
                                    .bg(theme().bg_base)
                                    .child(icon(icons::HELIX, 28.0, with_alpha(theme().ready, 0.7)))
                                    .child(
                                        div()
                                            .text_size(px(16.0))
                                            .text_color(theme().text_faint)
                                            .child("No active session"),
                                    )
                                    .child(div().w(px(56.0)).h(px(2.0)).rounded(px(1.0)).bg(
                                        linear_gradient(
                                            90.0,
                                            linear_color_stop(theme().accent, 0.0),
                                            linear_color_stop(theme().ready, 1.0),
                                        ),
                                    ))
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme().text_ghost)
                                            .child("Click + in the sidebar to open a project"),
                                    ),
                            );
                        } else {
                            main_area = main_area.child(self.render_project_overview(cx));
                        }
                    }
                    MainTab::Reader => {
                        main_area = main_area.child(self.render_reader_view(cx));
                    }
                    MainTab::Browser => {
                        main_area = main_area.child(self.render_browser_placeholder(cx));
                    }
                    MainTab::Transcript => {
                        main_area = main_area.child(self.render_transcript_view(cx));
                    }
                }

                if active_is_done {
                    let mut buttons = div().flex().flex_row().gap(px(8.0));

                    if active_is_resumable {
                        buttons = buttons.child(
                            div()
                                .id("resume-btn")
                                .cursor_pointer()
                                .px(px(10.0))
                                .py(px(4.0))
                                .rounded(px(6.0))
                                .bg(theme().accent)
                                .text_size(px(11.0))
                                .text_color(theme().text_on_accent)
                                .hover(|s| s.bg(theme().info))
                                .child("Resume")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this: &mut Self, _event, _window, cx| {
                                        if let Some(active) = this.active {
                                            this.pending_action = Some(
                                                SessionAction::ResumeSession {
                                                    project_idx: active.project_idx,
                                                    session_idx: active.session_idx,
                                                }
                                                .into(),
                                            );
                                            cx.notify();
                                        }
                                    }),
                                ),
                        );
                    }

                    buttons = buttons.child(
                        div()
                            .id("restart-btn")
                            .cursor_pointer()
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .bg(theme().bg_hover)
                            .text_size(px(11.0))
                            .text_color(theme().text_primary)
                            .hover(|s| s.bg(theme().bg_active))
                            .child("New Session")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _event, _window, cx| {
                                    if let Some(active) = this.active {
                                        this.pending_action = Some(
                                            SessionAction::AddSessionToProject(active.project_idx)
                                                .into(),
                                        );
                                        cx.notify();
                                    }
                                }),
                            ),
                    );

                    main_area = main_area.child(
                        // "Session ended" overlay bar at bottom
                        div()
                            .absolute()
                            .bottom(px(0.0))
                            .left(px(0.0))
                            .right(px(0.0))
                            .px(px(16.0))
                            .py(px(10.0))
                            .bg(theme().bg_raised)
                            .border_t_1()
                            .border_color(theme().border_default)
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme().text_faint)
                                    .child("Session ended"),
                            )
                            .child(buttons),
                    );
                }

                // --- Quit confirmation banner (absolute overlay at top) ---
                if self.confirming.quit {
                    let active_count = self
                        .projects
                        .iter()
                        .flat_map(|p| &p.sessions)
                        .filter(|s| {
                            matches!(s.status, SessionStatus::Running | SessionStatus::Idle)
                        })
                        .count();
                    let label = if active_count == 1 {
                        "1 session is still running — quit anyway?".to_string()
                    } else {
                        format!("{active_count} sessions are still running — quit anyway?")
                    };
                    main_area = main_area.child(
                        div()
                            .absolute()
                            .top(px(0.0))
                            .left(px(0.0))
                            .right(px(0.0))
                            .px(px(16.0))
                            .py(px(10.0))
                            .bg(theme().tint_danger_soft) // subtle red tint
                            .border_b_1()
                            .border_color(theme().danger)
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme().danger) // red
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .id("quit-confirm-btn")
                                            .cursor_pointer()
                                            .px(px(10.0))
                                            .py(px(4.0))
                                            .rounded(px(6.0))
                                            .bg(theme().danger)
                                            .text_size(px(11.0))
                                            .text_color(theme().text_on_accent)
                                            .hover(|s| s.bg(theme().danger_soft))
                                            .child("Quit")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this: &mut Self, _event, _window, cx| {
                                                        this.confirming.quit = false;
                                                        cx.quit();
                                                    },
                                                ),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("quit-cancel-btn")
                                            .cursor_pointer()
                                            .px(px(10.0))
                                            .py(px(4.0))
                                            .rounded(px(6.0))
                                            .bg(theme().bg_hover)
                                            .text_size(px(11.0))
                                            .text_color(theme().text_primary)
                                            .hover(|s| s.bg(theme().bg_active))
                                            .child("Cancel")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this: &mut Self, _event, _window, cx| {
                                                        this.confirming.quit = false;
                                                        cx.notify();
                                                    },
                                                ),
                                            ),
                                    ),
                            ),
                    );
                }

                // --- Pull warning banner (absolute overlay at top) ---
                if let Some(ref warning) = self.pull_warning {
                    // The banner carries more than pull failures now (branch
                    // resolution warnings land here too), so the message
                    // supplies its own context rather than being prefixed.
                    let label = warning.clone();
                    main_area = main_area.child(
                        div()
                            .absolute()
                            .top(px(0.0))
                            .left(px(0.0))
                            .right(px(0.0))
                            .px(px(16.0))
                            .py(px(10.0))
                            .bg(theme().tint_warning_soft) // subtle amber tint
                            .border_b_1()
                            .border_color(theme().warning) // yellow
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme().warning) // yellow
                                    .child(label),
                            )
                            .child(
                                div()
                                    .id("pull-warning-dismiss-btn")
                                    .cursor_pointer()
                                    .px(px(10.0))
                                    .py(px(4.0))
                                    .rounded(px(6.0))
                                    .bg(theme().bg_hover)
                                    .text_size(px(11.0))
                                    .text_color(theme().text_primary)
                                    .hover(|s| s.bg(theme().bg_active))
                                    .child("Dismiss")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this: &mut Self, _event, _window, cx| {
                                            this.pull_warning = None;
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    );
                }

                // --- Session-sync notice banner (absolute overlay at top) ---
                if let Some(ref notice) = self.sync_notice {
                    let label = notice.clone();
                    main_area = main_area.child(
                        div()
                            .absolute()
                            .top(px(0.0))
                            .left(px(0.0))
                            .right(px(0.0))
                            .px(px(16.0))
                            .py(px(10.0))
                            .bg(theme().bg_raised)
                            .border_b_1()
                            .border_color(theme().border_subtle)
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(theme().text_primary)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .id("sync-notice-dismiss-btn")
                                    .cursor_pointer()
                                    .px(px(10.0))
                                    .py(px(4.0))
                                    .rounded(px(6.0))
                                    .bg(theme().bg_hover)
                                    .text_size(px(11.0))
                                    .text_color(theme().text_primary)
                                    .hover(|s| s.bg(theme().bg_active))
                                    .child("Dismiss")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|this: &mut Self, _event, _window, cx| {
                                            this.sync_notice = None;
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    );
                }

                content_col = content_col.child(main_area);
            }

            // --- Drawer terminal (fixed height, shown per-session) ---
            if drawer_visible {
                content_col =
                    content_col.children(crate::drawer::build_drawer_items(self, window, cx));
            }

            // Hand the drawer's resize clamp the main area's real top edge.
            // Writing through the shared cell rather than `Entity::update`
            // keeps this off the notify path — prepaint must not re-render.
            let main_area_top = self.drawer.main_area_top.clone();
            content_col = content_col.on_children_prepainted(move |bounds, _window, _cx| {
                if let Some(b) = bounds.get(main_area_idx) {
                    main_area_top.set(f32::from(b.origin.y));
                }
            });

            content_col
        });

        // --- Right sidebar (conditional on right_sidebar_visible) ---
        if right_sidebar_visible {
            // Resize handle — 6px wide on left edge of right sidebar
            flex_row = flex_row.child(
                div()
                    .id("right-sidebar-resize-handle")
                    .w(px(6.0))
                    .h_full()
                    // See the left sidebar handle above.
                    .flex_shrink_0()
                    .bg(theme().bg_base)
                    .cursor_col_resize()
                    .hover(|s| s.bg(theme().bg_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _event, _window, cx| {
                            this.right_panel.resizing = true;
                            cx.notify();
                        }),
                    ),
            );
            flex_row = flex_row.child(
                div()
                    .w(px(right_sidebar_w))
                    .flex_shrink_0()
                    .h_full()
                    .bg(theme().bg_surface)
                    .border_l_1()
                    .border_color(theme().border_subtle)
                    .flex()
                    .flex_col()
                    // Header
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(10.0))
                            .border_b_1()
                            .border_color(theme().border_subtle)
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .font_weight(FontWeight::BOLD)
                                            .child("Changes"),
                                    )
                                    .child({
                                        let staged =
                                            self.changes.files.iter().filter(|f| f.staged).count();
                                        let unstaged = self.changes.files.len() - staged;
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(theme().text_faint)
                                            .child(if self.changes.loading {
                                                "…".to_string()
                                            } else {
                                                format!("{staged} staged · {unstaged} unstaged")
                                            })
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(
                                        div()
                                            .id("changes-refresh-btn")
                                            .cursor_pointer()
                                            .px(px(6.0))
                                            .py(px(2.0))
                                            .rounded(px(6.0))
                                            .text_size(px(12.0))
                                            .text_color(theme().text_faint)
                                            .hover(|s| {
                                                s.bg(theme().bg_raised)
                                                    .text_color(theme().text_primary)
                                            })
                                            .child("↻")
                                            .tooltip(|_window, cx| {
                                                cx.new(|_| SimpleTooltip {
                                                    text: "Refresh changes".into(),
                                                })
                                                .into()
                                            })
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this: &mut Self, _event, _window, cx| {
                                                        this.pending_action = Some(
                                                            SidebarAction::RefreshChanges.into(),
                                                        );
                                                        cx.notify();
                                                    },
                                                ),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("right-sidebar-close-btn")
                                            .cursor_pointer()
                                            .p(px(4.0))
                                            .rounded(px(6.0))
                                            .hover(|s| s.bg(theme().bg_raised))
                                            .child(icon(icons::X, 13.0, theme().text_faint))
                                            .tooltip(|_window, cx| {
                                                cx.new(|_| SimpleTooltip {
                                                    text: "Close changes panel".into(),
                                                })
                                                .into()
                                            })
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this: &mut Self, _event, _window, cx| {
                                                        this.pending_action = Some(
                                                            SidebarAction::ToggleRightSidebar
                                                                .into(),
                                                        );
                                                        cx.notify();
                                                    },
                                                ),
                                            ),
                                    ),
                            ),
                    )
                    // Body: git changes file list + diff pane
                    .child(self.render_changes_panel_body(cx)),
            );
        }

        // Outer wrapper: non-flex, relative-positioned container hosting both
        // the flex row and the optional drag overlay as siblings.
        let mut outer = div().id("app-outer").size_full().relative().child(flex_row);

        // Sidebar drag overlay
        if is_resizing {
            outer = outer.child(
                div()
                    .id("sidebar-drag-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor_col_resize()
                    .on_mouse_move(cx.listener(
                        |this: &mut Self, event: &MouseMoveEvent, window, cx| {
                            let viewport_w = f32::from(window.viewport_size().width);
                            let max = (viewport_w - 100.0).max(SIDEBAR_MIN_WIDTH);
                            let new_width =
                                f32::from(event.position.x).clamp(SIDEBAR_MIN_WIDTH, max);
                            if (new_width - this.sidebar.width).abs() > 0.5 {
                                this.sidebar.width = new_width;
                                window.refresh();
                                cx.notify();
                            }
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                            this.sidebar.resizing = false;
                            this.mark_settings_dirty();
                            cx.notify();
                        }),
                    ),
            );
        }

        // Right sidebar drag overlay
        if right_sidebar_resizing {
            outer = outer.child(
                div()
                    .id("right-sidebar-drag-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor_col_resize()
                    .on_mouse_move(cx.listener(
                        |this: &mut Self, event: &MouseMoveEvent, window, cx| {
                            let viewport_w = f32::from(window.viewport_size().width);
                            let mouse_x = f32::from(event.position.x);
                            // Right sidebar width = distance from right edge to mouse
                            let max = (viewport_w - 200.0).max(RIGHT_SIDEBAR_MIN_WIDTH);
                            let new_width =
                                (viewport_w - mouse_x).clamp(RIGHT_SIDEBAR_MIN_WIDTH, max);
                            if (new_width - this.right_panel.width).abs() > 0.5 {
                                this.right_panel.width = new_width;
                                window.refresh();
                                cx.notify();
                            }
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                            this.right_panel.resizing = false;
                            this.mark_settings_dirty();
                            cx.notify();
                        }),
                    ),
            );
        }

        // Drawer drag overlay
        if drawer_is_resizing {
            outer = outer.child(crate::drawer::build_drawer_drag_overlay(self, window, cx));
        }

        if let Some(pad) = self.scratch_pad.clone() {
            outer = outer.child(pad);
        }

        if self.file_palette.is_some() {
            outer = outer.child(self.render_file_palette(cx));
        }

        if self.search.is_some() {
            outer = outer.child(self.render_search(cx));
        }

        if self.command_palette.is_some() {
            outer = outer.child(self.render_command_palette(cx));
        }

        if let Some(modal) = self.new_session_modal.clone() {
            outer = outer.child(modal);
        }

        if let Some(modal) = self.edit_session_modal.clone() {
            outer = outer.child(modal);
        }

        if let Some(modal) = self.naming_modal.clone() {
            outer = outer.child(modal);
        }

        if let Some(modal) = self.conversation_picker.clone() {
            outer = outer.child(modal);
        }

        if let Some(browser) = self.remote_browser.clone() {
            outer = outer.child(browser);
        }

        outer = outer.child(self.render_session_context_menu(cx));
        outer = outer.child(self.render_project_context_menu(cx));

        // If a session has naming suggestions pending and no modal is open, spawn one.
        if self.naming_modal.is_none() {
            let pending = self
                .projects
                .iter()
                .flat_map(|p| &p.sessions)
                .find(|s| s.naming_suggestions.is_some())
                .map(|s| (s.id.clone(), s.naming_suggestions.clone().unwrap()));
            if let Some((session_id, suggestions)) = pending {
                let entity = cx.new(|cx| {
                    new_session_modal::NamingModal::new(cx, session_id.clone(), suggestions)
                });
                let sid = session_id.clone();
                cx.subscribe(&entity, move |this: &mut Self, _modal, event: &new_session_modal::NamingModalEvent, cx| {
                    match event {
                        new_session_modal::NamingModalEvent::Pick { session_id, slug } => {
                            let short_id: String = session_id.chars().take(8).collect();
                            let branch_name = naming::branch_name_from_slug(slug, &short_id);
                            let display_label = naming::slug_to_label(slug);

                            // Rename the branch — unless the user pinned a
                            // specific branch, in which case leave it untouched.
                            for project in &this.projects {
                                for session in &project.sessions {
                                    if session.id == *session_id {
                                        if !session.branch_locked {
                                            if let Some(ref cp) = session.clone_path {
                                                if let Err(e) = git::rename_session_branch(cp, session_id, &branch_name) {
                                                    tracing::warn!("naming modal: branch rename failed: {e}");
                                                }
                                            }
                                        }
                                        break;
                                    }
                                }
                            }

                            // Update session state
                            for project in &mut this.projects {
                                for session in &mut project.sessions {
                                    if session.id == *session_id {
                                        session.label = display_label.clone();
                                        if !session.branch_locked {
                                            session.branch_name = Some(branch_name.clone());
                                        }
                                        session.naming_suggestions = None;
                                        break;
                                    }
                                }
                            }
                            this.naming_modal = None;
                            this.mark_state_dirty();
                            cx.notify();
                        }
                        new_session_modal::NamingModalEvent::Close => {
                            // Clear suggestions without renaming
                            for project in &mut this.projects {
                                for session in &mut project.sessions {
                                    if session.id == sid {
                                        session.naming_suggestions = None;
                                        break;
                                    }
                                }
                            }
                            this.naming_modal = None;
                            cx.notify();
                        }
                    }
                }).detach();
                self.naming_modal = Some(entity);
            }
        }

        // Coalesce per-frame mutations into at most one write per file.
        // See ARCHITECTURE.md §3.4.
        self.checkpoint_persistence();

        outer
    }
}
