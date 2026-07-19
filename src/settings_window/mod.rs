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

use crate::icon::{icon, name as icons};
use crate::theme::theme;
use gpui::*;

use crate::settings::AgentConfig;
use crate::text_input::{TextInput, TextInputEvent};
use crate::AppState;

mod agents;
mod appearance;
mod browser;
mod editor;
mod infrastructure;
mod naming;
mod sessions;
mod widgets;
use agents::AgentsSection;
use appearance::AppearanceSection;
use browser::BrowserSection;
use editor::EditorSection;
use infrastructure::InfraSection;
use naming::NamingSection;
use sessions::SessionsSection;
use widgets::{card, input_frame, section_header, section_note, section_title};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Projects,
    Infrastructure,
    Sessions,
    Agents,
    Naming,
    Editor,
    Browser,
    Appearance,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Projects => "Projects",
            Section::Infrastructure => "Infrastructure",
            Section::Sessions => "Sessions",
            Section::Agents => "Agents",
            Section::Naming => "Naming",
            Section::Editor => "Editor",
            Section::Browser => "Browser",
            Section::Appearance => "Appearance",
        }
    }
}

pub struct SettingsWindowState {
    app: WeakEntity<AppState>,
    selected: Section,
    /// Sessions section (cleanup paths + creation toggles).
    sessions: SessionsSection,
    /// Editor section (external-editor command).
    editor: EditorSection,
    /// Browser section (Chrome integration toggle).
    browser: BrowserSection,
    /// Agents section (coding-agent registry).
    agents: AgentsSection,
    /// Appearance section (terminal font size).
    appearance: AppearanceSection,
    /// Naming section (branch-naming mode + model overrides).
    naming: NamingSection,
    /// Infrastructure section (base-infra toggle).
    infrastructure: InfraSection,
    /// Selected project index for the Projects pane.
    projects_selected: Option<usize>,
    /// Text inputs for the selected project's startup/shutdown commands.
    project_startup_input: Entity<TextInput>,
    project_shutdown_input: Entity<TextInput>,
    /// Draft terminal entry (label, command) for adding a new terminal.
    project_terminal_label_input: Entity<TextInput>,
    project_terminal_command_input: Entity<TextInput>,
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
        initial_naming_claude_model: String,
        initial_naming_opencode_model: String,
        initial_base_infra_enabled: bool,
    ) -> Self {
        let project_startup_input =
            cx.new(|cx| TextInput::new(cx, "", "e.g. session-start.sh {{unique_port}} {{folder}}"));
        cx.subscribe(
            &project_startup_input,
            |this, input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    let value = input.read(cx).text().to_string();
                    this.push_project_startup(value, cx);
                }
            },
        )
        .detach();

        let project_shutdown_input =
            cx.new(|cx| TextInput::new(cx, "", "e.g. session-stop.sh {{folder}}"));
        cx.subscribe(
            &project_shutdown_input,
            |this, input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    let value = input.read(cx).text().to_string();
                    this.push_project_shutdown(value, cx);
                }
            },
        )
        .detach();

        let project_terminal_label_input = cx.new(|cx| TextInput::new(cx, "", "Label"));
        let project_terminal_command_input = cx.new(|cx| TextInput::new(cx, "", "Command"));

        let mut s = Self {
            app,
            selected: Section::Sessions,
            sessions: SessionsSection::new(
                cx,
                initial_paths,
                initial_git_pull_before_new_session,
                initial_promote_attention_sessions,
            ),
            editor: EditorSection::new(cx, initial_external_editor),
            browser: BrowserSection::new(initial_browser_integration),
            agents: AgentsSection::new(initial_agents, initial_default_agent),
            appearance: AppearanceSection::new(initial_font_size),
            naming: NamingSection::new(
                cx,
                initial_naming_claude_model,
                initial_naming_opencode_model,
            ),
            infrastructure: InfraSection::new(initial_base_infra_enabled),
            projects_selected: None,
            project_startup_input,
            project_shutdown_input,
            project_terminal_label_input,
            project_terminal_command_input,
        };
        s.agents.sync_inputs(cx);
        s
    }

    // --- base infrastructure --------------------------------------------

    // --- project orchestration ------------------------------------------

    fn push_project_settings(&self, cx: &mut Context<Self>) {
        let Some(project_idx) = self.projects_selected else {
            return;
        };
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let settings = app
            .read(cx)
            .projects
            .get(project_idx)
            .map(|p| p.settings.clone());
        if let Some(settings) = settings {
            self.app
                .update(cx, |state: &mut AppState, cx| {
                    state.pending_action = Some(
                        crate::SettingsAction::UpdateProjectSettings {
                            project_idx,
                            settings,
                        }
                        .into(),
                    );
                    cx.notify();
                })
                .ok();
        }
    }

    fn push_project_startup(&self, value: String, cx: &mut Context<Self>) {
        let Some(project_idx) = self.projects_selected else {
            return;
        };
        let startup = if value.trim().is_empty() {
            None
        } else {
            Some(value)
        };
        self.app
            .update(cx, |state: &mut AppState, cx| {
                if let Some(project) = state.projects.get_mut(project_idx) {
                    project.settings.startup = startup;
                }
                state.pending_action = Some(
                    crate::SettingsAction::UpdateProjectSettings {
                        project_idx,
                        settings: state
                            .projects
                            .get(project_idx)
                            .map(|p| p.settings.clone())
                            .unwrap_or_default(),
                    }
                    .into(),
                );
                cx.notify();
            })
            .ok();
    }

    fn push_project_shutdown(&self, value: String, cx: &mut Context<Self>) {
        let Some(project_idx) = self.projects_selected else {
            return;
        };
        let shutdown = if value.trim().is_empty() {
            None
        } else {
            Some(value)
        };
        self.app
            .update(cx, |state: &mut AppState, cx| {
                if let Some(project) = state.projects.get_mut(project_idx) {
                    project.settings.shutdown = shutdown;
                }
                state.pending_action = Some(
                    crate::SettingsAction::UpdateProjectSettings {
                        project_idx,
                        settings: state
                            .projects
                            .get(project_idx)
                            .map(|p| p.settings.clone())
                            .unwrap_or_default(),
                    }
                    .into(),
                );
                cx.notify();
            })
            .ok();
    }

    fn select_project(&mut self, idx: usize, cx: &mut Context<Self>) {
        self.projects_selected = Some(idx);
        let (startup, shutdown) = self
            .app
            .upgrade()
            .and_then(|app| {
                let state = app.read(cx);
                state.projects.get(idx).map(|p| {
                    (
                        p.settings.startup.clone().unwrap_or_default(),
                        p.settings.shutdown.clone().unwrap_or_default(),
                    )
                })
            })
            .unwrap_or_default();
        self.project_startup_input
            .update(cx, |i, cx| i.set_text_silent(&startup, cx));
        self.project_shutdown_input
            .update(cx, |i, cx| i.set_text_silent(&shutdown, cx));
        cx.notify();
    }

    // --- cleanup paths -------------------------------------------------

    // --- naming --------------------------------------------------------

    // --- browser -------------------------------------------------------

    // --- agents --------------------------------------------------------
}

impl Render for SettingsWindowState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(theme().bg_base)
            .text_color(theme().text_primary)
            .child(render_sidebar(self.selected, cx))
            .child(render_pane(self, cx))
    }
}

fn render_sidebar(selected: Section, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
    let sections = [
        Section::Projects,
        Section::Infrastructure,
        Section::Sessions,
        Section::Agents,
        Section::Naming,
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
        .border_color(theme().border_subtle)
        .bg(theme().bg_surface);

    for section in sections {
        let is_selected = section == selected;
        let id: SharedString = format!("settings-section-{}", section.label()).into();
        let row = div()
            .id(id)
            .px(px(14.0))
            .py(px(6.0))
            .text_size(px(12.0))
            .cursor_pointer()
            .text_color(if is_selected {
                theme().text_primary
            } else {
                theme().text_secondary
            })
            .bg(if is_selected {
                theme().bg_raised
            } else {
                theme().bg_surface
            })
            .hover(|s| s.bg(theme().bg_raised))
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
        Section::Projects => render_projects_pane(this, cx).into_any_element(),
        Section::Infrastructure => this.infrastructure.render(&this.app, cx).into_any_element(),
        Section::Sessions => this.sessions.render(cx).into_any_element(),
        Section::Agents => this.agents.render(cx).into_any_element(),
        Section::Naming => this.naming.render(&this.app, cx).into_any_element(),
        Section::Editor => this.editor.render(cx).into_any_element(),
        Section::Browser => this.browser.render(cx).into_any_element(),
        Section::Appearance => this.appearance.render(cx).into_any_element(),
    }
}

fn render_projects_pane(
    this: &mut SettingsWindowState,
    cx: &mut Context<SettingsWindowState>,
) -> impl IntoElement {
    let projects: Vec<(usize, String)> = this
        .app
        .upgrade()
        .map(|app| {
            app.read(cx)
                .projects
                .iter()
                .enumerate()
                .map(|(i, p)| (i, p.name.clone()))
                .collect()
        })
        .unwrap_or_default();

    let selected = this.projects_selected;

    // Project list
    let mut project_list = div().flex().flex_col().gap(px(2.0));
    for (idx, name) in &projects {
        let idx = *idx;
        let is_selected = selected == Some(idx);
        project_list = project_list.child(
            div()
                .id(SharedString::from(format!("project-{idx}")))
                .cursor_pointer()
                .px(px(10.0))
                .py(px(5.0))
                .rounded(px(6.0))
                .bg(if is_selected {
                    theme().bg_raised
                } else {
                    theme().bg_surface
                })
                .hover(|s| s.bg(theme().bg_raised))
                .text_size(px(12.0))
                .text_color(if is_selected {
                    theme().text_primary
                } else {
                    theme().text_secondary
                })
                .child(name.clone())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.select_project(idx, cx);
                    }),
                ),
        );
    }

    // Detail pane for selected project
    let detail = if let Some(sel_idx) = selected {
        let terminals: Vec<crate::config::TerminalCfg> = this
            .app
            .upgrade()
            .and_then(|app| {
                app.read(cx)
                    .projects
                    .get(sel_idx)
                    .map(|p| p.settings.terminals.clone())
            })
            .unwrap_or_default();

        let mut terminal_list = div().flex().flex_col().gap(px(4.0));
        for (t_idx, term) in terminals.iter().enumerate() {
            terminal_list = terminal_list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .bg(theme().bg_surface)
                    .child(
                        div()
                            .w(px(80.0))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme().accent)
                            .child(term.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_size(px(11.0))
                            .text_color(theme().text_secondary)
                            .child(if term.command.is_empty() {
                                "(shell)".to_string()
                            } else {
                                term.command.clone()
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("term-remove-{t_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .hover(|s| s.bg(theme().bg_raised))
                            .child(icon(icons::X, 12.0, theme().text_faint))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    let Some(sel) = this.projects_selected else {
                                        return;
                                    };
                                    this.app
                                        .update(cx, |state: &mut AppState, _cx| {
                                            if let Some(project) = state.projects.get_mut(sel) {
                                                if t_idx < project.settings.terminals.len() {
                                                    project.settings.terminals.remove(t_idx);
                                                }
                                            }
                                        })
                                        .ok();
                                    this.push_project_settings(cx);
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }

        // Add terminal row
        let add_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .w(px(80.0))
                    .child(input_frame(this.project_terminal_label_input.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(input_frame(this.project_terminal_command_input.clone())),
            )
            .child(
                div()
                    .id("add-terminal")
                    .cursor_pointer()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(6.0))
                    .bg(theme().bg_raised)
                    .hover(|s| s.bg(theme().bg_hover))
                    .text_size(px(11.0))
                    .text_color(theme().success)
                    .child("+ Add")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            let label = this
                                .project_terminal_label_input
                                .read(cx)
                                .text()
                                .to_string();
                            let command = this
                                .project_terminal_command_input
                                .read(cx)
                                .text()
                                .to_string();
                            if label.trim().is_empty() {
                                return;
                            }
                            let Some(sel) = this.projects_selected else {
                                return;
                            };
                            this.app
                                .update(cx, |state: &mut AppState, _cx| {
                                    if let Some(project) = state.projects.get_mut(sel) {
                                        project.settings.terminals.push(
                                            crate::config::TerminalCfg {
                                                label: label.trim().to_string(),
                                                command: command.trim().to_string(),
                                            },
                                        );
                                    }
                                })
                                .ok();
                            this.project_terminal_label_input
                                .update(cx, |i, cx| i.set_text_silent("", cx));
                            this.project_terminal_command_input
                                .update(cx, |i, cx| i.set_text_silent("", cx));
                            this.push_project_settings(cx);
                            cx.notify();
                        }),
                    ),
            );

        div()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(section_header("Startup command"))
            .child(input_frame(this.project_startup_input.clone()))
            .child(section_header("Shutdown command"))
            .child(input_frame(this.project_shutdown_input.clone()))
            .child(section_header("Drawer terminals"))
            .child(terminal_list)
            .child(add_row)
            .into_any_element()
    } else {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme().text_faint)
                    .child("Select a project to configure"),
            )
            .into_any_element()
    };

    div()
        .id("projects-pane")
        .flex_1()
        .p(px(20.0))
        .flex()
        .flex_col()
        .gap(px(12.0))
        .overflow_y_scroll()
        .child(section_title("Projects"))
        .child(section_note(
            "Configure per-session orchestration: startup/shutdown lifecycle and drawer terminals.",
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(16.0))
                .flex_1()
                .child(
                    div()
                        .w(px(160.0))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(project_list),
                )
                .child(card().flex_1().child(detail)),
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
    initial_agents: Vec<AgentConfig>,
    initial_default_agent: Option<String>,
    initial_font_size: f32,
    initial_git_pull_before_new_session: bool,
    initial_promote_attention_sessions: bool,
    initial_naming_claude_model: String,
    initial_naming_opencode_model: String,
    initial_base_infra_enabled: bool,
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

    cx.open_window(options, move |window, cx| {
        let app_for_close = app.clone();
        window.on_window_should_close(cx, move |_window, cx| {
            if let Some(strong) = app_for_close.upgrade() {
                strong.update(cx, |state: &mut AppState, _cx| {
                    state.settings_window = None;
                });
            }
            true
        });
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
                initial_naming_claude_model,
                initial_naming_opencode_model,
                initial_base_infra_enabled,
            )
        })
    })
}
