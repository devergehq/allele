//! Sidebar rendering — project tree, session rows, archives, per-project settings.
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 4.
//! Takes `&mut AppState` + GPUI context and returns the flat list of elements
//! that the top-level render lays out into the left sidebar flex column.

use crate::icon::{icon, name as icons};
use crate::theme::{theme, with_alpha};
use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::accessibility::DENSE_CONTROL_MIN_HEIGHT;
use crate::actions::{
    ArchiveAction, DraggedProject, DraggedSession, ProjectAction, SessionAction, SessionCursor,
};
use crate::app_state::AppState;
use crate::project::Project;
use crate::session::SessionStatus;
use crate::SimpleTooltip;

/// Left indents matching the rows each confirmation strip sits beneath, so the
/// prompt lines up with the thing it is about to destroy.
mod px_indent {
    /// Matches the session row's `pl(24)`.
    pub(super) const SESSION: f32 = 24.0;
    /// Matches the archive row's `pl(24)`.
    pub(super) const ARCHIVE: f32 = 24.0;
    /// Matches the ARCHIVES section header's `px(16)`. Used by the bulk-delete
    /// strip, which applies to the whole section rather than to one row.
    pub(super) const ARCHIVES_HEADER: f32 = 16.0;
}

/// What a confirmation-strip button does when clicked. Boxed so both buttons
/// go through one construction path regardless of what they capture.
type ConfirmHandler = Box<dyn Fn(&mut AppState, &mut Context<AppState>)>;

/// A destructive-confirmation strip, rendered as its own row *below* the row
/// it applies to.
///
/// Stacked — message on its own full-width line, buttons right-aligned
/// beneath — rather than sharing a line with the buttons. The sidebar is only
/// a few hundred points wide and user-resizable; once the buttons take their
/// share of a single line there is not enough room left for copy that says
/// what is about to be deleted, so an inline layout pushes the buttons out of
/// sight. Stacking holds at any sidebar width.
///
/// The owning row keeps its `danger` tint while this is visible — that, not
/// the copy, is what identifies which row is about to be destroyed, so the
/// message never has to spell out a path.
fn confirm_strip(
    id_prefix: String,
    // Owned rather than `&'static str` so a caller can name what it is about to
    // destroy — the bulk-archive prompt interpolates a count.
    message: impl Into<SharedString>,
    confirm_label: &'static str,
    indent: f32,
    on_confirm: impl Fn(&mut AppState, &mut Context<AppState>) + 'static,
    on_cancel: impl Fn(&mut AppState, &mut Context<AppState>) + 'static,
    cx: &mut Context<AppState>,
) -> AnyElement {
    let button = |id: String,
                  label: &'static str,
                  danger: bool,
                  handler: ConfirmHandler,
                  cx: &mut Context<AppState>| {
        let base = div()
            .id(SharedString::from(id))
            .flex_shrink_0()
            .cursor_pointer()
            .px(px(8.0))
            .py(px(2.0))
            .min_h(px(DENSE_CONTROL_MIN_HEIGHT))
            .rounded(px(6.0))
            .text_size(px(11.0));
        let styled = if danger {
            base.bg(theme().tint_danger)
                .text_color(theme().danger)
                .hover(|s| s.bg(theme().tint_danger_hover))
        } else {
            base.text_color(theme().text_muted)
                .hover(|s| s.text_color(theme().text_primary))
        };
        styled.child(label).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this: &mut AppState, _event, _window, cx| {
                cx.stop_propagation();
                handler(this, cx);
            }),
        )
    };

    div()
        .w_full()
        .bg(theme().tint_danger)
        .pl(px(indent))
        .pr(px(12.0))
        .pb(px(6.0))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .text_size(px(11.0))
                .text_color(theme().danger)
                .child(message.into()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .items_center()
                .gap(px(4.0))
                .child(button(
                    format!("{id_prefix}-confirm"),
                    confirm_label,
                    true,
                    Box::new(on_confirm),
                    cx,
                ))
                .child(button(
                    format!("{id_prefix}-cancel"),
                    "Cancel",
                    false,
                    Box::new(on_cancel),
                    cx,
                )),
        )
        .into_any_element()
}

// ── Active-only view mode (DEV-295) ───────────────────────────────
//
// The predicates below are the single source of truth for what the
// active-only filter hides. `build_sidebar_items` skips rows with them and
// `active_only_hidden_counts` tallies with them, so the "N hidden" hint can
// never drift out of step with what is actually on screen.

/// Does this session survive the active-only filter?
///
/// Inactive means no live agent — `Suspended` or `Done`. The session under
/// the active cursor is always kept regardless: hiding the row you are
/// currently looking at is disorienting, and a failed resume legitimately
/// leaves the selected session `Suspended` with an error + Retry button that
/// must stay reachable.
pub(crate) fn session_survives_active_only(status: SessionStatus, is_active_cursor: bool) -> bool {
    is_active_cursor || status.is_active()
}

/// Does this project survive the active-only filter?
///
/// Kept when any of its sessions survives, or when a clone is still in
/// flight — a brand-new session's project must not vanish mid-clone, which
/// would strand the session being created.
pub(crate) fn project_survives_active_only(
    project: &Project,
    project_idx: usize,
    active: Option<SessionCursor>,
) -> bool {
    !project.loading_sessions.is_empty()
        || project.sessions.iter().enumerate().any(|(session_idx, s)| {
            let is_active_cursor = active
                == Some(SessionCursor {
                    project_idx,
                    session_idx,
                });
            session_survives_active_only(s.status, is_active_cursor)
        })
}

/// How many sessions and projects the active-only filter would hide, as
/// `(sessions, projects)`. Drives the hint row's "N hidden" copy.
///
/// This reports what the *filter* hides, not what happens to be off screen —
/// a project pinned visible by an open settings panel still counts as hidden.
pub(crate) fn active_only_hidden_counts(
    projects: &[Project],
    active: Option<SessionCursor>,
) -> (usize, usize) {
    let mut sessions = 0;
    let mut hidden_projects = 0;
    for (project_idx, project) in projects.iter().enumerate() {
        for (session_idx, s) in project.sessions.iter().enumerate() {
            let is_active_cursor = active
                == Some(SessionCursor {
                    project_idx,
                    session_idx,
                });
            if !session_survives_active_only(s.status, is_active_cursor) {
                sessions += 1;
            }
        }
        if !project_survives_active_only(project, project_idx, active) {
            hidden_projects += 1;
        }
    }
    (sessions, hidden_projects)
}

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
    let active_only = state.user_settings.sidebar_active_only;

    for (p_idx, project) in state.projects.iter().enumerate() {
        let project_name = project.name.clone();

        // Active-only: drop projects with nothing live in them. Projects the
        // user is mid-interaction with are pinned visible — otherwise an open
        // settings panel or a confirmation prompt would disappear under them.
        if active_only && !project_survives_active_only(project, p_idx, active_cursor) {
            let has_open_ui = state.editing_project_settings == Some(p_idx)
                || state.confirming.remove_project == Some(p_idx)
                || state.confirming.dirty_session == Some(p_idx);
            if !has_open_ui {
                continue;
            }
        }

        // When filtering, skip projects that have no matching sessions.
        if filtering {
            let project_matches = project_name.to_lowercase().contains(filter);
            let any_session_matches = project.sessions.iter().any(|s| {
                s.label.to_lowercase().contains(filter)
                    || s.comment
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(filter)
            });
            if !project_matches && !any_session_matches {
                continue;
            }
        }

        let is_confirming_remove = state.confirming.remove_project == Some(p_idx);

        // Project header
        let header_drag_label = project_name.clone();
        let mut header = div()
            .id(SharedString::from(format!("project-{p_idx}")))
            .group(format!("proj-{p_idx}"))
            .on_drag(DraggedProject(p_idx), move |_drag, _offset, _window, cx| {
                cx.new(|_| crate::DragPreview(header_drag_label.clone()))
            })
            .drag_over::<DraggedProject>(|style, _, _, _| style.bg(theme().bg_hover_soft))
            .on_drop(cx.listener(
                move |this: &mut AppState, dragged: &DraggedProject, _window, cx| {
                    this.pending_action = Some(
                        ProjectAction::ReorderProject {
                            from: dragged.0,
                            to: p_idx,
                        }
                        .into(),
                    );
                    cx.notify();
                },
            ))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(
                    move |this: &mut AppState, event: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.project_context_menu = Some((p_idx, event.position));
                        cx.notify();
                    },
                ),
            )
            .px(px(12.0))
            .py(px(6.0))
            .bg(if is_confirming_remove {
                theme().tint_danger
            } else {
                theme().bg_sunken
            })
            .border_b_1()
            .border_color(theme().border_subtle)
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
                    .child(icon(icons::CHEVRON_DOWN, 12.0, theme().text_faint))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme().text_primary)
                            .child(project_name),
                    ),
            );

        if is_confirming_remove {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .gap(px(4.0))
                    .items_center()
                    .child(
                        div()
                            .id(SharedString::from(format!("confirm-remove-{p_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .bg(theme().bg_hover)
                            .text_size(px(12.0))
                            .text_color(theme().danger)
                            .hover(|s| s.bg(theme().tint_danger_hover))
                            .child("Remove")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action =
                                        Some(ProjectAction::RemoveProject(p_idx).into());
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("cancel-remove-{p_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .text_size(px(12.0))
                            .text_color(theme().text_muted)
                            .hover(|s| s.text_color(theme().text_primary))
                            .child("Cancel")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action =
                                        Some(ProjectAction::CancelRemoveProject.into());
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        } else {
            header = header
                .child(
                    // New session button — revealed on header hover
                    div()
                        .id(SharedString::from(format!("new-session-{p_idx}")))
                        .invisible()
                        .group_hover(format!("proj-{p_idx}"), |s| s.visible())
                        .cursor_pointer()
                        .p(px(4.0))
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme().bg_raised))
                        .child(icon(icons::PLUS, 13.0, theme().text_faint))
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip {
                                text: "New session".into(),
                            })
                            .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action =
                                    Some(SessionAction::AddSessionToProject(p_idx).into());
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    // New session with details button — revealed on header hover
                    div()
                        .id(SharedString::from(format!("new-session-details-{p_idx}")))
                        .invisible()
                        .group_hover(format!("proj-{p_idx}"), |s| s.visible())
                        .cursor_pointer()
                        .p(px(4.0))
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme().bg_raised))
                        .child(icon(icons::CHEVRON_RIGHT, 13.0, theme().text_faint))
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip {
                                text: "New session with details".into(),
                            })
                            .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action =
                                    Some(SessionAction::OpenNewSessionModal(p_idx).into());
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    // Project settings button — revealed on header hover,
                    // pinned visible while its panel is open
                    div()
                        .id(SharedString::from(format!("settings-project-{p_idx}")))
                        .when(state.editing_project_settings != Some(p_idx), |el| {
                            el.invisible()
                                .group_hover(format!("proj-{p_idx}"), |s| s.visible())
                        })
                        .cursor_pointer()
                        .p(px(4.0))
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme().bg_raised))
                        .child(icon(
                            icons::SETTINGS,
                            13.0,
                            if state.editing_project_settings == Some(p_idx) {
                                theme().accent
                            } else {
                                theme().text_ghost
                            },
                        ))
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip {
                                text: "Project settings".into(),
                            })
                            .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                cx.stop_propagation();
                                this.toggle_project_settings_panel(p_idx, cx);
                            }),
                        ),
                )
                .child(
                    // Remove project button — revealed on header hover
                    div()
                        .id(SharedString::from(format!("remove-project-{p_idx}")))
                        .invisible()
                        .group_hover(format!("proj-{p_idx}"), |s| s.visible())
                        .cursor_pointer()
                        .p(px(4.0))
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme().bg_raised))
                        .child(icon(icons::X, 13.0, theme().text_ghost))
                        .tooltip(|_window, cx| {
                            cx.new(|_| SimpleTooltip {
                                text: "Remove project".into(),
                            })
                            .into()
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                cx.stop_propagation();
                                this.pending_action =
                                    Some(ProjectAction::RequestRemoveProject(p_idx).into());
                                cx.notify();
                            }),
                        ),
                );
        }

        sidebar_items.push(header.into_any_element());

        // Dirty-state confirmation prompt
        if state.confirming.dirty_session == Some(p_idx) {
            sidebar_items.push(
                div()
                    .id(SharedString::from(format!("dirty-confirm-{p_idx}")))
                    .pl(px(24.0))
                    .pr(px(12.0))
                    .py(px(5.0))
                    .bg(theme().tint_warning) // subtle amber tint
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.0))
                            .text_color(theme().warning) // yellow
                            .child("Uncommitted changes — proceed?"),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("dirty-proceed-{p_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .bg(theme().success)
                            .text_size(px(12.0))
                            .text_color(theme().text_on_accent)
                            .hover(|s| s.bg(theme().teal))
                            .child("Proceed")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action =
                                        Some(SessionAction::ProceedDirtySession(p_idx).into());
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("dirty-cancel-{p_idx}")))
                            .cursor_pointer()
                            .px(px(6.0))
                            .py(px(2.0))
                            .rounded(px(6.0))
                            .bg(theme().bg_hover)
                            .text_size(px(12.0))
                            .text_color(theme().text_primary)
                            .hover(|s| s.bg(theme().bg_active))
                            .child("Cancel")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action =
                                        Some(SessionAction::CancelDirtySession.into());
                                    cx.notify();
                                }),
                            ),
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
            let rebase_label = if project.settings.rebase_before_merge {
                "Yes"
            } else {
                "No"
            };

            // Helper: a settings row with label + clickable value

            // Settings header
            sidebar_items.push(
                div()
                    .pl(px(24.0))
                    .pr(px(12.0))
                    .py(px(4.0))
                    .bg(theme().bg_base)
                    .border_b_1()
                    .border_color(theme().border_subtle)
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme().accent)
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
                    .bg(theme().bg_base)
                    .hover(|s| s.bg(theme().bg_raised))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_faint)
                            .child("Merge strategy"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().accent)
                            .child(SharedString::from(current_strategy.to_string())),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut AppState, _event, _window, cx| {
                            cx.stop_propagation();
                            if let Some(project) = this.projects.get_mut(p_idx) {
                                project.settings.merge_strategy =
                                    match project.settings.merge_strategy {
                                        crate::settings::MergeStrategy::Merge => {
                                            crate::settings::MergeStrategy::Squash
                                        }
                                        crate::settings::MergeStrategy::Squash => {
                                            crate::settings::MergeStrategy::RebaseThenMerge
                                        }
                                        crate::settings::MergeStrategy::RebaseThenMerge => {
                                            crate::settings::MergeStrategy::Merge
                                        }
                                    };
                            }
                            this.mark_settings_dirty();
                            cx.notify();
                        }),
                    )
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
                    .bg(theme().bg_base)
                    .hover(|s| s.bg(theme().bg_raised))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_faint)
                            .child("Sync remote first"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(if project.settings.rebase_before_merge {
                                theme().success // green = on
                            } else {
                                theme().danger // red = off
                            })
                            .child(SharedString::from(rebase_label.to_string())),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut AppState, _event, _window, cx| {
                            cx.stop_propagation();
                            if let Some(project) = this.projects.get_mut(p_idx) {
                                project.settings.rebase_before_merge =
                                    !project.settings.rebase_before_merge;
                            }
                            this.mark_settings_dirty();
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
            );

            // Default branch + remote — live inputs (empty = auto/origin)
            let settings_input_row =
                |label: &'static str, input: Entity<crate::text_input::TextInput>| -> AnyElement {
                    div()
                        .pl(px(24.0))
                        .pr(px(12.0))
                        .py(px(3.0))
                        .bg(theme().bg_base)
                        .flex()
                        .flex_row()
                        .justify_between()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme().text_faint)
                                .child(label),
                        )
                        .child(
                            div()
                                .w(px(140.0))
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(6.0))
                                .bg(theme().bg_sunken)
                                .text_size(px(12.0))
                                .text_color(theme().text_primary)
                                .overflow_hidden()
                                .child(input),
                        )
                        .into_any_element()
                };
            sidebar_items.push(settings_input_row(
                "Default branch",
                state.project_branch_input.clone(),
            ));
            sidebar_items.push(settings_input_row(
                "Remote",
                state.project_remote_input.clone(),
            ));

            // Bottom border
            sidebar_items.push(
                div()
                    .pl(px(24.0))
                    .pr(px(12.0))
                    .py(px(2.0))
                    .bg(theme().bg_base)
                    .border_b_1()
                    .border_color(theme().border_subtle)
                    .into_any_element(),
            );
        }

        // Loading placeholders (sessions mid-clone)
        for loading in &project.loading_sessions {
            if filtering {
                continue;
            }
            sidebar_items.push(
                div()
                    .id(SharedString::from(format!("loading-{}", loading.id)))
                    .pl(px(24.0))
                    .pr(px(12.0))
                    .py(px(5.0))
                    .bg(theme().bg_surface)
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
                            .child(icon(icons::LOADER, 12.0, theme().warning).with_animation(
                                SharedString::from(format!("cloning-spin-{p_idx}")),
                                Animation::new(std::time::Duration::from_millis(900)).repeat(),
                                |ic, delta| {
                                    ic.with_transformation(Transformation::rotate(percentage(
                                        delta,
                                    )))
                                },
                            ))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme().text_muted)
                                    .child(loading.label.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme().text_dim)
                                    .child(loading.status.clone()),
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

            let is_active = active_cursor
                .map(|c| c.project_idx == p_idx && c.session_idx == s_idx)
                .unwrap_or(false);

            // Active-only: drop sessions with no live agent behind them.
            if active_only && !session_survives_active_only(session.status, is_active) {
                continue;
            }

            // Skip sessions that don't match the filter.
            if filtering {
                let label_matches = session.label.to_lowercase().contains(filter);
                let comment_matches = session
                    .comment
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(filter);
                let project_matches = project.name.to_lowercase().contains(filter);
                if !label_matches && !comment_matches && !project_matches {
                    continue;
                }
            }

            let is_suspended = session.status == SessionStatus::Suspended;
            let status_color = session.status.color();
            let status_icon = session.status.icon_name();
            // Prefer the auto-named label once it's no longer a
            // placeholder ("Claude N" / "Shell N").  Fall back to the
            // terminal's OSC title only while waiting for auto-naming,
            // and to the raw label as a last resort.
            let is_placeholder =
                session.label.starts_with("Claude ") || session.label.starts_with("Shell ");
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
            let session_cursor = SessionCursor {
                project_idx: p_idx,
                session_idx: s_idx,
            };
            let is_confirming_discard = state.confirming.discard == Some(session_cursor);
            let is_confirming_merge = state.confirming.dirty_merge == Some(session_cursor);

            let label_color = if is_suspended {
                theme().text_faint // greyed out for Suspended
            } else if is_active {
                theme().text_primary
            } else {
                theme().text_muted
            };

            let row_bg = if is_confirming_discard {
                theme().tint_danger // subtle red tint while confirming discard
            } else if is_confirming_merge {
                theme().tint_warning // subtle amber tint while confirming merge with uncommitted
            } else if is_active {
                theme().bg_raised
            } else {
                theme().bg_surface
            };

            let row_drag_label = label.clone();
            let mut row = div()
                .id(SharedString::from(format!("session-{p_idx}-{s_idx}")))
                .group(format!("sess-{p_idx}-{s_idx}"))
                .on_drag(
                    DraggedSession {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    },
                    move |_drag, _offset, _window, cx| {
                        cx.new(|_| crate::DragPreview(row_drag_label.clone()))
                    },
                )
                .drag_over::<DraggedSession>(|style, _, _, _| style.bg(theme().bg_hover_soft))
                .on_drop(cx.listener(
                    move |this: &mut AppState, dragged: &DraggedSession, _window, cx| {
                        // Same-project reorder only — sessions are clones of
                        // their project's repo and can't change parents.
                        if dragged.project_idx == p_idx {
                            this.pending_action = Some(
                                SessionAction::ReorderSession {
                                    project_idx: p_idx,
                                    from: dragged.session_idx,
                                    to: s_idx,
                                }
                                .into(),
                            );
                            cx.notify();
                        }
                    },
                ))
                .pl(px(24.0))
                .pr(px(12.0))
                .py(px(5.0))
                .bg(row_bg)
                .hover(|s| s.bg(theme().bg_raised))
                .cursor_pointer()
                .flex()
                .flex_row()
                .gap(px(8.0))
                .items_center()
                .justify_between()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut AppState, _event, _window, cx| {
                        this.session_context_menu = None;
                        this.project_context_menu = None;
                        this.pending_action = Some(
                            SessionAction::SelectSession {
                                project_idx: p_idx,
                                session_idx: s_idx,
                            }
                            .into(),
                        );
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(
                        move |this: &mut AppState, event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                            this.project_context_menu = None;
                            this.session_context_menu = Some((
                                SessionCursor {
                                    project_idx: p_idx,
                                    session_idx: s_idx,
                                },
                                event.position,
                            ));
                            cx.notify();
                        },
                    ),
                )
                .child({
                    let session_pinned = session.pinned;
                    let session_comment = session.comment.clone();
                    let session_startup_status = session.startup_status.clone();
                    let session_operation_error = session.operation_error.clone();
                    let session_operation_result = session.operation_result.clone();
                    let mut label_row = div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .items_center()
                        .overflow_hidden()
                        .child({
                            // Running sessions breathe; everything else is still.
                            let status_dot = icon(status_icon, 11.0, status_color);
                            if session.status == SessionStatus::Running {
                                status_dot
                                    .with_animation(
                                        SharedString::from(format!("status-pulse-{p_idx}-{s_idx}")),
                                        Animation::new(std::time::Duration::from_millis(2200))
                                            .repeat()
                                            .with_easing(pulsating_between(0.35, 1.0)),
                                        |dot, delta| dot.opacity(delta),
                                    )
                                    .into_any_element()
                            } else {
                                status_dot.into_any_element()
                            }
                        });
                    if session_pinned {
                        label_row = label_row.child(icon(icons::PIN, 11.0, theme().warning));
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
                                .text_size(px(12.0))
                                .text_color(theme().text_dim)
                                .min_w(px(60.0))
                                .child(elapsed),
                        );
                    if session.git_dirty == Some(true) {
                        label_row = label_row.child(
                            div()
                                .id(SharedString::from(format!("dirty-{p_idx}-{s_idx}")))
                                .flex_shrink_0()
                                .child(icon(icons::CIRCLE_FILL, 7.0, theme().warning))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip {
                                        text: "Uncommitted changes in workspace".into(),
                                    })
                                    .into()
                                }),
                        );
                    }

                    let mut info_col = div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .child(label_row);
                    if let Some(ref status) = session_startup_status {
                        info_col = info_col.child(
                            div()
                                .pl(px(16.0))
                                .text_size(px(12.0))
                                .text_color(theme().warning)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(status.clone()),
                        );
                    } else if let Some(comment) = session_comment {
                        info_col = info_col.child(
                            div()
                                .pl(px(16.0))
                                .text_size(px(12.0))
                                .text_color(theme().text_dim)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(comment),
                        );
                    }
                    if let Some(error) = session_operation_error {
                        let retry_kind = error.kind;
                        let retry_label = match retry_kind {
                            crate::session::OperationErrorKind::Resume => "Retry",
                            crate::session::OperationErrorKind::MergeAndClose => "Retry merge",
                            crate::session::OperationErrorKind::Startup => "Retry",
                        };
                        info_col = info_col.child(
                            div().pl(px(16.0)).flex().items_center().gap(px(6.0))
                                .child(div().flex_1().text_size(px(11.0)).text_color(theme().danger).child(error.message))
                                .child(
                                    div().id(SharedString::from(format!("retry-operation-{p_idx}-{s_idx}")))
                                        .cursor_pointer().text_size(px(11.0)).text_color(theme().accent).child(retry_label)
                                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                            cx.stop_propagation();
                                            let action = match retry_kind {
                                                crate::session::OperationErrorKind::Resume => {
                                                    SessionAction::ResumeSession { project_idx: p_idx, session_idx: s_idx }
                                                }
                                                crate::session::OperationErrorKind::MergeAndClose => {
                                                    SessionAction::MergeAndClose { project_idx: p_idx, session_idx: s_idx }
                                                }
                                                crate::session::OperationErrorKind::Startup => {
                                                    SessionAction::RetryStartup { project_idx: p_idx, session_idx: s_idx }
                                                }
                                            };
                                            this.pending_action = Some(action.into());
                                            cx.notify();
                                        })),
                                ),
                        );
                    } else if let Some(result) = session_operation_result {
                        info_col = info_col.child(
                            div().pl(px(16.0)).text_size(px(11.0)).text_color(theme().text_dim).child(result),
                        );
                    }
                    info_col
                });

            if is_confirming_merge {
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme().warning)
                                .child("Uncommitted changes"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(4.0))
                                .items_center()
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "confirm-merge-{p_idx}-{s_idx}"
                                        )))
                                        .cursor_pointer()
                                        .px(px(6.0))
                                        .py(px(2.0))
                                        .rounded(px(6.0))
                                        .bg(theme().bg_hover)
                                        .text_size(px(12.0))
                                        .text_color(theme().warning)
                                        .hover(|s| s.bg(theme().tint_warning_hover))
                                        .child("Merge committed")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this: &mut AppState, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.pending_action = Some(
                                                        SessionAction::ProceedDirtyMerge {
                                                            project_idx: p_idx,
                                                            session_idx: s_idx,
                                                        }
                                                        .into(),
                                                    );
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "cancel-merge-{p_idx}-{s_idx}"
                                        )))
                                        .cursor_pointer()
                                        .px(px(6.0))
                                        .py(px(2.0))
                                        .rounded(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(theme().text_muted)
                                        .hover(|s| s.text_color(theme().text_primary))
                                        .child("Cancel")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                |this: &mut AppState, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.pending_action = Some(
                                                        SessionAction::CancelDirtyMerge.into(),
                                                    );
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                ),
                        ),
                );
            } else {
                // Normal state: Merge & Close, Close (keep clone), and Discard.
                // Action cluster revealed on row hover.
                row = row.child(
                    div()
                        .flex_shrink_0()
                        .invisible()
                        .group_hover(format!("sess-{p_idx}-{s_idx}"), |s| s.visible())
                        .flex()
                        .flex_row()
                        .gap(px(2.0))
                        .items_center()
                        .child(
                            div()
                                .id(SharedString::from(format!("sync-up-{p_idx}-{s_idx}")))
                                .cursor_pointer()
                                .p(px(4.0))
                                .rounded(px(6.0))
                                .hover(|s| s.bg(theme().bg_raised))
                                .child(icon(icons::CLOUD_UPLOAD, 13.0, theme().text_ghost))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip {
                                        text: "Sync session up".into(),
                                    })
                                    .into()
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.sync_up_session(
                                            SessionCursor {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            },
                                            cx,
                                        );
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("merge-{p_idx}-{s_idx}")))
                                .cursor_pointer()
                                .p(px(4.0))
                                .rounded(px(6.0))
                                .hover(|s| s.bg(theme().bg_raised))
                                .child(icon(icons::CHECK, 13.0, theme().text_ghost))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip {
                                        text: "Merge and close session".into(),
                                    })
                                    .into()
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(
                                            SessionAction::MergeAndClose {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            }
                                            .into(),
                                        );
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("close-{p_idx}-{s_idx}")))
                                .cursor_pointer()
                                .p(px(4.0))
                                .rounded(px(6.0))
                                .hover(|s| s.bg(theme().bg_raised))
                                .child(icon(icons::X, 13.0, theme().text_ghost))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip {
                                        text: "Suspend session".into(),
                                    })
                                    .into()
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(
                                            SessionAction::CloseSessionKeepClone {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            }
                                            .into(),
                                        );
                                        cx.notify();
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("discard-{p_idx}-{s_idx}")))
                                .cursor_pointer()
                                .p(px(4.0))
                                .rounded(px(6.0))
                                .hover(|s| s.bg(theme().bg_raised))
                                .child(icon(icons::TRASH, 13.0, theme().text_ghost))
                                .tooltip(|_window, cx| {
                                    cx.new(|_| SimpleTooltip {
                                        text: "Discard session".into(),
                                    })
                                    .into()
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.pending_action = Some(
                                            SessionAction::RequestDiscardSession {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            }
                                            .into(),
                                        );
                                        cx.notify();
                                    }),
                                ),
                        ),
                );
            }

            sidebar_items.push(row.into_any_element());

            if is_confirming_discard {
                sidebar_items.push(confirm_strip(
                    format!("discard-{p_idx}-{s_idx}"),
                    "Discard this workspace? Uncommitted work is lost.",
                    "Discard",
                    px_indent::SESSION,
                    move |this, cx| {
                        this.pending_action = Some(
                            SessionAction::DiscardSession {
                                project_idx: p_idx,
                                session_idx: s_idx,
                            }
                            .into(),
                        );
                        cx.notify();
                    },
                    |this, cx| {
                        this.pending_action = Some(SessionAction::CancelDiscard.into());
                        cx.notify();
                    },
                    cx,
                ));
            }
        }

        // Archived sessions for this project. Hidden while text-filtering, and
        // hidden under active-only — an archive is inactive by definition.
        if !project.archives.is_empty() && !filtering && !active_only {
            let archive_count = project.archives.len();
            let is_confirming_delete_all = state.confirming.delete_all_archives == Some(p_idx);

            // Section header
            sidebar_items.push(
                div()
                    .id(SharedString::from(format!("archives-header-{p_idx}")))
                    .group(format!("archives-hdr-{p_idx}"))
                    // Opaque recessed fill: without it the row shows only the
                    // sidebar's 85%-translucent surface and washes out (bright
                    // window content blooms through, text becomes illegible).
                    .bg(theme().bg_sunken)
                    // Lineage rail — archives are variants of the trunk.
                    .border_l_2()
                    .border_color(with_alpha(theme().ready, 0.3))
                    .px(px(16.0))
                    .py(px(4.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_dim)
                            .child(format!("ARCHIVES ({archive_count})")),
                    )
                    .child(
                        // Bulk delete — hover-revealed like the per-row buttons
                        // so a destructive control is never one stray click away.
                        div()
                            .id(SharedString::from(format!("delallarchives-{p_idx}")))
                            .invisible()
                            .group_hover(format!("archives-hdr-{p_idx}"), |s| s.visible())
                            .cursor_pointer()
                            .px(px(4.0))
                            .py(px(1.0))
                            .rounded(px(6.0))
                            .text_size(px(11.0))
                            .text_color(theme().text_ghost)
                            .hover(|s| s.bg(theme().bg_raised).text_color(theme().danger))
                            .child("delete all")
                            .tooltip(|_window, cx| {
                                cx.new(|_| SimpleTooltip {
                                    text: "Delete every archive in this project".into(),
                                })
                                .into()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this: &mut AppState, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(
                                        ArchiveAction::RequestDeleteAllArchives {
                                            project_idx: p_idx,
                                        }
                                        .into(),
                                    );
                                    cx.notify();
                                }),
                            ),
                    )
                    .into_any_element(),
            );

            if is_confirming_delete_all {
                sidebar_items.push(confirm_strip(
                    format!("delallarchives-{p_idx}"),
                    format!(
                        "Delete all {archive_count} recovery refs? Unmerged work in them is \
                         lost. This cannot be undone."
                    ),
                    "Delete all",
                    px_indent::ARCHIVES_HEADER,
                    move |this, cx| {
                        this.pending_action =
                            Some(ArchiveAction::DeleteAllArchives { project_idx: p_idx }.into());
                        cx.notify();
                    },
                    |this, cx| {
                        this.pending_action = Some(ArchiveAction::CancelDeleteAllArchives.into());
                        cx.notify();
                    },
                    cx,
                ));
            }

            for (a_idx, archive) in project.archives.iter().enumerate() {
                let is_confirming_delete = state.confirming.delete_archive == Some((p_idx, a_idx));
                let display_label = archive.label.clone();
                let age = {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let delta = now.saturating_sub(archive.archived_at);
                    if delta < 60 {
                        "just now".to_string()
                    } else if delta < 3600 {
                        format!("{}m ago", delta / 60)
                    } else if delta < 86400 {
                        format!("{}h ago", delta / 3600)
                    } else {
                        format!("{}d ago", delta / 86400)
                    }
                };

                sidebar_items.push(
                    div()
                        .id(SharedString::from(format!("archive-{p_idx}-{a_idx}")))
                        .group(format!("arch-{p_idx}-{a_idx}"))
                        // Opaque recessed fill so the archive row stays legible
                        // over the translucent sidebar (see header note above).
                        .bg(theme().bg_sunken)
                        .border_l_2()
                        .border_color(with_alpha(theme().ready, 0.3))
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
                                .child(icon(icons::ARCHIVE, 12.0, with_alpha(theme().ready, 0.7)))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(theme().text_faint)
                                        .child(display_label),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(theme().text_ghost)
                                        .child(age),
                                ),
                        )
                        .child(
                            div()
                                .invisible()
                                .group_hover(format!("arch-{p_idx}-{a_idx}"), |s| s.visible())
                                .flex()
                                .flex_row()
                                .gap(px(4.0))
                                .child(
                                    // Restore button — reactivate as a suspended session
                                    div()
                                        .id(SharedString::from(format!("restore-{p_idx}-{a_idx}")))
                                        .cursor_pointer()
                                        .px(px(4.0))
                                        .py(px(1.0))
                                        .rounded(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(theme().accent)
                                        .hover(|s| s.bg(theme().bg_raised))
                                        .child("restore")
                                        .tooltip(|_window, cx| {
                                            cx.new(|_| SimpleTooltip {
                                                text: "Restore as active session".into(),
                                            })
                                            .into()
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this: &mut AppState, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.pending_action = Some(
                                                        ArchiveAction::RestoreArchive {
                                                            project_idx: p_idx,
                                                            archive_idx: a_idx,
                                                        }
                                                        .into(),
                                                    );
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                )
                                .child(
                                    // Merge button
                                    div()
                                        .id(SharedString::from(format!("merge-{p_idx}-{a_idx}")))
                                        .cursor_pointer()
                                        .px(px(4.0))
                                        .py(px(1.0))
                                        .rounded(px(6.0))
                                        .text_size(px(12.0))
                                        .text_color(theme().success)
                                        .hover(|s| s.bg(theme().bg_raised))
                                        .child("merge")
                                        .tooltip(|_window, cx| {
                                            cx.new(|_| SimpleTooltip {
                                                text: "Merge archive into project".into(),
                                            })
                                            .into()
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this: &mut AppState, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.pending_action = Some(
                                                        ArchiveAction::MergeArchive {
                                                            project_idx: p_idx,
                                                            archive_idx: a_idx,
                                                        }
                                                        .into(),
                                                    );
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                )
                                .child(
                                    // Delete button
                                    div()
                                        .id(SharedString::from(format!(
                                            "delarchive-{p_idx}-{a_idx}"
                                        )))
                                        .cursor_pointer()
                                        .p(px(3.0))
                                        .rounded(px(6.0))
                                        .hover(|s| s.bg(theme().bg_raised))
                                        .child(icon(icons::X, 12.0, theme().text_ghost))
                                        .tooltip(|_window, cx| {
                                            cx.new(|_| SimpleTooltip {
                                                text: "Delete archive".into(),
                                            })
                                            .into()
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this: &mut AppState, _event, _window, cx| {
                                                    cx.stop_propagation();
                                                    this.pending_action = Some(
                                                        ArchiveAction::RequestDeleteArchive {
                                                            project_idx: p_idx,
                                                            archive_idx: a_idx,
                                                        }
                                                        .into(),
                                                    );
                                                    cx.notify();
                                                },
                                            ),
                                        ),
                                ),
                        )
                        .into_any_element(),
                );
                if is_confirming_delete {
                    sidebar_items.push(confirm_strip(
                        format!("delarchive-{p_idx}-{a_idx}"),
                        "Delete this recovery ref? This cannot be undone.",
                        "Delete",
                        px_indent::ARCHIVE,
                        move |this, cx| {
                            this.pending_action = Some(
                                ArchiveAction::DeleteArchive {
                                    project_idx: p_idx,
                                    archive_idx: a_idx,
                                }
                                .into(),
                            );
                            cx.notify();
                        },
                        |this, cx| {
                            this.pending_action = Some(ArchiveAction::CancelDeleteArchive.into());
                            cx.notify();
                        },
                        cx,
                    ));
                }
                if let Some(message) = archive.merge_error.clone() {
                    sidebar_items.push(
                        div()
                            .border_l_2()
                            .border_color(with_alpha(theme().danger, 0.4))
                            .pl(px(40.0))
                            .pr(px(12.0))
                            .pb(px(4.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.0))
                                    .text_color(theme().danger)
                                    .child(message),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "retry-archive-{p_idx}-{a_idx}"
                                    )))
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(theme().accent)
                                    .child("Retry")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(
                                            move |this: &mut AppState, _event, _window, cx| {
                                                cx.stop_propagation();
                                                this.pending_action = Some(
                                                    ArchiveAction::MergeArchive {
                                                        project_idx: p_idx,
                                                        archive_idx: a_idx,
                                                    }
                                                    .into(),
                                                );
                                                cx.notify();
                                            },
                                        ),
                                    ),
                            )
                            .into_any_element(),
                    );
                }
            }
        }
    }

    sidebar_items
}

#[cfg(test)]
mod tests {
    // Import explicitly rather than `use super::*` — this module's parent does
    // `use gpui::*`, whose glob shadows the standard `#[test]` attribute with
    // gpui's own `test` macro. See src/session/mod.rs for the same note.
    use super::{
        active_only_hidden_counts, project_survives_active_only, session_survives_active_only,
    };
    use crate::actions::SessionCursor;
    use crate::project::Project;
    use crate::session::{Session, SessionStatus};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    const LIVE: [SessionStatus; 4] = [
        SessionStatus::Running,
        SessionStatus::Idle,
        SessionStatus::AwaitingInput,
        SessionStatus::ResponseReady,
    ];

    fn session(status: SessionStatus) -> Session {
        let now = SystemTime::now();
        let mut s = Session::suspended_from_persisted(
            "id".into(),
            "label".into(),
            now,
            now,
            Duration::ZERO,
            None,
            false,
        );
        s.status = status;
        s
    }

    fn project(statuses: &[SessionStatus]) -> Project {
        let mut p = Project::new("proj".into(), PathBuf::from("/tmp/proj"));
        p.sessions = statuses.iter().copied().map(session).collect();
        p
    }

    fn cursor(project_idx: usize, session_idx: usize) -> Option<SessionCursor> {
        Some(SessionCursor {
            project_idx,
            session_idx,
        })
    }

    // ── session predicate ──────────────────────────────────────────

    #[test]
    fn suspended_session_is_filtered_out() {
        assert!(!session_survives_active_only(
            SessionStatus::Suspended,
            false
        ));
    }

    #[test]
    fn done_session_is_filtered_out() {
        // Done = the agent process exited; the row's action is "Resume
        // session", same as Suspended. Clutter, not live work.
        assert!(!session_survives_active_only(SessionStatus::Done, false));
    }

    #[test]
    fn live_sessions_survive() {
        for status in LIVE {
            assert!(
                session_survives_active_only(status, false),
                "{status:?} should survive the active-only filter"
            );
        }
    }

    #[test]
    fn selected_session_survives_even_when_inactive() {
        // A failed resume leaves the *selected* session Suspended with an
        // error + Retry button. Hiding it would strand the user.
        assert!(session_survives_active_only(SessionStatus::Suspended, true));
        assert!(session_survives_active_only(SessionStatus::Done, true));
    }

    // ── project predicate ──────────────────────────────────────────

    #[test]
    fn project_with_only_inactive_sessions_is_filtered_out() {
        let p = project(&[SessionStatus::Suspended, SessionStatus::Done]);
        assert!(!project_survives_active_only(&p, 0, None));
    }

    #[test]
    fn project_survives_on_a_single_live_session() {
        let p = project(&[
            SessionStatus::Suspended,
            SessionStatus::Suspended,
            SessionStatus::Running,
        ]);
        assert!(project_survives_active_only(&p, 0, None));
    }

    #[test]
    fn empty_project_is_filtered_out() {
        assert!(!project_survives_active_only(&project(&[]), 0, None));
    }

    #[test]
    fn project_survives_while_a_clone_is_in_flight() {
        // Otherwise a brand-new session's project vanishes mid-clone and the
        // session being created becomes unreachable.
        let mut p = project(&[SessionStatus::Suspended]);
        p.loading_sessions.push(crate::project::LoadingSession {
            id: "loading".into(),
            label: "new session".into(),
            status: "Cloning…".into(),
        });
        assert!(project_survives_active_only(&p, 0, None));
    }

    #[test]
    fn project_survives_because_its_inactive_session_is_selected() {
        let p = project(&[SessionStatus::Suspended]);
        assert!(!project_survives_active_only(&p, 0, None));
        assert!(project_survives_active_only(&p, 0, cursor(0, 0)));
        // …but only for the project the cursor actually points at.
        assert!(!project_survives_active_only(&p, 1, cursor(0, 0)));
    }

    // ── hidden counts ──────────────────────────────────────────────

    #[test]
    fn hidden_counts_tally_sessions_and_projects() {
        let projects = vec![
            // Fully inactive → 2 sessions + 1 project hidden.
            project(&[SessionStatus::Suspended, SessionStatus::Done]),
            // Mixed → 1 session hidden, project stays.
            project(&[SessionStatus::Suspended, SessionStatus::Running]),
            // Fully live → nothing hidden.
            project(&[SessionStatus::Running]),
        ];
        assert_eq!(active_only_hidden_counts(&projects, None), (3, 1));
    }

    #[test]
    fn hidden_counts_exclude_the_selected_session() {
        let projects = vec![project(&[SessionStatus::Suspended, SessionStatus::Done])];
        // Selecting the Suspended row keeps it (and its project) visible.
        assert_eq!(active_only_hidden_counts(&projects, cursor(0, 0)), (1, 0));
    }

    #[test]
    fn hidden_counts_are_zero_when_everything_is_live() {
        let projects = vec![project(&LIVE)];
        assert_eq!(active_only_hidden_counts(&projects, None), (0, 0));
    }
}
