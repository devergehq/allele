//! Coding-agents settings section — the agent registry (enable/disable, default,
//! path/args overrides, add/remove custom agents).

use gpui::*;

use super::widgets::{input_frame, section_note, section_title};
use super::SettingsWindowState;
use crate::agents;
use crate::icon::{icon, name as icons};
use crate::settings::{AgentConfig, AgentKind};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::theme;
use crate::AppState;

/// Per-agent text-input bundle. Entities are persistent across renders so
/// cursor / selection / focus state survives every `notify`.
struct AgentInputs {
    id: String,
    name: Entity<TextInput>,
    path: Entity<TextInput>,
    args: Entity<TextInput>,
}

/// Owns the agents list, the default id, and the per-agent input entities.
pub(super) struct AgentsSection {
    list: Vec<AgentConfig>,
    default_agent: Option<String>,
    /// Per-agent input entities, kept in lockstep with `list` by
    /// [`Self::sync_inputs`], indexed by agent id.
    inputs: Vec<AgentInputs>,
}

impl AgentsSection {
    pub(super) fn new(list: Vec<AgentConfig>, default_agent: Option<String>) -> Self {
        Self {
            list,
            default_agent,
            inputs: Vec::new(),
        }
    }

    fn push(&self, app: &WeakEntity<AppState>, cx: &mut Context<SettingsWindowState>) {
        let agents = self.list.clone();
        let default_agent = self.default_agent.clone();
        app.update(cx, |state: &mut AppState, cx| {
            state.pending_action = Some(
                crate::SettingsAction::UpdateAgents {
                    agents,
                    default_agent,
                }
                .into(),
            );
            cx.notify();
        })
        .ok();
    }

    fn ensure_default_valid(&mut self) {
        let valid = self
            .default_agent
            .as_deref()
            .map(|id| self.list.iter().any(|a| a.id == id && a.enabled))
            .unwrap_or(false);
        if !valid {
            self.default_agent = self
                .list
                .iter()
                .find(|a| a.enabled && a.path.is_some())
                .or_else(|| self.list.iter().find(|a| a.enabled))
                .map(|a| a.id.clone());
        }
    }

    /// Reconcile `inputs` with `list`: keep entities for ids that still exist
    /// (preserves cursor/selection on unrelated rows when one is added or
    /// removed), create entities for new ids, drop entities for removed ids.
    /// Reorders to match `list`.
    pub(super) fn sync_inputs(&mut self, cx: &mut Context<SettingsWindowState>) {
        let mut next: Vec<AgentInputs> = Vec::with_capacity(self.list.len());
        for agent in &self.list {
            if let Some(pos) = self.inputs.iter().position(|a| a.id == agent.id) {
                let existing = self.inputs.remove(pos);
                // Defensive: if the underlying agent was edited from outside the
                // window (allele.json reload, etc.), refresh the input contents
                // without firing Changed.
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
                let name =
                    cx.new(|cx| TextInput::new(cx, agent.display_name.clone(), "Display name"));
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
                        if let Some(a) = this.agents.list.iter_mut().find(|a| a.id == id_for_name) {
                            a.display_name = value;
                        }
                        this.agents.push(&this.app, cx);
                    }
                })
                .detach();
                let id_for_path = agent_id.clone();
                cx.subscribe(&path, move |this, input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                        let value = input.read(cx).text().to_string();
                        if let Some(a) = this.agents.list.iter_mut().find(|a| a.id == id_for_path) {
                            a.path = if value.is_empty() { None } else { Some(value) };
                        }
                        this.agents.push(&this.app, cx);
                    }
                })
                .detach();
                let id_for_args = agent_id.clone();
                cx.subscribe(&args, move |this, input, event: &TextInputEvent, cx| {
                    if matches!(event, TextInputEvent::Changed | TextInputEvent::Submitted) {
                        let value = input.read(cx).text().to_string();
                        if let Some(a) = this.agents.list.iter_mut().find(|a| a.id == id_for_args) {
                            a.extra_args = split_args(&value);
                        }
                        this.agents.push(&this.app, cx);
                    }
                })
                .detach();
                next.push(AgentInputs {
                    id: agent_id,
                    name,
                    path,
                    args,
                });
            }
        }
        self.inputs = next;
    }

    pub(super) fn render(&self, cx: &mut Context<SettingsWindowState>) -> impl IntoElement {
        let default_id = self.default_agent.clone();

        let mut rows = div().flex().flex_col().w_full().gap(px(10.0));
        for (idx, agent) in self.list.clone().iter().enumerate() {
            let inputs = self
                .inputs
                .iter()
                .find(|i| i.id == agent.id)
                .map(|i| (i.name.clone(), i.path.clone(), i.args.clone()));
            let Some((name_input, path_input, args_input)) = inputs else {
                continue;
            };
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
            .rounded(px(6.0))
            .bg(theme().bg_hover)
            .text_size(px(12.0))
            .text_color(theme().text_primary)
            .hover(|s| s.bg(theme().bg_active))
            .child("Re-detect")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    for agent in this.agents.list.iter_mut() {
                        if matches!(agent.kind, AgentKind::Generic) {
                            continue;
                        }
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
                    this.agents.sync_inputs(cx);
                    this.agents.push(&this.app, cx);
                    cx.notify();
                }),
            );

        let add_custom = div()
            .id("agents-add-custom")
            .cursor_pointer()
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(6.0))
            .bg(theme().accent)
            .text_size(px(12.0))
            .text_color(theme().text_on_accent)
            .hover(|s| s.bg(theme().lavender))
            .child("+ Add custom")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    let base = "custom";
                    let mut n = 1;
                    let id = loop {
                        let candidate = if n == 1 {
                            base.to_string()
                        } else {
                            format!("{base}-{n}")
                        };
                        if !this.agents.list.iter().any(|a| a.id == candidate) {
                            break candidate;
                        }
                        n += 1;
                    };
                    let display = if n == 1 {
                        "Custom".to_string()
                    } else {
                        format!("Custom {n}")
                    };
                    this.agents.list.push(AgentConfig {
                        id,
                        kind: AgentKind::Generic,
                        display_name: display,
                        path: None,
                        extra_args: Vec::new(),
                        enabled: true,
                    });
                    this.agents.sync_inputs(cx);
                    this.agents.push(&this.app, cx);
                    cx.notify();
                }),
            );

        let toolbar = div()
            .flex()
            .flex_row()
            .gap(px(8.0))
            .child(redetect)
            .child(add_custom);

        div()
            .id("agents-pane-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .overflow_y_scroll()
            .p(px(20.0))
            .gap(px(12.0))
            .child(section_title("Coding Agents"))
            .child(section_note(
                "Configure which coding agents Allele can launch in a \
                     session. The default is used for every new session; \
                     a project can override it by adding an \"agent\" key \
                     to its allele.json. Extra args are appended to the \
                     built-in args the adapter generates (useful for \
                     flags like --dangerously-skip-permissions).",
            ))
            .child(toolbar)
            .child(rows)
    }
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
        .rounded(px(6.0))
        .bg(theme().bg_raised)
        .text_size(px(10.0))
        .text_color(theme().accent)
        .child(format!("{:?}", agent.kind));

    let enabled = agent.enabled;
    let toggle = div()
        .id(SharedString::from(format!("agent-toggle-{idx}")))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                if let Some(a) = this.agents.list.get_mut(idx) {
                    a.enabled = !a.enabled;
                }
                this.agents.ensure_default_valid();
                this.agents.push(&this.app, cx);
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(30.0))
                .h(px(16.0))
                .rounded(px(8.0))
                .bg(if enabled {
                    theme().accent
                } else {
                    theme().bg_hover
                })
                .flex()
                .items_center()
                .px(px(2.0))
                .child(
                    div()
                        .w(px(12.0))
                        .h(px(12.0))
                        .rounded(px(6.0))
                        .bg(theme().bg_base)
                        .ml(if enabled { px(14.0) } else { px(0.0) }),
                ),
        );

    let default_btn = div()
        .id(SharedString::from(format!("agent-default-{idx}")))
        .cursor_pointer()
        .px(px(8.0))
        .py(px(2.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .bg(if is_default {
            theme().accent
        } else {
            theme().bg_raised
        })
        .text_color(if is_default {
            theme().bg_base
        } else {
            theme().text_secondary
        })
        .hover(|s| s.bg(theme().bg_active))
        .child(if is_default { "Default" } else { "Set default" })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                if let Some(a) = this.agents.list.get(idx) {
                    let id = a.id.clone();
                    this.agents.default_agent = Some(id);
                    this.agents.push(&this.app, cx);
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
            .rounded(px(6.0))
            .hover(|s| s.bg(theme().bg_raised))
            .child(icon(icons::X, 12.0, theme().text_faint))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    if idx < this.agents.list.len() {
                        this.agents.list.remove(idx);
                    }
                    this.agents.ensure_default_valid();
                    this.agents.sync_inputs(cx);
                    this.agents.push(&this.app, cx);
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
                    .text_color(theme().text_secondary)
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
        .bg(theme().bg_surface)
        .border_1()
        .border_color(theme().border_subtle)
        .child(header)
        .child(labelled("Path", path_input))
        .child(labelled("Args", args_input))
        .into_any_element()
}

/// Minimal shell-ish splitter. Splits on whitespace; preserves quoted spans so
/// `--flag="one two"` stays intact.
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
