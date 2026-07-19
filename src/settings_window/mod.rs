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

use crate::theme::theme;
use gpui::*;

use crate::settings::AgentConfig;
use crate::AppState;

mod agents;
mod appearance;
mod browser;
mod editor;
mod infrastructure;
mod naming;
mod projects;
mod sessions;
mod widgets;
use agents::AgentsSection;
use appearance::AppearanceSection;
use browser::BrowserSection;
use editor::EditorSection;
use infrastructure::InfraSection;
use naming::NamingSection;
use projects::ProjectsSection;
use sessions::SessionsSection;

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
    /// Projects section (per-project orchestration).
    projects: ProjectsSection,
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
            projects: ProjectsSection::new(cx),
        };
        s.agents.sync_inputs(cx);
        s
    }
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
        Section::Projects => this.projects.render(&this.app, cx).into_any_element(),
        Section::Infrastructure => this.infrastructure.render(&this.app, cx).into_any_element(),
        Section::Sessions => this.sessions.render(cx).into_any_element(),
        Section::Agents => this.agents.render(cx).into_any_element(),
        Section::Naming => this.naming.render(&this.app, cx).into_any_element(),
        Section::Editor => this.editor.render(cx).into_any_element(),
        Section::Browser => this.browser.render(cx).into_any_element(),
        Section::Appearance => this.appearance.render(cx).into_any_element(),
    }
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
