//! Sidebar rendering — project tree, session rows, archives, per-project settings.
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 4.
//! Takes `&mut AppState` + GPUI context and returns the flat list of elements
//! that the top-level render lays out into the left sidebar flex column.

use gpui::*;

use crate::actions::{PendingAction, SessionCursor};
use crate::app_state::AppState;
use crate::session::SessionStatus;
use crate::SimpleTooltip;

pub(crate) fn build_sidebar_items(
    state: &mut AppState,
    _window: &mut Window,
    cx: &mut Context<AppState>,
) -> Vec<AnyElement> {
    // Build sidebar items: for each project, a header then its sessions
    let mut sidebar_items: Vec<AnyElement> = Vec::new();
    let active_cursor = state.active;
    let filter = &state.sidebar_filter;
    let filtering = !filter.is_empty();

    for (p_idx, project) in state.projects.iter().enumerate() {
        let project_name = project.name.clone();

        // When filtering, skip projects that have no matching sessions.
        if filtering {
            let project_matches = project_name.to_lowercase().contains(filter);
            let any_session_matches = project.sessions.iter().any(|s| {
                s.label.to_lowercase().contains(filter)
                    || s.comment.as_deref().unwrap_or("").to_lowercase().contains(filter)
            });
            if !project_matches && !any_session_matches {
                continue;
            }
        }

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
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip { text: "New session".into() }).into()
                        })
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                            cx.stop_propagation();
                            this.pending_action = Some(PendingAction::AddSessionToProject(p_idx));
                            cx.notify();
                        })),
                )
                .child(
                    // New session with details button
                    div()
                        .id(SharedString::from(format!("new-session-details-{p_idx}")))
                        .cursor_pointer()
                        .px(px(4.0))
                        .text_size(px(11.0))
                        .text_color(rgb(0x6c7086))
                        .hover(|s| s.text_color(rgb(0xa6e3a1)))
                        .child("▸")
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip { text: "New session with details".into() }).into()
                        })
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                            cx.stop_propagation();
                            this.pending_action = Some(PendingAction::OpenNewSessionModal(p_idx));
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
                        .text_color(if state.editing_project_settings == Some(p_idx) {
                            rgb(0x89b4fa) // blue when active
                        } else {
                            rgb(0x45475a)
                        })
                        .hover(|s| s.text_color(rgb(0x89b4fa)))
                        .child("⚙")
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip { text: "Project settings".into() }).into()
                        })
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
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
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip { text: "Remove project".into() }).into()
                        })
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                            cx.stop_propagation();
                            this.pending_action = Some(PendingAction::RemoveProject(p_idx));
                            cx.notify();
                        })),
                )
                .into_any_element(),
        );

        // Dirty-state confirmation prompt
        if state.confirming_dirty_session == Some(p_idx) {
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
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action = Some(PendingAction::ProceedDirtySession(p_idx));
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
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action = Some(PendingAction::CancelDirtySession);
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }

        // Inline project settings panel
        if state.editing_project_settings == Some(p_idx) {
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
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                        cx.stop_propagation();
                        if let Some(project) = this.projects.get_mut(p_idx) {
                            project.settings.merge_strategy = match project.settings.merge_strategy {
                                crate::settings::MergeStrategy::Merge => crate::settings::MergeStrategy::Squash,
                                crate::settings::MergeStrategy::Squash => crate::settings::MergeStrategy::RebaseThenMerge,
                                crate::settings::MergeStrategy::RebaseThenMerge => crate::settings::MergeStrategy::Merge,
                            };
                        }
                        this.save_settings();
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
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                        cx.stop_propagation();
                        if let Some(project) = this.projects.get_mut(p_idx) {
                            project.settings.rebase_before_merge = !project.settings.rebase_before_merge;
                        }
                        this.save_settings();
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
            if filtering { continue; }
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

        // Sessions under this project — attention-needed first (if
        // enabled), then pinned, then the rest by original index.
        let mut session_order: Vec<usize> = (0..project.sessions.len()).collect();
        let promote = state.user_settings.promote_attention_sessions;
        session_order.sort_by_key(|&idx| {
            let s = &project.sessions[idx];
            let attention: u8 = if promote {
                match s.status {
                    SessionStatus::AwaitingInput => 0,
                    SessionStatus::ResponseReady => 1,
                    _ => 2,
                }
            } else {
                2
            };
            (attention, !s.pinned, idx)
        });

        for s_idx in session_order {
            let session = &project.sessions[s_idx];

            // Skip sessions that don't match the filter.
            if filtering {
                let label_matches = session.label.to_lowercase().contains(filter);
                let comment_matches = session.comment.as_deref().unwrap_or("").to_lowercase().contains(filter);
                let project_matches = project.name.to_lowercase().contains(filter);
                if !label_matches && !comment_matches && !project_matches {
                    continue;
                }
            }

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
            let is_confirming = state.confirming_discard
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
                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                    this.session_context_menu = None;
                    this.pending_action = Some(PendingAction::SelectSession {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    });
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(move |this: &mut AppState, event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                    this.session_context_menu = Some((
                        SessionCursor { project_idx: p_idx, session_idx: s_idx },
                        event.position,
                    ));
                    cx.notify();
                }))
                .child({
                    let session_pinned = session.pinned;
                    let session_comment = session.comment.clone();
                    let mut label_row = div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .items_center()
                        .overflow_hidden()
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(status_color))
                                .child(status_icon.to_string())
                        );
                    if session_pinned {
                        label_row = label_row.child(
                            div()
                                .text_size(px(9.0))
                                .text_color(rgb(0xf9e2af))
                                .child("📌"),
                        );
                    }
                    label_row = label_row
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_size(px(12.0))
                                .text_color(label_color)
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.0))
                                .text_color(rgb(0x585b70))
                                .min_w(px(60.0))
                                .child(elapsed),
                        );

                    let mut info_col = div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(label_row);
                    if let Some(comment) = session_comment {
                        info_col = info_col.child(
                            div()
                                .pl(px(16.0))
                                .text_size(px(10.0))
                                .text_color(rgb(0x585b70))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(comment),
                        );
                    }
                    info_col
                });

            if is_confirming {
                // Replace the normal buttons with a two-button confirm
                // prompt: Discard (destructive) + Cancel.
                row = row.child(
                    div()
                        .flex_shrink_0()
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
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::DiscardSession {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    });
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
                                .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::CancelDiscard);
                                    cx.notify();
                                })),
                        ),
                );
            } else {
                // Normal state: Merge & Close, Close (keep clone), and Discard.
                row = row.child(
                    div()
                        .flex_shrink_0()
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
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip { text: "Merge and close session".into() }).into()
                                })
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::MergeAndClose {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    });
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
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip { text: "Suspend session".into() }).into()
                                })
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::CloseSessionKeepClone {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    });
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
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip { text: "Discard session".into() }).into()
                                })
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::RequestDiscardSession {
                                        project_idx: p_idx,
                                        session_idx: s_idx,
                                    });
                                    cx.notify();
                                })),
                        ),
                );
            }

            sidebar_items.push(row.into_any_element());
        }

        // Archived sessions for this project (hidden when filtering)
        if !project.archives.is_empty() && !filtering {
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
                                        .tooltip(|_window, cx| {
                                            cx.new(|_| SimpleTooltip { text: "Merge archive into project".into() }).into()
                                        })
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                            cx.stop_propagation();
                                            this.pending_action = Some(PendingAction::MergeArchive {
                                                project_idx: p_idx,
                                                archive_idx: a_idx,
                                            });
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
                                        .tooltip(|_window, cx| {
                                            cx.new(|_| SimpleTooltip { text: "Delete archive".into() }).into()
                                        })
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                            cx.stop_propagation();
                                            this.pending_action = Some(PendingAction::DeleteArchive {
                                                project_idx: p_idx,
                                                archive_idx: a_idx,
                                            });
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
