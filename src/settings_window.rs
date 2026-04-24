//! Native Settings window — a standalone GPUI entity opened from the
//! "Allele → Settings…" menu item.
//!
//! Layout matches the platform convention: section list on the left,
//! editor pane for the selected section on the right. Sections today:
//! Sessions (cleanup paths), Agents (coding-agent registry), Editor
//! (external editor command), Browser (Chrome integration toggle).
//!
//! The window owns no persistent state. It mirrors the relevant fields
//! from `AppState.user_settings` and pushes every mutation back through
//! a `PendingAction::*`, so the main window remains the single source
//! of truth for settings and persistence.
//!
//! All text fields use the reusable `text_input::TextInput` component,
//! which gives proper cursor, drag-to-select, paste, IME, and arrow
//! navigation. The settings window subscribes to each input's
//! `Changed` / `Submitted` events to push state out.

use gpui::*;

use crate::AppState;
use crate::agents;
use crate::settings::{AgentConfig, AgentKind};
use crate::text_input::{TextInput, TextInputEvent};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Sessions,
    Agents,
    Editor,
    Browser,
    Appearance,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Sessions => "Sessions",
            Section::Agents => "Agents",
            Section::Editor => "Editor",
            Section::Browser => "Browser",
            Section::Appearance => "Appearance",
        }
    }
}

/// Per-agent text-input bundle. Entities are persistent across renders
/// so cursor / selection / focus state survives every `notify`.
struct AgentInputs {
    id: String,
    name: Entity<TextInput>,
    path: Entity<TextInput>,
    args: Entity<TextInput>,
}

pub struct SettingsWindowState {
    app: WeakEntity<AppState>,
    selected: Section,
    /// Local mirror of `session_cleanup_paths`, kept in sync via
    /// `push_cleanup_paths` on every edit.
    cleanup_paths: Vec<String>,
    /// "Add cleanup path" input.
    draft_input: Entity<TextInput>,
    /// External-editor command input.
    external_editor_input: Entity<TextInput>,
    /// Mirrored browser-integration toggle.
    browser_integration_enabled: bool,
    /// Local mirror of the agents list + default, pushed back on every
    /// edit via `UpdateAgents`.
    agents: Vec<AgentConfig>,
    default_agent: Option<String>,
    /// Per-agent input entities, kept in lockstep with `agents` by
    /// `sync_agent_inputs`. Indexed by agent id so reordering or
    /// removing entries doesn't churn focus state on unrelated rows.
    agent_inputs: Vec<AgentInputs>,
    /// Global terminal font size (points). Clamped 8–32. Mirrored from
    /// `Settings::font_size`; pushed back via `UpdateFontSize`.
    font_size: f32,
    /// Whether to run `git pull` on each project's source root before
    /// creating a new session clone. Mirrored from
    /// `Settings::git_pull_before_new_session`; pushed back via
    /// `UpdateGitPullBeforeNewSession`.
    git_pull_before_new_session: bool,
    /// Whether attention-needed sessions are promoted to the top of the
    /// sidebar list. Mirrored from `Settings::promote_attention_sessions`.
    promote_attention_sessions: bool,
}

impl SettingsWindowState {
    pub fn new(
        cx: &mut Context<Self>,
        app: WeakEntity<AppState>,
        initial_paths: Vec<String>,
        initial_external_editor: String,
        initial_browser_integration: bool,
        initial_agents: Vec<AgentConfig>,
        initial_default_agent: Option<String>,
        initial_font_size: f32,
        initial_git_pull_before_new_session: bool,
        initial_promote_attention_sessions: bool,
    ) -> Self {
        let draft_input = cx.new(|cx| {
            TextInput::new(cx, "", "Add a path (e.g. tmp/pids/server.pid)")
        });
        cx.subscribe(&draft_input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                this.commit_draft(value, cx);
                input.update(cx, |i, cx| i.set_text_silent("", cx));
            }
        })
        .detach();

        let external_editor_input = cx.new(|cx| {
            TextInput::new(
                cx,
                initial_external_editor,
                crate::settings::DEFAULT_EXTERNAL_EDITOR,
            )
        });
        cx.subscribe(
            &external_editor_input,
            |this, input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    let value = input.read(cx).text().to_string();
                    this.push_external_editor(value, cx);
                }
            },
        )
        .detach();

        let mut s = Self {
            app,
            selected: Section::Sessions,
            cleanup_paths: initial_paths,
            draft_input,
            external_editor_input,
            browser_integration_enabled: initial_browser_integration,
            agents: initial_agents,
            default_agent: initial_default_agent,
            agent_inputs: Vec::new(),
            font_size: crate::terminal::clamp_font_size(initial_font_size),
            git_pull_before_new_session: initial_git_pull_before_new_session,
            promote_attention_sessions: initial_promote_attention_sessions,
        };
        s.sync_agent_inputs(cx);
        s
    }

    // --- cleanup paths -------------------------------------------------

    fn push_cleanup_paths(&self, cx: &mut Context<Self>) {
        let paths = self.cleanup_paths.clone();
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdateCleanupPaths(paths).into());
                cx.notify();
            })
            .ok();
    }

    fn commit_draft(&mut self, value: String, cx: &mut Context<Self>) {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.cleanup_paths.iter().any(|p| p == trimmed) {
            self.cleanup_paths.push(trimmed.to_string());
            self.push_cleanup_paths(cx);
        }
        cx.notify();
    }

    fn remove_path(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.cleanup_paths.len() {
            self.cleanup_paths.remove(idx);
            self.push_cleanup_paths(cx);
            cx.notify();
        }
    }

    // --- editor --------------------------------------------------------

    fn push_external_editor(&self, value: String, cx: &mut Context<Self>) {
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdateExternalEditor(value).into());
                cx.notify();
            })
            .ok();
    }

    // --- font size ------------------------------------------------------

    fn push_font_size(&self, cx: &mut Context<Self>) {
        let value = self.font_size;
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action = Some(crate::SettingsAction::UpdateFontSize(value).into());
                cx.notify();
            })
            .ok();
    }

    fn adjust_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let new_size = crate::terminal::clamp_font_size(self.font_size + delta);
        if (new_size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = new_size;
        self.push_font_size(cx);
        cx.notify();
    }

    fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        let default = crate::terminal::DEFAULT_FONT_SIZE;
        if (default - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = default;
        self.push_font_size(cx);
        cx.notify();
    }

    fn push_git_pull_before_new_session(&self, cx: &mut Context<Self>) {
        let value = self.git_pull_before_new_session;
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdateGitPullBeforeNewSession(value).into());
                cx.notify();
            })
            .ok();
    }

    fn push_promote_attention_sessions(&self, cx: &mut Context<Self>) {
        let value = self.promote_attention_sessions;
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdatePromoteAttentionSessions(value).into());
                cx.notify();
            })
            .ok();
    }

    // --- browser -------------------------------------------------------

    fn push_browser_integration(&self, cx: &mut Context<Self>) {
        let value = self.browser_integration_enabled;
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdateBrowserIntegration(value).into());
                cx.notify();
            })
            .ok();
    }

    // --- agents --------------------------------------------------------

    fn push_agents(&self, cx: &mut Context<Self>) {
        let agents = self.agents.clone();
        let default_agent = self.default_agent.clone();
        self.app
            .update(cx, |state: &mut AppState, cx| {
                state.pending_action =
                    Some(crate::SettingsAction::UpdateAgents { agents, default_agent }.into());
                cx.notify();
            })
            .ok();
    }

    fn ensure_default_valid(&mut self) {
        let valid = self
            .default_agent
            .as_deref()
            .map(|id| self.agents.iter().any(|a| a.id == id && a.enabled))
            .unwrap_or(false);
        if !valid {
            self.default_agent = self
                .agents
                .iter()
                .find(|a| a.enabled && a.path.is_some())
                .or_else(|| self.agents.iter().find(|a| a.enabled))
                .map(|a| a.id.clone());
        }
    }

    /// Reconcile `agent_inputs` with `agents`: keep entities for ids
    /// that still exist (preserves cursor/selection on unrelated rows
    /// when one is added or removed), create entities for new ids,
    /// drop entities for removed ids. Reorders to match `agents`.
    fn sync_agent_inputs(&mut self, cx: &mut Context<Self>) {
        let mut next: Vec<AgentInputs> = Vec::with_capacity(self.agents.len());
        for agent in &self.agents {
            if let Some(pos) = self.agent_inputs.iter().position(|a| a.id == agent.id) {
                let existing = self.agent_inputs.remove(pos);
                // Defensive: if the underlying agent was edited from
                // outside the window (allele.json reload, etc.), refresh
                // the input contents without firing Changed.
                let name_text = existing.name.read(cx).text().to_string();
                if name_text != agent.display_name {
                    existing.name.update(cx, |i, cx| {
                        i.set_text_silent(agent.display_name.clone(), cx)
                    });
                }
                let path_text = existing.path.read(cx).text().to_string();
                let path_value = agent.path.clone().unwrap_or_default();
                if path_text != path_value {
                    existing
                        .path
                        .update(cx, |i, cx| i.set_text_silent(path_value, cx));
                }
                let args_text = existing.args.read(cx).text().to_string();
                let args_value = agent.extra_args.join(" ");
                if args_text != args_value {
                    existing
                        .args
                        .update(cx, |i, cx| i.set_text_silent(args_value, cx));
                }
                next.push(existing);
            } else {
                let agent_id = agent.id.clone();
                let name = cx.new(|cx| TextInput::new(cx, agent.display_name.clone(), "Display name"));
                let path = cx.new(|cx| {
                    TextInput::new(
                        cx,
                        agent.path.clone().unwrap_or_default(),
                        "Path to binary (leave blank to auto-detect)",
                    )
                });
                let args = cx.new(|cx| {
                    TextInput::new(
                        cx,
                        agent.extra_args.join(" "),
                        "Extra args (space-separated, e.g. --dangerously-skip-permissions)",
                    )
                });
                let id_for_name = agent_id.clone();
                cx.subscribe(&name, move |this, input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                        let value = input.read(cx).text().to_string();
                        if let Some(a) = this.agents.iter_mut().find(|a| a.id == id_for_name) {
                            a.display_name = value;
                        }
                        this.push_agents(cx);
                    }
                })
                .detach();
                let id_for_path = agent_id.clone();
                cx.subscribe(&path, move |this, input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                        let value = input.read(cx).text().to_string();
                        if let Some(a) = this.agents.iter_mut().find(|a| a.id == id_for_path) {
                            a.path = if value.is_empty() { None } else { Some(value) };
                        }
                        this.push_agents(cx);
                    }
                })
                .detach();
                let id_for_args = agent_id.clone();
                cx.subscribe(&args, move |this, input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                        let value = input.read(cx).text().to_string();
                        if let Some(a) = this.agents.iter_mut().find(|a| a.id == id_for_args) {
                            a.extra_args = split_args(&value);
                        }
                        this.push_agents(cx);
                    }
                })
                .detach();
                next.push(AgentInputs { id: agent_id, name, path, args });
            }
        }
        self.agent_inputs = next;
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
    let sections = [
        Section::Sessions,
        Section::Agents,
        Section::Editor,
        Section::Browser,
        Section::Appearance,
    ];

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
        let id: SharedString = format!("settings-section-{}", section.label()).into();
        let row = div()
            .id(id)
            .px(px(14.0))
            .py(px(6.0))
            .text_size(px(12.0))
            .cursor_pointer()
            .text_color(if is_selected { rgb(0xcdd6f4) } else { rgb(0xa6adc8) })
            .bg(if is_selected { rgb(0x313244) } else { rgb(0x181825) })
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
        Section::Agents => render_agents_pane(this, cx).into_any_element(),
        Section::Editor => render_editor_pane(this, cx).into_any_element(),
        Section::Browser => render_browser_pane(this, cx).into_any_element(),
        Section::Appearance => render_appearance_pane(this, cx).into_any_element(),
    }
}

fn input_frame(child: Entity<TextInput>) -> Div {
    div()
        .flex_1()
        .min_w(px(0.0))
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(0x45475a))
        .bg(rgb(0x11111b))
        .text_size(px(12.0))
        .text_color(rgb(0xcdd6f4))
        .overflow_hidden()
        .child(child)
}

fn render_appearance_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let size = this.font_size;
    let min = crate::terminal::MIN_FONT_SIZE;
    let max = crate::terminal::MAX_FONT_SIZE;
    let default = crate::terminal::DEFAULT_FONT_SIZE;
    let at_min = size <= min + f32::EPSILON;
    let at_max = size >= max - f32::EPSILON;
    let at_default = (size - default).abs() < f32::EPSILON;

    let stepper_button = |id: &'static str, label: &'static str, enabled: bool| {
        let base = div()
            .id(SharedString::from(id))
            .w(px(28.0))
            .h(px(24.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(0x45475a))
            .bg(rgb(0x11111b))
            .text_size(px(14.0))
            .text_color(if enabled { rgb(0xcdd6f4) } else { rgb(0x585b70) })
            .child(label);
        if enabled {
            base.cursor_pointer().hover(|s| s.bg(rgb(0x313244)))
        } else {
            base.hover(|s| s)
        }
    };

    let minus = stepper_button("font-size-minus", "−", !at_min).on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, _window, cx| {
            cx.stop_propagation();
            this.adjust_font_size(-1.0, cx);
        }),
    );

    let plus = stepper_button("font-size-plus", "+", !at_max).on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, _window, cx| {
            cx.stop_propagation();
            this.adjust_font_size(1.0, cx);
        }),
    );

    let readout = div()
        .w(px(56.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .bg(rgb(0x181825))
        .text_size(px(12.0))
        .text_color(rgb(0xcdd6f4))
        .child(format!("{:.0} pt", size));

    let reset = {
        let base = div()
            .id("font-size-reset")
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(0x45475a))
            .bg(rgb(0x11111b))
            .text_size(px(11.0))
            .text_color(if at_default { rgb(0x585b70) } else { rgb(0xcdd6f4) })
            .child("Reset");
        if at_default {
            base
        } else {
            base.cursor_pointer().hover(|s| s.bg(rgb(0x313244))).on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.reset_font_size(cx);
                }),
            )
        }
    };

    let controls = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(minus)
        .child(readout)
        .child(plus)
        .child(reset);

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
                .child("Appearance"),
        )
        .child(
            div()
                .w_full()
                .text_size(px(12.0))
                .text_color(rgb(0xa6adc8))
                .child(
                    "Terminal font size — applies to every terminal (sessions \
                     and drawer tabs) live. Cmd+= / Cmd+- / Cmd+0 change this \
                     same value from inside a terminal.",
                ),
        )
        .child(controls)
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
                .child(if enabled { "Enabled" } else { "Disabled" }),
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
    _cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let input = input_frame(this.external_editor_input.clone()).w_full();

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

    let input = input_frame(this.draft_input.clone());

    let pull_enabled = this.git_pull_before_new_session;
    let pull_toggle = div()
        .id("git-pull-toggle")
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.git_pull_before_new_session = !this.git_pull_before_new_session;
                this.push_git_pull_before_new_session(cx);
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(32.0))
                .h(px(18.0))
                .rounded(px(9.0))
                .bg(if pull_enabled { rgb(0x89b4fa) } else { rgb(0x45475a) })
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
                        .ml(if pull_enabled { px(14.0) } else { px(0.0) }),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4))
                .child("Run `git pull` on source before creating a new session"),
        );

    let promote_enabled = this.promote_attention_sessions;
    let promote_toggle = div()
        .id("promote-attention-toggle")
        .cursor_pointer()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.promote_attention_sessions = !this.promote_attention_sessions;
                this.push_promote_attention_sessions(cx);
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(32.0))
                .h(px(18.0))
                .rounded(px(9.0))
                .bg(if promote_enabled { rgb(0x89b4fa) } else { rgb(0x45475a) })
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
                        .ml(if promote_enabled { px(14.0) } else { px(0.0) }),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4))
                .child("Move attention-needed sessions to top of list"),
        );

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
                let value = this.draft_input.read(cx).text().to_string();
                this.commit_draft(value, cx);
                this.draft_input.update(cx, |i, cx| i.set_text_silent("", cx));
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
                .child("Sessions"),
        )
        .child(pull_toggle)
        .child(promote_toggle)
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

fn render_agents_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let default_id = this.default_agent.clone();

    let mut rows = div().flex().flex_col().w_full().gap(px(10.0));
    for (idx, agent) in this.agents.clone().iter().enumerate() {
        let inputs = this
            .agent_inputs
            .iter()
            .find(|i| i.id == agent.id)
            .map(|i| (i.name.clone(), i.path.clone(), i.args.clone()));
        let Some((name_input, path_input, args_input)) = inputs else { continue };
        let is_default = default_id.as_deref() == Some(agent.id.as_str());
        rows = rows.child(render_agent_row(
            agent, idx, is_default, name_input, path_input, args_input, cx,
        ));
    }

    let redetect = div()
        .id("agents-redetect")
        .cursor_pointer()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(rgb(0x45475a))
        .text_size(px(12.0))
        .text_color(rgb(0xcdd6f4))
        .hover(|s| s.bg(rgb(0x585b70)))
        .child("Re-detect")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                for agent in this.agents.iter_mut() {
                    if matches!(agent.kind, AgentKind::Generic) { continue; }
                    let detected = agents::detect_path(agent.kind)
                        .map(|p| p.to_string_lossy().to_string());
                    if agent.path.is_none() || agent.path.as_deref() == Some("") {
                        agent.path = detected;
                    } else if let Some(d) = detected {
                        if !std::path::Path::new(agent.path.as_deref().unwrap_or("")).exists() {
                            agent.path = Some(d);
                        }
                    }
                }
                this.sync_agent_inputs(cx);
                this.push_agents(cx);
                cx.notify();
            }),
        );

    let add_custom = div()
        .id("agents-add-custom")
        .cursor_pointer()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(rgb(0x89b4fa))
        .text_size(px(12.0))
        .text_color(rgb(0x1e1e2e))
        .hover(|s| s.bg(rgb(0xb4befe)))
        .child("+ Add custom")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                let base = "custom";
                let mut n = 1;
                let id = loop {
                    let candidate = if n == 1 { base.to_string() } else { format!("{base}-{n}") };
                    if !this.agents.iter().any(|a| a.id == candidate) { break candidate; }
                    n += 1;
                };
                let display = if n == 1 { "Custom".to_string() } else { format!("Custom {n}") };
                this.agents.push(AgentConfig {
                    id,
                    kind: AgentKind::Generic,
                    display_name: display,
                    path: None,
                    extra_args: Vec::new(),
                    enabled: true,
                });
                this.sync_agent_inputs(cx);
                this.push_agents(cx);
                cx.notify();
            }),
        );

    let toolbar = div().flex().flex_row().gap(px(8.0)).child(redetect).child(add_custom);

    div()
        .id("agents-pane-scroll")
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .overflow_y_scroll()
        .p(px(20.0))
        .gap(px(12.0))
        .child(
            div()
                .text_size(px(16.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(0xcdd6f4))
                .child("Coding Agents"),
        )
        .child(
            div()
                .w_full()
                .text_size(px(12.0))
                .text_color(rgb(0xa6adc8))
                .child(
                    "Configure which coding agents Allele can launch in a \
                     session. The default is used for every new session; \
                     a project can override it by adding an \"agent\" key \
                     to its allele.json. Extra args are appended to the \
                     built-in args the adapter generates (useful for \
                     flags like --dangerously-skip-permissions).",
                ),
        )
        .child(toolbar)
        .child(rows)
}

#[allow(clippy::too_many_arguments)]
fn render_agent_row(
    agent: &AgentConfig,
    idx: usize,
    is_default: bool,
    name_input: Entity<TextInput>,
    path_input: Entity<TextInput>,
    args_input: Entity<TextInput>,
    cx: &mut Context<SettingsWindowState>,
) -> AnyElement {
    let kind_badge = div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(3.0))
        .bg(rgb(0x313244))
        .text_size(px(10.0))
        .text_color(rgb(0x89b4fa))
        .child(format!("{:?}", agent.kind));

    let enabled = agent.enabled;
    let toggle = div()
        .id(SharedString::from(format!("agent-toggle-{idx}")))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                if let Some(a) = this.agents.get_mut(idx) {
                    a.enabled = !a.enabled;
                }
                this.ensure_default_valid();
                this.push_agents(cx);
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(30.0))
                .h(px(16.0))
                .rounded(px(8.0))
                .bg(if enabled { rgb(0x89b4fa) } else { rgb(0x45475a) })
                .flex()
                .items_center()
                .px(px(2.0))
                .child(
                    div()
                        .w(px(12.0))
                        .h(px(12.0))
                        .rounded(px(6.0))
                        .bg(rgb(0x1e1e2e))
                        .ml(if enabled { px(14.0) } else { px(0.0) }),
                ),
        );

    let default_btn = div()
        .id(SharedString::from(format!("agent-default-{idx}")))
        .cursor_pointer()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(3.0))
        .text_size(px(11.0))
        .bg(if is_default { rgb(0x89b4fa) } else { rgb(0x313244) })
        .text_color(if is_default { rgb(0x1e1e2e) } else { rgb(0xa6adc8) })
        .hover(|s| s.bg(rgb(0x585b70)))
        .child(if is_default { "Default" } else { "Set default" })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                if let Some(a) = this.agents.get(idx) {
                    let id = a.id.clone();
                    this.default_agent = Some(id);
                    this.push_agents(cx);
                    cx.notify();
                }
            }),
        );

    let is_custom = matches!(agent.kind, AgentKind::Generic);
    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(toggle)
        .child(kind_badge)
        .child(input_frame(name_input))
        .child(default_btn);

    if is_custom {
        let delete = div()
            .id(SharedString::from(format!("agent-delete-{idx}")))
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
                    if idx < this.agents.len() {
                        this.agents.remove(idx);
                    }
                    this.ensure_default_valid();
                    this.sync_agent_inputs(cx);
                    this.push_agents(cx);
                    cx.notify();
                }),
            );
        header = header.child(delete);
    }

    let labelled = |label: &'static str, body: Entity<TextInput>| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .w(px(60.0))
                    .text_size(px(11.0))
                    .text_color(rgb(0xa6adc8))
                    .child(label),
            )
            .child(input_frame(body))
    };

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(10.0))
        .rounded(px(6.0))
        .bg(rgb(0x181825))
        .border_1()
        .border_color(rgb(0x313244))
        .child(header)
        .child(labelled("Path", path_input))
        .child(labelled("Args", args_input))
        .into_any_element()
}

/// Minimal shell-ish splitter. Splits on whitespace; preserves quoted
/// spans so `--flag="one two"` stays intact.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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
    initial_agents: Vec<AgentConfig>,
    initial_default_agent: Option<String>,
    initial_font_size: f32,
    initial_git_pull_before_new_session: bool,
    initial_promote_attention_sessions: bool,
) -> anyhow::Result<WindowHandle<SettingsWindowState>> {
    let window_size = size(px(640.0), px(440.0));
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Allele Settings".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(520.0), px(360.0))),
        window_bounds: Some(WindowBounds::centered(window_size, cx)),
        ..Default::default()
    };

    cx.open_window(options, move |_window, cx| {
        cx.new(move |cx| {
            SettingsWindowState::new(
                cx,
                app,
                initial_paths,
                initial_external_editor,
                initial_browser_integration,
                initial_agents,
                initial_default_agent,
                initial_font_size,
                initial_git_pull_before_new_session,
                initial_promote_attention_sessions,
            )
        })
    })
}
