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
use crate::AppState;
use gpui::*;

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
    /// Build the window state, deriving every section's initial values from the
    /// app's current `user_settings`. The main window remains the source of
    /// truth; this just mirrors it into per-section state.
    pub fn new(cx: &mut Context<Self>, app: WeakEntity<AppState>) -> Self {
        let settings = app
            .upgrade()
            .map(|app| app.read(cx).user_settings.clone())
            .unwrap_or_default();

        let mut s = Self {
            app,
            selected: Section::Sessions,
            sessions: SessionsSection::new(
                cx,
                settings.session_cleanup_paths.clone(),
                settings.git_pull_before_new_session,
                settings.promote_attention_sessions,
            ),
            editor: EditorSection::new(
                cx,
                settings.external_editor_command.clone().unwrap_or_default(),
            ),
            browser: BrowserSection::new(settings.browser_integration_enabled),
            agents: AgentsSection::new(settings.agents.clone(), settings.default_agent.clone()),
            appearance: AppearanceSection::new(settings.font_size),
            naming: NamingSection::new(
                cx,
                settings.naming.claude.model.clone().unwrap_or_default(),
                settings.naming.opencode.model.clone().unwrap_or_default(),
            ),
            infrastructure: InfraSection::new(settings.base_infra_enabled),
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
        cx.new(move |cx| SettingsWindowState::new(cx, app))
    })
}
