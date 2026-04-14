//! Native Settings window — a standalone GPUI entity opened from the
//! "Allele → Settings…" menu item.
//!
//! Layout matches the platform convention: section list on the left,
//! editor pane for the selected section on the right. Today there is one
//! section — Sessions — whose sole control is the cleanup-paths list
//! (paths purged from every fresh session clone to avoid inherited
//! Overmind/pid files etc.). Additional sections (Sounds, Appearance)
//! can slot in later without restructuring the window.
//!
//! The window owns no persistent state. It mirrors
//! `AppState.user_settings.session_cleanup_paths` into local vectors for
//! rendering and pushes every mutation back through a
//! `PendingAction::UpdateCleanupPaths`, so the main window remains the
//! single source of truth for settings and persistence.
//!
//! Text input here is intentionally minimal (no IME/selection/paste) —
//! same hand-rolled `on_key_down` pattern used for drawer-tab rename.

use gpui::*;

use crate::AppState;

/// Which section is currently selected in the left-hand list. Only one
/// entry today, but modelled as an enum so adding more sections later is
/// a local change.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Sessions,
    Editor,
    Browser,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Sessions => "Sessions",
            Section::Editor => "Editor",
            Section::Browser => "Browser",
        }
    }
}

pub struct SettingsWindowState {
    /// Weak handle back to the main window's AppState so we can route
    /// mutations + trigger `Settings::save()`. Weak so this window can
    /// outlive the app temporarily without dangling.
    app: WeakEntity<AppState>,
    selected: Section,
    /// Local mirror of `session_cleanup_paths` for rendering. Kept in sync
    /// by calling `push_cleanup_paths` on every edit.
    cleanup_paths: Vec<String>,
    /// Text buffer for the "add new entry" input.
    draft: String,
    /// Focus handle for the draft input. Created lazily on first render.
    draft_focus: Option<FocusHandle>,
    /// Live text buffer for the external-editor command input.
    external_editor: String,
    /// Focus handle for the external-editor input.
    external_editor_focus: Option<FocusHandle>,
    /// Whether Chrome browser integration is enabled. Mirrored from
    /// `Settings::browser_integration_enabled`; pushed back via
    /// `UpdateBrowserIntegration` on toggle.
    browser_integration_enabled: bool,
}

impl SettingsWindowState {
    pub fn new(
        app: WeakEntity<AppState>,
        initial_paths: Vec<String>,
        initial_external_editor: String,
        initial_browser_integration: bool,
    ) -> Self {
        Self {
            app,
            selected: Section::Sessions,
            cleanup_paths: initial_paths,
            draft: String::new(),
            draft_focus: None,
            external_editor: initial_external_editor,
            external_editor_focus: None,
            browser_integration_enabled: initial_browser_integration,
        }
    }

    /// Send the updated list back to AppState. AppState applies the change
    /// to `user_settings` and persists via `Settings::save()`.
    fn push_cleanup_paths(&self, cx: &mut Context<Self>) {
        let paths = self.cleanup_paths.clone();
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::PendingAction::UpdateCleanupPaths(paths));
                cx.notify();
            })
            .ok();
    }

    fn commit_draft(&mut self, cx: &mut Context<Self>) {
        let trimmed = self.draft.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        // Ignore duplicates — the list is a set in spirit.
        if !self.cleanup_paths.iter().any(|p| p == &trimmed) {
            self.cleanup_paths.push(trimmed);
            self.push_cleanup_paths(cx);
        }
        self.draft.clear();
        cx.notify();
    }

    fn remove_path(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.cleanup_paths.len() {
            self.cleanup_paths.remove(idx);
            self.push_cleanup_paths(cx);
            cx.notify();
        }
    }

    fn push_external_editor(&self, cx: &mut Context<Self>) {
        let value = self.external_editor.clone();
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::PendingAction::UpdateExternalEditor(value));
                cx.notify();
            })
            .ok();
    }

    fn push_browser_integration(&self, cx: &mut Context<Self>) {
        let value = self.browser_integration_enabled;
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::PendingAction::UpdateBrowserIntegration(value));
                cx.notify();
            })
            .ok();
    }
}

impl Render for SettingsWindowState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(render_sidebar(self.selected, cx))
            .child(render_pane(self, cx))
    }
}

fn render_sidebar(selected: Section, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
    let sections = [Section::Sessions, Section::Editor, Section::Browser];

    let mut list = div()
        .flex()
        .flex_col()
        .w(px(180.0))
        .h_full()
        .py(px(12.0))
        .border_r_1()
        .border_color(rgb(0x313244))
        .bg(rgb(0x181825));

    for section in sections {
        let is_selected = section == selected;
        let id: SharedString =
            format!("settings-section-{}", section.label()).into();
        let row = div()
            .id(id)
            .px(px(14.0))
            .py(px(6.0))
            .text_size(px(12.0))
            .cursor_pointer()
            .text_color(if is_selected {
                rgb(0xcdd6f4)
            } else {
                rgb(0xa6adc8)
            })
            .bg(if is_selected {
                rgb(0x313244)
            } else {
                rgb(0x181825)
            })
            .hover(|s| s.bg(rgb(0x313244)))
            .child(section.label())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.selected = section;
                    cx.notify();
                }),
            );
        list = list.child(row);
    }

    list
}

fn render_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> AnyElement {
    match this.selected {
        Section::Sessions => render_sessions_pane(this, cx).into_any_element(),
        Section::Editor => render_editor_pane(this, cx).into_any_element(),
        Section::Browser => render_browser_pane(this, cx).into_any_element(),
    }
}

fn render_browser_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let enabled = this.browser_integration_enabled;

    let toggle = div()
        .id("browser-toggle")
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.browser_integration_enabled = !this.browser_integration_enabled;
                this.push_browser_integration(cx);
                cx.notify();
            }),
        )
        .child(
            // Small track + knob toggle, styled consistently with the rest
            // of the dark theme. Coloured knob is on the right when enabled.
            div()
                .w(px(32.0))
                .h(px(18.0))
                .rounded(px(9.0))
                .bg(if enabled { rgb(0x89b4fa) } else { rgb(0x45475a) })
                .flex()
                .items_center()
                .justify_start()
                .px(px(2.0))
                .child(
                    div()
                        .w(px(14.0))
                        .h(px(14.0))
                        .rounded(px(7.0))
                        .bg(rgb(0x1e1e2e))
                        .ml(if enabled { px(14.0) } else { px(0.0) }),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4))
                .child(if enabled {
                    "Enabled"
                } else {
                    "Disabled"
                }),
        );

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .p(px(20.0))
        .gap(px(12.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(0xcdd6f4))
                .child("Browser"),
        )
        .child(
            div()
                .w_full()
                .text_size(px(12.0))
                .text_color(rgb(0xa6adc8))
                .child(
                    "Link each Allele session to a tab in your running \
                     Google Chrome. Switching sessions activates the \
                     matching tab; new sessions open a tab at the project's \
                     allele.json preview URL. Uses AppleScript against your \
                     real Chrome (first use prompts for Automation \
                     permission). When disabled, preview URLs fall back to \
                     your system default browser.",
                ),
        )
        .child(toggle)
}

fn render_editor_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let focus = this
        .external_editor_focus
        .get_or_insert_with(|| cx.focus_handle())
        .clone();

    let display_is_placeholder = this.external_editor.is_empty();
    let display = if display_is_placeholder {
        crate::settings::DEFAULT_EXTERNAL_EDITOR.to_string()
    } else {
        this.external_editor.clone()
    };

    let input = div()
        .w_full()
        .track_focus(&focus)
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(0x45475a))
        .bg(rgb(0x11111b))
        .text_size(px(12.0))
        .text_color(if display_is_placeholder {
            rgb(0x585b70)
        } else {
            rgb(0xcdd6f4)
        })
        .child(display)
        .on_key_down(cx.listener(
            move |this, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;
                match key {
                    "enter" => {
                        this.push_external_editor(cx);
                    }
                    "backspace" => {
                        this.external_editor.pop();
                        this.push_external_editor(cx);
                        cx.notify();
                    }
                    _ => {
                        if let Some(ch) = event.keystroke.key_char.as_ref() {
                            if !mods.control && !mods.platform {
                                this.external_editor.push_str(ch);
                                this.push_external_editor(cx);
                                cx.notify();
                            }
                        }
                    }
                }
            },
        ));

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .p(px(20.0))
        .gap(px(12.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(0xcdd6f4))
                .child("Editor"),
        )
        .child(
            div()
                .w_full()
                .text_size(px(12.0))
                .text_color(rgb(0xa6adc8))
                .child(
                    "External editor command — used by \"Open in External Editor\" \
                     in the file tree's right-click menu. Bare binary name (e.g. \
                     `subl`, `code`, `mate`) if on PATH, or an absolute path. \
                     Leave blank to use the default (Sublime Text's `subl`).",
                ),
        )
        .child(input)
}

fn render_sessions_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    // Lazily create the focus handle for the add-input the first time we
    // render — matches the pattern used for drawer-tab rename in main.rs.
    let draft_focus = this
        .draft_focus
        .get_or_insert_with(|| cx.focus_handle())
        .clone();

    let mut list = div().flex().flex_col().w_full().gap(px(4.0));
    for (idx, path) in this.cleanup_paths.iter().enumerate() {
        let row = div()
            .flex()
            .flex_row()
            .w_full()
            .min_w(px(0.0))
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .bg(rgb(0x181825))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(12.0))
                    .text_color(rgb(0xcdd6f4))
                    .child(path.clone()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("cleanup-remove-{idx}")))
                    .cursor_pointer()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(3.0))
                    .text_size(px(11.0))
                    .text_color(rgb(0x6c7086))
                    .hover(|s| s.text_color(rgb(0xf38ba8)))
                    .child("✕")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.remove_path(idx, cx);
                        }),
                    ),
            );
        list = list.child(row);
    }

    let draft_display = if this.draft.is_empty() {
        "Add a path (e.g. tmp/pids/server.pid)".to_string()
    } else {
        this.draft.clone()
    };
    let draft_is_placeholder = this.draft.is_empty();

    let input = div()
        .flex_1()
        .min_w(px(0.0))
        .track_focus(&draft_focus)
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(0x45475a))
        .bg(rgb(0x11111b))
        .text_size(px(12.0))
        .text_color(if draft_is_placeholder {
            rgb(0x585b70)
        } else {
            rgb(0xcdd6f4)
        })
        .child(draft_display)
        .on_key_down(cx.listener(
            move |this, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;
                match key {
                    "enter" => {
                        this.commit_draft(cx);
                    }
                    "backspace" => {
                        this.draft.pop();
                        cx.notify();
                    }
                    _ => {
                        if let Some(ch) = event.keystroke.key_char.as_ref() {
                            if !mods.control && !mods.platform {
                                this.draft.push_str(ch);
                                cx.notify();
                            }
                        }
                    }
                }
            },
        ));

    let add_button = div()
        .id("cleanup-add")
        .cursor_pointer()
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(rgb(0x89b4fa))
        .text_size(px(12.0))
        .text_color(rgb(0x1e1e2e))
        .hover(|s| s.bg(rgb(0xb4befe)))
        .child("Add")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.commit_draft(cx);
            }),
        );

    div()
        .flex()
        .flex_col()
        .flex_1()
        // Flex items default to min-width:auto (content-size), which lets
        // long rows blow past the window width. Force shrink-to-parent so
        // inner text wraps instead of overflowing horizontally.
        .min_w(px(0.0))
        .overflow_hidden()
        .p(px(20.0))
        .gap(px(12.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(0xcdd6f4))
                .child("Sessions"),
        )
        .child(
            div()
                .w_full()
                .text_size(px(12.0))
                .text_color(rgb(0xa6adc8))
                .child(
                    "Cleanup paths — deleted from each new session clone. \
                     Useful for stale runtime files that the parent working \
                     tree left behind (e.g. .overmind.sock, \
                     tmp/pids/server.pid).",
                ),
        )
        .child(list.w_full())
        .child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .min_w(px(0.0))
                .gap(px(8.0))
                .items_center()
                .child(input)
                .child(add_button),
        )
}

/// Open the Settings window, or focus the existing one if it's already
/// visible. Returns the window handle so the caller can track it on
/// `AppState`.
pub fn open_settings_window(
    cx: &mut App,
    app: WeakEntity<AppState>,
    initial_paths: Vec<String>,
    initial_external_editor: String,
    initial_browser_integration: bool,
) -> anyhow::Result<WindowHandle<SettingsWindowState>> {
    // Size tuned to the default content: 180px section list + a pane with
    // a short description and three default cleanup entries. Leaves enough
    // vertical room for ~10 entries before scrolling would be needed.
    let window_size = size(px(560.0), px(380.0));
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Allele Settings".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(480.0), px(320.0))),
        window_bounds: Some(WindowBounds::centered(window_size, cx)),
        ..Default::default()
    };

    cx.open_window(options, move |_window, cx| {
        cx.new(move |_cx| {
            SettingsWindowState::new(
                app,
                initial_paths,
                initial_external_editor,
                initial_browser_integration,
            )
        })
    })
}
