//! Global command & discovery palette (DEV-41).
//!
//! A single Cmd+Shift+P surface to switch project/session, open any major
//! surface, run the other palettes, and create sessions. Commands are
//! context-filtered (unavailable ones explain why rather than vanishing),
//! destructive ones are labelled, and each shows its shortcut for discovery.

use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::actions::SessionCursor;
use crate::app_state::{AppState, MainTab};
use crate::theme::theme;

/// What a command does when run. Every variant is executable from a listener
/// that has `(window, cx)`, so no deferred plumbing is needed.
#[derive(Clone)]
enum Cmd {
    Tab(MainTab),
    ToggleSidebar,
    ToggleChanges,
    GoToFile,
    SearchProject,
    OpenScratchPad,
    NewSession,
    SwitchSession(SessionCursor),
    DiscardChanges,
}

/// A palette row: what it says, whether it can run, and what it does.
struct CommandItem {
    title: String,
    subtitle: Option<String>,
    /// Extra terms folded into fuzzy matching (never shown).
    keywords: String,
    shortcut: Option<&'static str>,
    destructive: bool,
    /// `Some(reason)` renders the row greyed and blocks execution.
    disabled: Option<String>,
    cmd: Cmd,
}

pub(crate) struct CommandPalette {
    pub(crate) query: String,
    pub(crate) selected: usize,
    /// Indices into the freshly-built command list, after filtering.
    pub(crate) results: Vec<usize>,
}

impl AppState {
    pub(crate) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.command_palette = Some(CommandPalette {
            query: String::new(),
            selected: 0,
            results: Vec::new(),
        });
        self.recompute_commands();
        self.command_palette_input
            .update(cx, |i, cx| i.set_text_silent("", cx));
        self.command_palette_input
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_command_palette(&mut self, cx: &mut Context<Self>) {
        if self.command_palette.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn set_command_query(&mut self, query: &str, cx: &mut Context<Self>) {
        if let Some(p) = self.command_palette.as_mut() {
            p.query = query.to_string();
        }
        self.recompute_commands();
        cx.notify();
    }

    pub(crate) fn move_command_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        if let Some(p) = self.command_palette.as_mut() {
            if p.results.is_empty() {
                return;
            }
            let len = p.results.len() as i32;
            let mut i = p.selected as i32 + delta;
            if i < 0 {
                i = len - 1;
            } else if i >= len {
                i = 0;
            }
            p.selected = i as usize;
            cx.notify();
        }
    }

    /// Re-filter the command list against the current query.
    fn recompute_commands(&mut self) {
        let Some(query) = self
            .command_palette
            .as_ref()
            .map(|p| p.query.trim().to_lowercase())
        else {
            return;
        };
        let commands = self.build_commands();
        let mut results: Vec<usize> = if query.is_empty() {
            (0..commands.len()).collect()
        } else {
            let mut scored: Vec<(i32, usize)> = commands
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    let hay = format!("{} {}", c.title, c.keywords).to_lowercase();
                    super::palette::fuzzy_score(&query, &hay).map(|s| (s, i))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            scored.into_iter().map(|(_, i)| i).collect()
        };
        // Keep runnable commands ahead of disabled ones for equal relevance.
        results.sort_by_key(|&i| commands[i].disabled.is_some());
        if let Some(p) = self.command_palette.as_mut() {
            p.results = results;
            p.selected = 0;
        }
    }

    /// Run the highlighted command (unless disabled) and dismiss the palette.
    pub(crate) fn confirm_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self
            .command_palette
            .as_ref()
            .and_then(|p| p.results.get(p.selected).copied())
        else {
            return;
        };
        let mut commands = self.build_commands();
        if idx >= commands.len() {
            return;
        }
        let item = commands.swap_remove(idx);
        if item.disabled.is_some() {
            return; // prerequisites unmet
        }
        self.command_palette = None;
        self.run_command(item.cmd, window, cx);
        cx.notify();
    }

    fn run_command(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        match cmd {
            Cmd::Tab(tab) => self.main_tab = tab,
            Cmd::ToggleSidebar => self.sidebar.visible = !self.sidebar.visible,
            Cmd::ToggleChanges => self.right_panel.visible = !self.right_panel.visible,
            Cmd::GoToFile => self.open_file_palette(window, cx),
            Cmd::SearchProject => self.open_search(window, cx),
            Cmd::OpenScratchPad => self.open_scratch_pad(window, cx),
            Cmd::NewSession => {
                if let Some(p_idx) = self.active.map(|c| c.project_idx) {
                    self.open_new_session_modal(p_idx, window, cx);
                }
            }
            Cmd::SwitchSession(cursor) => self.active = Some(cursor),
            Cmd::DiscardChanges => {
                // Open the standard discard confirmation prompt — safe; the
                // actual discard still requires the user to confirm.
                if let Some(cursor) = self.active {
                    self.sidebar.visible = true;
                    self.confirming.discard = Some(cursor);
                }
            }
        }
    }

    /// Build the context-filtered command list for the current state.
    fn build_commands(&self) -> Vec<CommandItem> {
        let mut cmds = Vec::new();
        let has_session = self.active.is_some();

        let mut push = |title: &str,
                        subtitle: Option<&str>,
                        keywords: &str,
                        shortcut: Option<&'static str>,
                        destructive: bool,
                        disabled: Option<String>,
                        cmd: Cmd| {
            cmds.push(CommandItem {
                title: title.to_string(),
                subtitle: subtitle.map(|s| s.to_string()),
                keywords: keywords.to_string(),
                shortcut,
                destructive,
                disabled,
                cmd,
            });
        };

        // ── Surfaces ──────────────────────────────────────────────
        push(
            "Go to Claude",
            Some("Main tab"),
            "surface tab agent",
            None,
            false,
            None,
            Cmd::Tab(MainTab::Claude),
        );
        push(
            "Go to Reader",
            Some("Files & source"),
            "surface tab files source read",
            None,
            false,
            None,
            Cmd::Tab(MainTab::Reader),
        );
        push(
            "Go to Transcript",
            Some("Session transcript"),
            "surface tab log history",
            Some("⌘⇧R"),
            false,
            None,
            Cmd::Tab(MainTab::Transcript),
        );
        let browser_disabled = if self.browser_tab_available() {
            None
        } else {
            Some("No linked Chrome tab for this session".to_string())
        };
        push(
            "Go to Browser",
            Some("Linked Chrome tab"),
            "surface tab web chrome",
            None,
            false,
            browser_disabled,
            Cmd::Tab(MainTab::Browser),
        );

        // ── Panels & overlays ─────────────────────────────────────
        push(
            "Toggle Sidebar",
            None,
            "panel projects sessions hide show",
            Some("⌘B"),
            false,
            None,
            Cmd::ToggleSidebar,
        );
        push(
            "Toggle Changes Panel",
            None,
            "panel git diff review",
            None,
            false,
            None,
            Cmd::ToggleChanges,
        );
        push(
            "Go to File…",
            Some("Fuzzy file search"),
            "open find path cmd-p",
            Some("⌘P"),
            false,
            None,
            Cmd::GoToFile,
        );
        push(
            "Search Project…",
            Some("Content & symbols"),
            "grep find text symbol",
            Some("⌘⇧F"),
            false,
            None,
            Cmd::SearchProject,
        );
        push(
            "Open Scratch Pad",
            None,
            "notes compose",
            Some("⌘K"),
            false,
            None,
            Cmd::OpenScratchPad,
        );

        // ── Session lifecycle ─────────────────────────────────────
        let new_disabled = if has_session {
            None
        } else {
            Some("Select a project first".to_string())
        };
        push(
            "New Session…",
            Some("In the current project"),
            "create add worktree",
            None,
            false,
            new_disabled,
            Cmd::NewSession,
        );
        let discard_disabled = if has_session {
            None
        } else {
            Some("No active session".to_string())
        };
        push(
            "Discard Session Changes…",
            Some("Reset the session worktree"),
            "delete reset revert destructive",
            None,
            true,
            discard_disabled,
            Cmd::DiscardChanges,
        );

        // ── Switch project / session ──────────────────────────────
        for (p_idx, project) in self.projects.iter().enumerate() {
            for (s_idx, session) in project.sessions.iter().enumerate() {
                let is_active = self.active
                    == Some(SessionCursor {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    });
                if is_active {
                    continue;
                }
                cmds.push(CommandItem {
                    title: format!("Switch to {} / {}", project.name, session.label),
                    subtitle: Some("Project · session".to_string()),
                    keywords: format!(
                        "switch go project session {} {}",
                        project.name, session.label
                    ),
                    shortcut: None,
                    destructive: false,
                    disabled: None,
                    cmd: Cmd::SwitchSession(SessionCursor {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    }),
                });
            }
        }

        cmds
    }

    pub(crate) fn render_command_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div();
        let Some(palette) = self.command_palette.as_ref() else {
            return root;
        };
        let commands = self.build_commands();

        let mut list = div().flex().flex_col().py(px(4.0));
        if palette.results.is_empty() {
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme().text_faint)
                    .child("No matching commands"),
            );
        }
        for (row, &cmd_idx) in palette.results.iter().enumerate() {
            let Some(item) = commands.get(cmd_idx) else {
                continue;
            };
            let selected = row == palette.selected;
            let disabled = item.disabled.is_some();
            let title_color = if disabled {
                theme().text_ghost
            } else if item.destructive {
                theme().danger
            } else {
                theme().text_primary
            };
            let sub = item.disabled.clone().or_else(|| item.subtitle.clone());
            let sub_color = if disabled {
                theme().warning
            } else {
                theme().text_ghost
            };
            let shortcut = item.shortcut;
            let run_idx = cmd_idx;

            list = list.child(
                div()
                    .id(("command-row", row))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(5.0))
                    .when(selected, |d| d.bg(theme().bg_active))
                    .when(!disabled, |d| {
                        d.cursor_pointer().hover(|s| s.bg(theme().bg_hover))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(title_color)
                                    .child(item.title.clone()),
                            )
                            .when_some(sub, |d, s| {
                                d.child(div().text_size(px(10.0)).text_color(sub_color).child(s))
                            }),
                    )
                    .when_some(shortcut, |d, sc| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.0))
                                .text_color(theme().text_faint)
                                .font_family(crate::theme::FONT_MONO)
                                .child(sc),
                        )
                    })
                    .when(!disabled, |d| {
                        d.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _e, window, cx| {
                                if let Some(p) = this.command_palette.as_mut() {
                                    if let Some(pos) = p.results.iter().position(|&i| i == run_idx)
                                    {
                                        p.selected = pos;
                                    }
                                }
                                this.confirm_command(window, cx);
                            }),
                        )
                    }),
            );
        }

        let panel = div()
            .w(px(600.0))
            .max_h(px(460.0))
            .flex()
            .flex_col()
            .bg(theme().bg_surface)
            .border_1()
            .border_color(theme().border_strong)
            .rounded(px(10.0))
            .shadow_lg()
            .font_family(crate::theme::FONT_UI)
            .child(
                div()
                    .p(px(8.0))
                    .border_b_1()
                    .border_color(theme().border_subtle)
                    .child(self.command_palette_input.clone()),
            )
            .child(
                div()
                    .id("command-results")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(list),
            );

        root = root.child(deferred(
            div()
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(80.0))
                .bg(hsla(0.0, 0.0, 0.0, 0.35))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut Self, _e, _w, cx| this.close_command_palette(cx)),
                )
                .on_key_down(
                    cx.listener(|this: &mut Self, event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => this.close_command_palette(cx),
                            "enter" => this.confirm_command(window, cx),
                            "down" => this.move_command_selection(1, cx),
                            "up" => this.move_command_selection(-1, cx),
                            _ => {}
                        }
                    }),
                )
                .child(
                    div()
                        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                        .child(panel),
                ),
        ));
        root
    }
}
