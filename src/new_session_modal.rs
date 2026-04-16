//! "New Session with Details" modal — lets the user customise session name,
//! branch slug, agent, and initial prompt before creating a session.

use gpui::*;
use crate::text_input::{TextInput, TextInputEvent};

/// Events emitted by the modal that AppState listens for.
#[derive(Debug, Clone)]
pub enum NewSessionModalEvent {
    Create {
        project_idx: usize,
        label: String,
        branch_slug: Option<String>,
        agent_id: Option<String>,
        initial_prompt: Option<String>,
    },
    Close,
}

impl EventEmitter<NewSessionModalEvent> for NewSessionModal {}

pub struct NewSessionModal {
    project_idx: usize,
    name_input: Entity<TextInput>,
    branch_input: Entity<TextInput>,
    prompt_input: Entity<TextInput>,
    /// (id, display_name) for each enabled agent.
    agents: Vec<(String, String)>,
    selected_agent_idx: usize,
    default_label: String,
    focus_handle: FocusHandle,
}

impl NewSessionModal {
    pub fn new(
        cx: &mut Context<Self>,
        project_idx: usize,
        agents: Vec<(String, String)>,
        default_agent_idx: usize,
        default_label: String,
    ) -> Self {
        let name_input = cx.new(|cx| {
            TextInput::new(cx, "", format!("{default_label}"))
        });
        let branch_input = cx.new(|cx| {
            TextInput::new(cx, "", "auto-generated from session name")
        });
        let prompt_input = cx.new(|cx| {
            TextInput::new(cx, "", "optional")
        });

        // When name input is submitted (Enter), treat as form submit.
        cx.subscribe(&name_input, |this: &mut Self, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                this.submit(cx);
            }
        }).detach();
        cx.subscribe(&branch_input, |this: &mut Self, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                this.submit(cx);
            }
        }).detach();
        cx.subscribe(&prompt_input, |this: &mut Self, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                this.submit(cx);
            }
        }).detach();

        Self {
            project_idx,
            name_input,
            branch_input,
            prompt_input,
            agents,
            selected_agent_idx: default_agent_idx,
            default_label,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let label = {
            let text = self.name_input.read(cx).text().to_string();
            if text.trim().is_empty() {
                self.default_label.clone()
            } else {
                text.trim().to_string()
            }
        };

        let branch_slug = {
            let text = self.branch_input.read(cx).text().to_string();
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        };

        let agent_id = self.agents.get(self.selected_agent_idx).map(|(id, _)| id.clone());

        let initial_prompt = {
            let text = self.prompt_input.read(cx).text().to_string();
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        };

        cx.emit(NewSessionModalEvent::Create {
            project_idx: self.project_idx,
            label,
            branch_slug,
            agent_id,
            initial_prompt,
        });
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(NewSessionModalEvent::Close);
    }

    fn cycle_agent(&mut self, cx: &mut Context<Self>) {
        if self.agents.is_empty() { return; }
        self.selected_agent_idx = (self.selected_agent_idx + 1) % self.agents.len();
        cx.notify();
    }

    fn render_form_row(
        label_text: &str,
        content: impl IntoElement,
    ) -> Div {
        div()
            .w_full()
            .px(px(20.0))
            .py(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(110.0))
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(rgb(0x6c7086))
                    .child(label_text.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(content),
            )
    }

    fn input_frame(child: Entity<TextInput>) -> Div {
        div()
            .w_full()
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
}

impl Render for NewSessionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div()
            .id("new-session-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(rgba(0x00000099))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _ev, _w, cx| {
                    this.close(cx);
                }),
            );

        // Agent selector row
        let agent_display = if let Some((_, name)) = self.agents.get(self.selected_agent_idx) {
            name.clone()
        } else {
            "Shell (no agent)".to_string()
        };

        let card = div()
            .id("new-session-card")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;
                if key == "escape" {
                    this.close(cx);
                } else if key == "enter" && mods.platform {
                    this.submit(cx);
                }
            }))
            .w(px(480.0))
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded(px(8.0))
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| {
                cx.stop_propagation();
            })
            // Header
            .child(
                div()
                    .w_full()
                    .px(px(20.0))
                    .py(px(12.0))
                    .border_b_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xcdd6f4))
                            .child("New Session"),
                    )
                    .child(
                        div()
                            .id("new-session-close-btn")
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .text_size(px(14.0))
                            .text_color(rgb(0x6c7086))
                            .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    this.close(cx);
                                }),
                            ),
                    ),
            )
            // Form fields
            .child(
                div()
                    .w_full()
                    .py(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    // Session name
                    .child(Self::render_form_row(
                        "Session name",
                        Self::input_frame(self.name_input.clone()),
                    ))
                    // Branch name
                    .child(Self::render_form_row(
                        "Branch name",
                        Self::input_frame(self.branch_input.clone()),
                    ))
                    // Agent selector
                    .child(Self::render_form_row(
                        "Agent",
                        div()
                            .id("new-session-agent-cycle")
                            .cursor_pointer()
                            .w_full()
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(rgb(0x45475a))
                            .bg(rgb(0x11111b))
                            .text_size(px(12.0))
                            .text_color(rgb(0x89b4fa))
                            .hover(|s| s.bg(rgb(0x181825)))
                            .child(agent_display)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    cx.stop_propagation();
                                    this.cycle_agent(cx);
                                }),
                            ),
                    ))
                    // Initial prompt
                    .child(Self::render_form_row(
                        "Initial prompt",
                        Self::input_frame(self.prompt_input.clone()),
                    )),
            )
            // Footer
            .child(
                div()
                    .w_full()
                    .px(px(20.0))
                    .py(px(12.0))
                    .border_t_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("new-session-cancel-btn")
                            .cursor_pointer()
                            .px(px(12.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x45475a))
                            .text_size(px(11.0))
                            .text_color(rgb(0xcdd6f4))
                            .hover(|s| s.bg(rgb(0x585b70)))
                            .child("Cancel")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    this.close(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("new-session-create-btn")
                            .cursor_pointer()
                            .px(px(12.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .bg(rgb(0xa6e3a1))
                            .text_size(px(11.0))
                            .text_color(rgb(0x1e1e2e))
                            .hover(|s| s.bg(rgb(0x94e2d5)))
                            .child("Create")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    this.submit(cx);
                                }),
                            ),
                    ),
            );

        backdrop.child(card)
    }
}

// ---------------------------------------------------------------------------
// Edit Session Modal
// ---------------------------------------------------------------------------

/// Events emitted by the edit session modal.
#[derive(Debug, Clone)]
pub enum EditSessionModalEvent {
    Apply {
        project_idx: usize,
        session_idx: usize,
        label: String,
        branch_slug: Option<String>,
        comment: Option<String>,
        pinned: bool,
    },
    Close,
}

impl EventEmitter<EditSessionModalEvent> for EditSessionModal {}

pub struct EditSessionModal {
    project_idx: usize,
    session_idx: usize,
    name_input: Entity<TextInput>,
    branch_input: Entity<TextInput>,
    comment_input: Entity<TextInput>,
    pinned: bool,
    focus_handle: FocusHandle,
}

impl EditSessionModal {
    pub fn new(
        cx: &mut Context<Self>,
        project_idx: usize,
        session_idx: usize,
        current_label: &str,
        current_branch_slug: &str,
        current_comment: &str,
        pinned: bool,
    ) -> Self {
        let label_owned: SharedString = current_label.to_string().into();
        let branch_owned: SharedString = current_branch_slug.to_string().into();
        let comment_owned: SharedString = current_comment.to_string().into();
        let name_input = cx.new(|cx| {
            TextInput::new(cx, label_owned, "Session name")
        });
        let branch_input = cx.new(|cx| {
            TextInput::new(cx, branch_owned, "Branch name")
        });
        let comment_input = cx.new(|cx| {
            TextInput::new(cx, comment_owned, "Add a comment…")
        });

        cx.subscribe(&name_input, |this: &mut Self, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                this.submit(cx);
            }
        }).detach();
        cx.subscribe(&branch_input, |this: &mut Self, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                this.submit(cx);
            }
        }).detach();
        cx.subscribe(&comment_input, |this: &mut Self, _input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submitted) {
                this.submit(cx);
            }
        }).detach();

        Self {
            project_idx,
            session_idx,
            name_input,
            branch_input,
            comment_input,
            pinned,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let label = self.name_input.read(cx).text().trim().to_string();
        if label.is_empty() { return; }

        let branch_slug = {
            let text = self.branch_input.read(cx).text().trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        };

        let comment = {
            let text = self.comment_input.read(cx).text().trim().to_string();
            if text.is_empty() { None } else { Some(text) }
        };

        cx.emit(EditSessionModalEvent::Apply {
            project_idx: self.project_idx,
            session_idx: self.session_idx,
            label,
            branch_slug,
            comment,
            pinned: self.pinned,
        });
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(EditSessionModalEvent::Close);
    }

    fn toggle_pinned(&mut self, cx: &mut Context<Self>) {
        self.pinned = !self.pinned;
        cx.notify();
    }
}

impl Render for EditSessionModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let backdrop = div()
            .id("edit-session-backdrop")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(rgba(0x00000099))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _ev, _w, cx| {
                    this.close(cx);
                }),
            );

        let pin_label = if self.pinned { "Pinned ✓" } else { "Not pinned" };
        let pin_color = if self.pinned { rgb(0xf9e2af) } else { rgb(0x6c7086) };

        let card = div()
            .id("edit-session-card")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                let mods = &event.keystroke.modifiers;
                if key == "escape" {
                    this.close(cx);
                } else if key == "enter" && mods.platform {
                    this.submit(cx);
                }
            }))
            .w(px(480.0))
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded(px(8.0))
            .shadow_lg()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_ev, _w, cx| {
                cx.stop_propagation();
            })
            // Header
            .child(
                div()
                    .w_full()
                    .px(px(20.0))
                    .py(px(12.0))
                    .border_b_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xcdd6f4))
                            .child("Edit Session"),
                    )
                    .child(
                        div()
                            .id("edit-session-close-btn")
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .text_size(px(14.0))
                            .text_color(rgb(0x6c7086))
                            .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    this.close(cx);
                                }),
                            ),
                    ),
            )
            // Form fields
            .child(
                div()
                    .w_full()
                    .py(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(NewSessionModal::render_form_row(
                        "Session name",
                        NewSessionModal::input_frame(self.name_input.clone()),
                    ))
                    .child(NewSessionModal::render_form_row(
                        "Branch name",
                        NewSessionModal::input_frame(self.branch_input.clone()),
                    ))
                    .child(NewSessionModal::render_form_row(
                        "Comment",
                        NewSessionModal::input_frame(self.comment_input.clone()),
                    ))
                    .child(NewSessionModal::render_form_row(
                        "Pinned",
                        div()
                            .id("edit-session-pin-toggle")
                            .cursor_pointer()
                            .w_full()
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .border_1()
                            .border_color(rgb(0x45475a))
                            .bg(rgb(0x11111b))
                            .text_size(px(12.0))
                            .text_color(pin_color)
                            .hover(|s| s.bg(rgb(0x181825)))
                            .child(pin_label.to_string())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    cx.stop_propagation();
                                    this.toggle_pinned(cx);
                                }),
                            ),
                    )),
            )
            // Footer
            .child(
                div()
                    .w_full()
                    .px(px(20.0))
                    .py(px(12.0))
                    .border_t_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("edit-session-cancel-btn")
                            .cursor_pointer()
                            .px(px(12.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x45475a))
                            .text_size(px(11.0))
                            .text_color(rgb(0xcdd6f4))
                            .hover(|s| s.bg(rgb(0x585b70)))
                            .child("Cancel")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    this.close(cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("edit-session-apply-btn")
                            .cursor_pointer()
                            .px(px(12.0))
                            .py(px(5.0))
                            .rounded(px(4.0))
                            .bg(rgb(0x89b4fa))
                            .text_size(px(11.0))
                            .text_color(rgb(0x1e1e2e))
                            .hover(|s| s.bg(rgb(0x74c7ec)))
                            .child("Save")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _ev, _w, cx| {
                                    this.submit(cx);
                                }),
                            ),
                    ),
            );

        backdrop.child(card)
    }
}
