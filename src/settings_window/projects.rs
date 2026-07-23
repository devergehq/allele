//! Projects settings section — per-project orchestration (startup/shutdown
//! commands + drawer terminals), driven by a project selector.

use gpui::*;

use super::widgets::{card, input_frame, section_header, section_note, section_title};
use super::SettingsWindowState;
use crate::icon::{icon, name as icons};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::theme;
use crate::AppState;

/// Owns the project selection + the startup/shutdown/terminal draft inputs.
pub(super) struct ProjectsSection {
    selected: Option<usize>,
    startup_input: Entity<TextInput>,
    shutdown_input: Entity<TextInput>,
    terminal_label_input: Entity<TextInput>,
    terminal_command_input: Entity<TextInput>,
}

impl ProjectsSection {
    pub(super) fn new(cx: &mut Context<SettingsWindowState>) -> Self {
        let startup_input =
            cx.new(|cx| TextInput::new(cx, "", "e.g. session-start.sh {{unique_port}} {{folder}}"));
        cx.subscribe(&startup_input, |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                let value = input.read(cx).text().to_string();
                this.projects.push_startup(value, &this.app, cx);
            }
        })
        .detach();

        let shutdown_input = cx.new(|cx| TextInput::new(cx, "", "e.g. session-stop.sh {{folder}}"));
        cx.subscribe(
            &shutdown_input,
            |this, input, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                    let value = input.read(cx).text().to_string();
                    this.projects.push_shutdown(value, &this.app, cx);
                }
            },
        )
        .detach();

        let terminal_label_input = cx.new(|cx| TextInput::new(cx, "", "Label"));
        let terminal_command_input = cx.new(|cx| TextInput::new(cx, "", "Command"));

        Self {
            selected: None,
            startup_input,
            shutdown_input,
            terminal_label_input,
            terminal_command_input,
        }
    }

    fn push_settings(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let Some(project_idx) = self.selected else {
            return;
        };
        let Some(app_entity) = app.upgrade() else {
            return;
        };
        let settings = app_entity
            .read(cx)
            .projects
            .get(project_idx)
            .map(|p| p.settings.clone());
        if let Some(settings) = settings {
            app.update(cx, |state: &mut AppState, cx| {
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

    fn push_startup(
        &self,
        value: String,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let Some(project_idx) = self.selected else {
            return;
        };
        let startup = if value.trim().is_empty() {
            None
        } else {
            Some(value)
        };
        app.update(cx, |state: &mut AppState, cx| {
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

    fn push_shutdown(
        &self,
        value: String,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        let Some(project_idx) = self.selected else {
            return;
        };
        let shutdown = if value.trim().is_empty() {
            None
        } else {
            Some(value)
        };
        app.update(cx, |state: &mut AppState, cx| {
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

    fn select(
        &mut self,
        idx: usize,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) {
        self.selected = Some(idx);
        let (startup, shutdown) = app
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
        self.startup_input
            .update(cx, |i, cx| i.set_text_silent(&startup, cx));
        self.shutdown_input
            .update(cx, |i, cx| i.set_text_silent(&shutdown, cx));
        cx.notify();
    }

    pub(super) fn render(
        &self,
        app: &WeakEntity<AppState>,
        cx: &mut Context<SettingsWindowState>,
    ) -> impl IntoElement {
        let projects: Vec<(usize, String)> = app
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

        let selected = self.selected;

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
                            this.projects.select(idx, &this.app, cx);
                        }),
                    ),
            );
        }

        // Detail pane for selected project
        let detail = if let Some(sel_idx) = selected {
            // DEV-25: surface where each project's configuration comes from —
            // UI defaults vs project settings vs versioned allele.json.
            let (config_path, config_status, branch_source, remote_source) = app
                .upgrade()
                .and_then(|app| {
                    app.read(cx).projects.get(sel_idx).map(|p| {
                        let path = p.source_path.join("allele.json");
                        let status = if !path.exists() {
                            "Not present; using UI settings"
                        } else if crate::config::ProjectConfig::load(&p.source_path).is_some() {
                            "Loaded successfully"
                        } else {
                            "Invalid; check logs and fix allele.json"
                        };
                        (
                            path,
                            status,
                            if p.settings.default_branch.is_some() {
                                "Project setting"
                            } else {
                                "Auto-detected default"
                            },
                            if p.settings.remote.is_some() {
                                "Project setting"
                            } else {
                                "Global default: origin"
                            },
                        )
                    })
                })
                .unwrap_or_else(|| {
                    (
                        std::path::PathBuf::from("allele.json"),
                        "Unavailable",
                        "Unavailable",
                        "Unavailable",
                    )
                });
            let terminals: Vec<crate::config::TerminalCfg> = app
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
                                        let Some(sel) = this.projects.selected else {
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
                                        this.projects.push_settings(&this.app, cx);
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
                        .child(input_frame(self.terminal_label_input.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(input_frame(self.terminal_command_input.clone())),
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
                                    .projects
                                    .terminal_label_input
                                    .read(cx)
                                    .text()
                                    .to_string();
                                let command = this
                                    .projects
                                    .terminal_command_input
                                    .read(cx)
                                    .text()
                                    .to_string();
                                if label.trim().is_empty() {
                                    return;
                                }
                                let Some(sel) = this.projects.selected else {
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
                                this.projects
                                    .terminal_label_input
                                    .update(cx, |i, cx| i.set_text_silent("", cx));
                                this.projects
                                    .terminal_command_input
                                    .update(cx, |i, cx| i.set_text_silent("", cx));
                                this.projects.push_settings(&this.app, cx);
                                cx.notify();
                            }),
                        ),
                );

            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(16.0))
                .child(section_header("Configuration precedence"))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme().text_secondary)
                        .child("Global defaults are overridden by project settings. Versioned allele.json orchestration takes precedence where supported."),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme().text_faint)
                        .child(format!("Branch: {branch_source} · Remote: {remote_source}")),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme().text_faint)
                        .child(format!("Advanced configuration: {}", config_path.display())),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(if config_status.starts_with("Invalid") {
                            theme().danger
                        } else {
                            theme().text_faint
                        })
                        .child(format!("allele.json: {config_status}")),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme().success)
                        .child("Changes save automatically"),
                )
                .child(section_header("Startup command"))
                .child(input_frame(self.startup_input.clone()))
                .child(section_header("Shutdown command"))
                .child(input_frame(self.shutdown_input.clone()))
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
                "Configure project behavior in one place. Active sources and precedence are shown for the selected project.",
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
}
