//! Sidebar rendering — project tree, session list, archives.

use gpui::*;

use crate::actions::{ArchiveAction, ProjectAction, SessionAction, SessionCursor};
use crate::app_state::AppState;
use crate::session::SessionStatus;

impl AppState {
    /// Build the sidebar item list: project headers, session rows, archives.
    pub(crate) fn build_sidebar_items(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut sidebar_items: Vec<AnyElement> = Vec::new();
        let active_cursor = self.active;

        for (p_idx, project) in self.projects.iter().enumerate() {
            let project_name = project.name.clone();
            // Project header
            sidebar_items.push(
                div()
                    .id(SharedString::from(format!("project-{p_idx}")))
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(rgb(0x11111b))
                    .border_b_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(0x6c7086))
                                    .child("▾"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(0xcdd6f4))
                                    .child(project_name),
                            ),
                    )
                    .child(
                        // New session button
                        div()
                            .id(SharedString::from(format!("new-session-{p_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .text_size(px(14.0))
                            .text_color(rgb(0x6c7086))
                            .hover(|s| s.text_color(rgb(0xa6e3a1)))
                            .child("+")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action = Some(SessionAction::AddToProject(p_idx).into());
                                cx.notify();
                            })),
                    )
                    .child(
                        // Project settings button
                        div()
                            .id(SharedString::from(format!("settings-project-{p_idx}")))
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_size(px(11.0))
                            .text_color(if self.editing_project_settings == Some(p_idx) {
                                rgb(0x89b4fa) // blue when active
                            } else {
                                rgb(0x45475a)
                            })
                            .hover(|s| s.text_color(rgb(0x89b4fa)))
                            .child("⚙")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                cx.stop_propagation();
                                if this.editing_project_settings == Some(p_idx) {
                                    this.editing_project_settings = None;
                                } else {
                                    this.editing_project_settings = Some(p_idx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        // Remove project button
                        div()
                            .id(SharedString::from(format!("remove-project-{p_idx}")))
                            .cursor_pointer()
                            .px(px(4.0))
                            .text_size(px(11.0))
                            .text_color(rgb(0x45475a))
                            .hover(|s| s.text_color(rgb(0xf38ba8)))
                            .child("✕")
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action = Some(ProjectAction::Remove(p_idx).into());
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );

            // Dirty-state confirmation prompt
            if self.confirmations.dirty_session == Some(p_idx) {
                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("dirty-confirm-{p_idx}")))
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(5.0))
                        .bg(rgb(0x3b2f1e)) // subtle amber tint
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(11.0))
                                .text_color(rgb(0xf9e2af)) // yellow
                                .child("Uncommitted changes — proceed?"),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("dirty-proceed-{p_idx}")))
                                .cursor_pointer()
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(3.0))
                                .bg(rgb(0xa6e3a1))
                                .text_size(px(10.0))
                                .text_color(rgb(0x1e1e2e))
                                .hover(|s| s.bg(rgb(0x94e2d5)))
                                .child("Proceed")
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(SessionAction::ProceedDirty(p_idx).into());
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("dirty-cancel-{p_idx}")))
                                .cursor_pointer()
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(3.0))
                                .bg(rgb(0x45475a))
                                .text_size(px(10.0))
                                .text_color(rgb(0xcdd6f4))
                                .hover(|s| s.bg(rgb(0x585b70)))
                                .child("Cancel")
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(SessionAction::CancelDirty.into());
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }

            // Inline project settings panel
            if self.editing_project_settings == Some(p_idx) {
                let current_strategy = match project.settings.merge_strategy {
                    crate::settings::MergeStrategy::Merge => "Merge (--no-ff)",
                    crate::settings::MergeStrategy::Squash => "Squash",
                    crate::settings::MergeStrategy::RebaseThenMerge => "Rebase + FF",
                };
                let current_branch = project.settings.default_branch
                    .as_deref()
                    .unwrap_or("auto-detect");
                let current_remote = project.settings.remote
                    .as_deref()
                    .unwrap_or("origin");
                let rebase_label = if project.settings.rebase_before_merge {
                    "Yes"
                } else {
                    "No"
                };

                // Helper: a settings row with label + clickable value
                let settings_row = |_id: &str, label: &str, value: &str| -> AnyElement {
                    div()
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(3.0))
                        .bg(rgb(0x1e1e2e))
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(0x6c7086))
                                .child(SharedString::from(label.to_string())),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(0x89b4fa))
                                .child(SharedString::from(value.to_string())),
                        )
                        .into_any_element()
                };

                // Settings header
                sidebar_items.push(
                    div()
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(4.0))
                        .bg(rgb(0x1e1e2e))
                        .border_b_1()
                        .border_color(rgb(0x313244))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(0x89b4fa))
                                .child("PROJECT SETTINGS"),
                        )
                        .into_any_element(),
                );

                // Merge strategy — clickable to cycle
                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("setting-strategy-{p_idx}")))
                        .cursor_pointer()
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(3.0))
                        .bg(rgb(0x1e1e2e))
                        .hover(|s| s.bg(rgb(0x313244)))
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(0x6c7086))
                                .child("Merge strategy"),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(0x89b4fa))
                                .child(SharedString::from(current_strategy.to_string())),
                        )
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                            cx.stop_propagation();
                            if let Some(project) = this.projects.get_mut(p_idx) {
                                project.settings.merge_strategy = match project.settings.merge_strategy {
                                    crate::settings::MergeStrategy::Merge => crate::settings::MergeStrategy::Squash,
                                    crate::settings::MergeStrategy::Squash => crate::settings::MergeStrategy::RebaseThenMerge,
                                    crate::settings::MergeStrategy::RebaseThenMerge => crate::settings::MergeStrategy::Merge,
                                };
                            }
                            this.mark_settings_dirty();
                            cx.notify();
                        }))
                        .into_any_element(),
                );

                // Rebase before merge — clickable to toggle
                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("setting-rebase-{p_idx}")))
                        .cursor_pointer()
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(3.0))
                        .bg(rgb(0x1e1e2e))
                        .hover(|s| s.bg(rgb(0x313244)))
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(0x6c7086))
                                .child("Sync remote first"),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(if project.settings.rebase_before_merge {
                                    rgb(0xa6e3a1) // green = on
                                } else {
                                    rgb(0xf38ba8) // red = off
                                })
                                .child(SharedString::from(rebase_label.to_string())),
                        )
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                            cx.stop_propagation();
                            if let Some(project) = this.projects.get_mut(p_idx) {
                                project.settings.rebase_before_merge = !project.settings.rebase_before_merge;
                            }
                            this.mark_settings_dirty();
                            cx.notify();
                        }))
                        .into_any_element(),
                );

                // Default branch — read-only display (editing needs text input)
                sidebar_items.push(settings_row(
                    &format!("setting-branch-{p_idx}"),
                    "Default branch",
                    current_branch,
                ));

                // Remote — read-only display
                sidebar_items.push(settings_row(
                    &format!("setting-remote-{p_idx}"),
                    "Remote",
                    current_remote,
                ));

                // Bottom border
                sidebar_items.push(
                    div()
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(2.0))
                        .bg(rgb(0x1e1e2e))
                        .border_b_1()
                        .border_color(rgb(0x313244))
                        .child(
                            div()
                                .text_size(px(9.0))
                                .text_color(rgb(0x45475a))
                                .child("Edit settings.json for branch/remote"),
                        )
                        .into_any_element(),
                );
            }

            // Loading placeholders (sessions mid-clone)
            for loading in &project.loading_sessions {
                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("loading-{}", loading.id)))
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(5.0))
                        .bg(rgb(0x181825))
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_row()
                                .gap(px(6.0))
                                .items_center()
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(0xf9e2af)) // yellow
                                        .child("◐"),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x9399b2))
                                        .child(loading.label.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .text_color(rgb(0x585b70))
                                        .child("Cloning…"),
                                ),
                        )
                        .into_any_element(),
                );
            }

            // Sessions under this project
            for (s_idx, session) in project.sessions.iter().enumerate() {
                let is_active = active_cursor
                    .map(|c| c.project_idx == p_idx && c.session_idx == s_idx)
                    .unwrap_or(false);
                let is_suspended = session.status == SessionStatus::Suspended;
                let status_color = session.status.color();
                let status_icon = session.status.icon();
                // Prefer the auto-named label once it's no longer a
                // placeholder ("Claude N" / "Shell N").  Fall back to the
                // terminal's OSC title only while waiting for auto-naming,
                // and to the raw label as a last resort.
                let is_placeholder = session.label.starts_with("Claude ")
                    || session.label.starts_with("Shell ");
                let label = if !is_placeholder {
                    session.label.clone()
                } else {
                    session
                        .terminal_view
                        .as_ref()
                        .and_then(|tv| tv.read(cx).title())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| session.label.clone())
                };
                let elapsed = session.elapsed_display();
                let is_confirming = self.confirmations.discard
                    == Some(SessionCursor { project_idx: p_idx, session_idx: s_idx });

                let label_color = if is_suspended {
                    rgb(0x6c7086) // greyed out for Suspended
                } else if is_active {
                    rgb(0xcdd6f4)
                } else {
                    rgb(0x9399b2)
                };

                let row_bg = if is_confirming {
                    rgb(0x3b1f28) // subtle red tint while confirming discard
                } else if is_active {
                    rgb(0x313244)
                } else {
                    rgb(0x181825)
                };

                let mut row = div()
                    .id(SharedString::from(format!("session-{p_idx}-{s_idx}")))
                    .pl(px(24.0))
                    .pr(px(12.0))
                    .py(px(5.0))
                    .bg(row_bg)
                    .hover(|s| s.bg(rgb(0x313244)))
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .items_center()
                    .justify_between()
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        this.pending_action = Some(SessionAction::Select {
                            project_idx: p_idx,
                            session_idx: s_idx,
                        }.into());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(status_color))
                                    .child(status_icon.to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(label_color)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(0x585b70))
                                    .min_w(px(60.0))
                                    .child(elapsed),
                            ),
                    );

                if is_confirming {
                    // Replace the normal buttons with a two-button confirm
                    // prompt: Discard (destructive) + Cancel.
                    row = row.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.0))
                            .items_center()
                            .child(
                                div()
                                    .id(SharedString::from(format!("confirm-discard-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(3.0))
                                    .bg(rgb(0x45475a))
                                    .text_size(px(10.0))
                                    .text_color(rgb(0xf38ba8))
                                    .hover(|s| s.bg(rgb(0x58303a)))
                                    .child("Discard")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(SessionAction::Discard {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        }.into());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("cancel-discard-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(3.0))
                                    .text_size(px(10.0))
                                    .text_color(rgb(0x9399b2))
                                    .hover(|s| s.text_color(rgb(0xcdd6f4)))
                                    .child("Cancel")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(SessionAction::CancelDiscard.into());
                                        cx.notify();
                                    })),
                            ),
                    );
                } else {
                    // Normal state: Merge & Close, Close (keep clone), and Discard.
                    row = row.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(2.0))
                            .items_center()
                            .child(
                                div()
                                    .id(SharedString::from(format!("merge-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x45475a))
                                    .hover(|s| s.text_color(rgb(0xa6e3a1)))
                                    .child("✓")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(SessionAction::MergeAndClose {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        }.into());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("close-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x45475a))
                                    .hover(|s| s.text_color(rgb(0x89b4fa)))
                                    .child("✕")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(SessionAction::CloseKeepClone {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        }.into());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("discard-{p_idx}-{s_idx}")))
                                    .cursor_pointer()
                                    .px(px(4.0))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x45475a))
                                    .hover(|s| s.text_color(rgb(0xf38ba8)))
                                    .child("🗑")
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(SessionAction::RequestDiscard {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        }.into());
                                        cx.notify();
                                    })),
                            ),
                    );
                }

                sidebar_items.push(row.into_any_element());
            }

            // Archived sessions for this project
            if !project.archives.is_empty() {
                // Section header
                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("archives-header-{p_idx}")))
                        .px(px(16.0))
                        .py(px(4.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .text_size(px(9.0))
                                .text_color(rgb(0x585b70))
                                .child(format!("ARCHIVES ({})", project.archives.len())),
                        )
                        .into_any_element(),
                );

                for (a_idx, archive) in project.archives.iter().enumerate() {
                    let display_label = archive.label.clone();
                    let age = {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let delta = now.saturating_sub(archive.archived_at);
                        if delta < 60 { "just now".to_string() }
                        else if delta < 3600 { format!("{}m ago", delta / 60) }
                        else if delta < 86400 { format!("{}h ago", delta / 3600) }
                        else { format!("{}d ago", delta / 86400) }
                    };

                    sidebar_items.push(
                        div()
                            .id(SharedString::from(format!("archive-{p_idx}-{a_idx}")))
                            .pl(px(24.0))
                            .pr(px(12.0))
                            .py(px(3.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_row()
                                    .gap(px(6.0))
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(rgb(0x585b70))
                                            .child("📦"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(rgb(0x6c7086))
                                            .child(display_label),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(9.0))
                                            .text_color(rgb(0x45475a))
                                            .child(age),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(4.0))
                                    .child(
                                        // Merge button
                                        div()
                                            .id(SharedString::from(format!("merge-{p_idx}-{a_idx}")))
                                            .cursor_pointer()
                                            .px(px(4.0))
                                            .py(px(1.0))
                                            .rounded(px(3.0))
                                            .text_size(px(9.0))
                                            .text_color(rgb(0xa6e3a1))
                                            .hover(|s| s.bg(rgb(0x313244)))
                                            .child("merge")
                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.pending_action = Some(ArchiveAction::Merge {
                                                    project_idx: p_idx,
                                                    archive_idx: a_idx,
                                                }.into());
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        // Delete button
                                        div()
                                            .id(SharedString::from(format!("delarchive-{p_idx}-{a_idx}")))
                                            .cursor_pointer()
                                            .px(px(4.0))
                                            .text_size(px(10.0))
                                            .text_color(rgb(0x45475a))
                                            .hover(|s| s.text_color(rgb(0xf38ba8)))
                                            .child("×")
                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.pending_action = Some(ArchiveAction::Delete {
                                                    project_idx: p_idx,
                                                    archive_idx: a_idx,
                                                }.into());
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .into_any_element(),
                    );
                }
            }
        }

        sidebar_items
    }
}
