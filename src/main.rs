mod actions;
mod agents;
mod app_state;
mod browser;
mod terminal;
mod sidebar;
mod clone;
mod config;
mod drawer;
mod editor;
mod errors;
mod git;
mod hook_events;
mod hooks;
mod keymap;
mod naming;
mod new_session_modal;
mod pending_actions;
mod platform;
mod project;
mod repositories;
mod rich;
mod scratch_pad;
mod session;
mod session_ops;
mod settings;
mod settings_window;
mod state;
mod stream;
mod text_input;
mod transcript;
mod trust;

use actions::{
    BrowserAction, DrawerAction, OverlayAction, ProjectAction, SessionAction, SessionCursor,
    SettingsAction, SidebarAction,
};
use app_state::{
    AppState, ConfirmationState, DrawerState, EditorState, MainTab, RichState, RightPanelState,
    SidebarState, DRAWER_MIN_HEIGHT, RIGHT_SIDEBAR_MIN_WIDTH, SIDEBAR_MIN_WIDTH,
};
use gpui::*;
use project::Project;
actions!(allele, [About, Quit, ToggleSidebarAction, ToggleDrawerAction, OpenSettings, OpenScratchPadAction, ToggleTranscriptTabAction]);
use session::{Session, SessionStatus};
use settings::{ProjectSave, Settings};
use state::{ArchivedSession, PersistedSession, PersistedState};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

/// A minimal tooltip view for hover text on buttons.
pub(crate) struct SimpleTooltip {
    pub(crate) text: SharedString,
}

impl Render for SimpleTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(rgb(0x1e1e2e))
            .border_1()
            .border_color(rgb(0x45475a))
            .text_size(px(11.0))
            .text_color(rgb(0xcdd6f4))
            .child(self.text.clone())
    }
}

/// Check whether Claude Code has on-disk history for a given session ID.
///
/// Claude stores each conversation at `~/.claude/projects/<slug>/<id>.jsonl`,
/// where `<slug>` is the cwd encoded with `/` → `-`. We don't assume the slug
/// format — just scan the `projects` directory for any matching filename.
/// Returns `false` on any IO error so the caller falls back to `--session-id`
/// (fresh session, same UUID) rather than failing into "Session ended".
fn claude_session_history_exists(session_id: &str) -> bool {
    let Some(home) = dirs::home_dir() else { return false; };
    let projects_dir = home.join(".claude").join("projects");
    let needle = format!("{session_id}.jsonl");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else { return false; };
    for entry in entries.flatten() {
        let sub = entry.path();
        if !sub.is_dir() {
            continue;
        }
        if sub.join(&needle).exists() {
            return true;
        }
    }
    false
}

impl AppState {
    /// Get the currently active session, if any.
    pub(crate) fn active_session(&self) -> Option<&Session> {
        let cursor = self.active?;
        self.projects
            .get(cursor.project_idx)?
            .sessions
            .get(cursor.session_idx)
    }

    /// Open the scratch pad compose overlay, or re-focus it if already open.
    pub(crate) fn open_scratch_pad(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Collect per-project history entries before creating the overlay so
        // we can seed the history panel with them.
        let project_id = self
            .active
            .and_then(|cursor| self.projects.get(cursor.project_idx))
            .map(|p| p.id.clone());
        let entries: Vec<scratch_pad::HistoryEntry> = match project_id.as_deref() {
            Some(pid) => self
                .scratch_pad_history
                .iter()
                .filter(|e| e.project_id == pid)
                .map(|e| scratch_pad::HistoryEntry {
                    id: e.id.clone(),
                    text: e.text.clone(),
                    created_at: e.created_at,
                })
                .collect(),
            None => Vec::new(),
        };

        if self.scratch_pad.is_none() {
            let entity = cx.new(|cx| {
                let mut pad = scratch_pad::ScratchPad::new(cx);
                pad.set_history(entries.clone());
                pad
            });
            cx.subscribe(
                &entity,
                |this: &mut Self, _pad, event: &scratch_pad::ScratchPadEvent, cx| {
                    match event {
                        scratch_pad::ScratchPadEvent::Send { text, attachments } => {
                            this.scratch_pad_send(text.clone(), attachments.clone(), cx);
                            this.scratch_pad = None;
                            this.pending_action = Some(SessionAction::FocusActive.into());
                            cx.notify();
                        }
                        scratch_pad::ScratchPadEvent::Close => {
                            this.scratch_pad = None;
                            this.pending_action = Some(SessionAction::FocusActive.into());
                            cx.notify();
                        }
                        scratch_pad::ScratchPadEvent::DeleteHistoryEntry { id } => {
                            this.delete_scratch_history_entry(id.clone(), cx);
                        }
                    }
                },
            )
            .detach();
            self.scratch_pad = Some(entity);
        } else if let Some(pad) = self.scratch_pad.as_ref() {
            // Overlay already open — refresh history in case it has changed
            // since it was first opened.
            pad.update(cx, |pad, _| pad.set_history(entries));
        }
        if let Some(pad) = self.scratch_pad.as_ref() {
            let fh = pad.read(cx).focus_handle();
            fh.focus(window, cx);
        }
        cx.notify();
    }

    /// Flush the composed scratch-pad payload to the active session's PTY.
    /// Mirrors the bracketed-paste logic in `terminal_view.rs` so behaviour
    /// is identical to a manual Cmd+V, then writes `\r` to submit.
    fn scratch_pad_send(
        &mut self,
        text: String,
        attachments: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.active_session() else { return; };
        let Some(tv) = session.terminal_view.clone() else { return; };

        // Capture this submission into per-project scratch history. Keyed
        // by the active session's project so the next Cmd+K in the same
        // project can recall it.
        if !text.trim().is_empty() {
            if let Some(cursor) = self.active {
                if let Some(project) = self.projects.get(cursor.project_idx) {
                    let entry = state::ScratchPadEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        project_id: project.id.clone(),
                        text: text.clone(),
                        created_at: std::time::SystemTime::now(),
                    };
                    self.scratch_pad_history.insert(0, entry);
                    // Trim this project's entries to the per-project limit.
                    let pid = project.id.clone();
                    let mut count = 0usize;
                    self.scratch_pad_history.retain(|e| {
                        if e.project_id != pid {
                            return true;
                        }
                        count += 1;
                        count <= state::SCRATCH_HISTORY_PER_PROJECT_LIMIT
                    });
                    self.mark_state_dirty();
                }
            }
        }

        // Prefix each attachment with `@` so Claude Code treats it as a file
        // mention (reads the file) rather than literal text.
        let mut payload = String::new();
        for p in &attachments {
            payload.push('@');
            payload.push_str(&p.to_string_lossy());
            payload.push('\n');
        }
        payload.push_str(&text);

        // Claude Code's input editor has a paste-detection heuristic: when
        // lots of bytes arrive back-to-back, the trailing `\r` gets absorbed
        // into the paste as another newline instead of firing the submit.
        // Wrap the payload in bracketed paste so CC knows where the paste
        // ends, then dispatch the `\r` after a short gap so it's treated as
        // a real Enter keystroke rather than pasted content.
        if let Some(terminal) = tv.read(cx).pty() {
            terminal.write(b"\x1b[200~");
            terminal.write(payload.as_bytes());
            terminal.write(b"\x1b[201~");
        }
        let tv_weak = tv.downgrade();
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(80))
                .await;
            let _ = cx.update(|cx| {
                if let Some(tv) = tv_weak.upgrade() {
                    if let Some(terminal) = tv.read(cx).pty() {
                        terminal.write(b"\r");
                    }
                }
            });
        })
        .detach();
    }

    /// Remove a scratch pad history entry by id, persist the change, and
    /// refresh the open overlay so the row disappears immediately.
    fn delete_scratch_history_entry(&mut self, id: String, cx: &mut Context<Self>) {
        let before = self.scratch_pad_history.len();
        self.scratch_pad_history.retain(|e| e.id != id);
        if self.scratch_pad_history.len() == before {
            return;
        }
        self.mark_state_dirty();

        // Refresh the overlay's in-memory history list so the UI updates
        // without waiting for re-open.
        if let Some(pad) = self.scratch_pad.as_ref() {
            let project_id = self
                .active
                .and_then(|cursor| self.projects.get(cursor.project_idx))
                .map(|p| p.id.clone());
            let entries: Vec<scratch_pad::HistoryEntry> = match project_id.as_deref() {
                Some(pid) => self
                    .scratch_pad_history
                    .iter()
                    .filter(|e| e.project_id == pid)
                    .map(|e| scratch_pad::HistoryEntry {
                        id: e.id.clone(),
                        text: e.text.clone(),
                        created_at: e.created_at,
                    })
                    .collect(),
                None => Vec::new(),
            };
            pad.update(cx, |pad, pad_cx| {
                pad.set_history(entries);
                pad_cx.notify();
            });
        }
        cx.notify();
    }

    /// Should the Browser tab appear in the tab strip? Requires both the
    /// feature flag to be on and the active session to have a preview URL
    /// recorded (populated from `allele.json` by apply_project_config).
    fn browser_tab_available(&self) -> bool {
        if !self.user_settings.browser_integration_enabled {
            return false;
        }
        self.active_session()
            .and_then(|s| s.browser_last_url.as_ref())
            .is_some()
    }

    /// Tab strip above the main content column: Claude / Editor.
    fn render_main_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.main_tab;

        let tab = |id: &'static str, label: &'static str, tab: MainTab| {
            let is_active = tab == active;
            let bg = if is_active { 0x313244 } else { 0x1e1e2e };
            let fg = if is_active { 0xcdd6f4 } else { 0xa6adc8 };
            div()
                .id(id)
                .px(px(12.0))
                .py(px(4.0))
                .rounded(px(4.0))
                .bg(rgb(bg))
                .text_size(px(11.0))
                .text_color(rgb(fg))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x45475a)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _event, _window, cx| {
                        let previous = this.main_tab;
                        this.main_tab = tab;
                        // Entering the Browser tab syncs Chrome to the
                        // active session (activates its tab or creates one).
                        if tab == MainTab::Browser && previous != MainTab::Browser {
                            this.pending_action =
                                Some(BrowserAction::SyncBrowserToActiveSession.into());
                        }
                        cx.notify();
                    }),
                )
        };

        let mut strip = div()
            .w_full()
            .flex_shrink_0()
            .px(px(8.0))
            .py(px(4.0))
            .bg(rgb(0x181825))
            .border_b_1()
            .border_color(rgb(0x313244))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(tab("main-tab-claude", "Claude", MainTab::Claude))
            .child(tab("main-tab-editor", "Editor", MainTab::Editor))
            .child(tab("main-tab-transcript", "Transcript", MainTab::Transcript));
        if self.browser_tab_available() {
            strip = strip.child(tab("main-tab-browser", "Browser", MainTab::Browser));
        }
        strip
    }

    // ── Rich Sidecar (Transcript tab) ────────────────────────────────
    //
    // A read-only structured view of the active session's Claude Code
    // transcript. Tails `~/.claude/projects/<dashed-cwd>/<session>.jsonl`
    // (+ subagent sidechains) and renders via `rich::RichView`. Prompts
    // composed here are routed into the active PTY via `scratch_pad_send`
    // — identical path to the Scratch Pad overlay, never any programmatic
    // drive of `claude`.

    /// Called on every spinner tick. No-op when no tailer has been built
    /// yet (user hasn't opened the Transcript tab on this session).
    fn poll_transcript_tailer(&mut self, cx: &mut Context<Self>) {
        let Some(tailer) = self.rich.transcript_tailer.as_mut() else { return };
        let events = tailer.poll();
        if events.is_empty() { return; }
        let Some(view) = self.rich.view.as_ref().cloned() else { return };
        view.update(cx, |rv, cx| {
            for ev in events {
                match ev {
                    transcript::TranscriptEvent::Rich(event) => rv.apply_event(event, cx),
                    transcript::TranscriptEvent::UserPrompt(text) => rv.push_user_prompt(text, cx),
                }
            }
        });
    }

    /// Build the RichView + TranscriptTailer for the active session,
    /// rebuilding when the active session changes. Returns the entity
    /// to render, or `None` when there is no active session.
    fn ensure_rich_view(&mut self, cx: &mut Context<Self>) -> Option<Entity<rich::RichView>> {
        let active = self.active?;
        let changed = self.rich.cursor != Some(active);
        if !changed && self.rich.view.is_some() {
            return self.rich.view.clone();
        }

        let (allele_session_id, cwd) = {
            let project = self.projects.get(active.project_idx)?;
            let session = project.sessions.get(active.session_idx)?;
            let cwd = session
                .clone_path
                .clone()
                .unwrap_or_else(|| project.source_path.clone());
            (session.id.clone(), cwd)
        };
        // Transcript density differs from terminal density — the
        // terminal is tuned for cramming rows, whereas the Rich view is
        // reading prose with nested cards. Bias the transcript font up
        // relative to the terminal setting, with a 15pt floor so it
        // stays legible even when the user has shrunk the terminal.
        let font_size = (self.user_settings.font_size + 2.0).max(15.0);

        let view = cx.new(|cx| rich::RichView::new(cx, allele_session_id.clone(), font_size));

        // ComposeBar submits bubble up as RichViewEvent::Submit. Route
        // them into the active PTY via the same bracketed-paste path
        // the Scratch Pad uses. Nothing here spawns or talks to the
        // `claude` binary directly.
        cx.subscribe(&view, |this: &mut Self, _v, event: &rich::RichViewEvent, cx| {
            match event {
                rich::RichViewEvent::Submit { text, attachments } => {
                    let paths: Vec<PathBuf> =
                        attachments.iter().map(|a| a.path.clone()).collect();
                    this.scratch_pad_send(text.clone(), paths, cx);
                }
            }
        })
        .detach();

        // Scope the tailer to the active session's JSONL. We point at
        // the ANTICIPATED path (derived from session id + dashed cwd)
        // even if the file doesn't exist yet — `TailedFile::read_new`
        // silently no-ops on a missing file and picks up content from
        // byte 0 the moment Claude Code writes it. This means opening
        // the Transcript tab on a brand-new session shows the internal
        // empty state ("Send a message to start.") and auto-populates
        // as soon as the first turn lands on disk, without any re-wire.
        self.rich.transcript_tailer = transcript::expected_session_jsonl(&cwd, &allele_session_id)
            .map(transcript::TranscriptTailer::new);
        self.rich.view = Some(view);
        self.rich.cursor = Some(active);
        self.rich.view.clone()
    }

    /// Render the Transcript tab. Shows a "no active session" placeholder
    /// when nothing is selected; otherwise renders the RichView, which
    /// handles its own empty state internally (e.g. "Send a message to
    /// start.") and auto-populates as soon as the tailer sees the first
    /// appended line.
    fn render_transcript_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.ensure_rich_view(cx) else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .bg(rgb(0x1e1e2e))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(0xcdd6f4))
                        .child("No active session"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(0x6c7086))
                        .child("Open a project and start a session to see its transcript here."),
                )
                .into_any_element();
        };
        view.into_any_element()
    }

    /// Compute the global-screen rect where Chrome should sit when the
    /// Browser tab is active. Uses the Allele window's current bounds minus
    /// the sidebar(s), tab strip, and drawer. Coords are top-left origin in
    /// points, matching the macOS Accessibility API.
    /// Status panel for the Browser tab. No Chrome process is embedded —
    /// this panel only shows the current sync state (Chrome running?
    /// session linked to a tab?) and exposes a Close button for the
    /// current session's tab.
    fn render_browser_placeholder(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.user_settings.browser_integration_enabled {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .bg(rgb(0x1e1e2e))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(0xcdd6f4))
                        .child("Chrome browser integration is disabled"),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(0x6c7086))
                        .child(
                            "Enable it in Allele → Settings → Browser to link \
                             each session to a tab in your running Chrome.",
                        ),
                );
        }
        let active = self.active;
        let chrome_up = browser::chrome_running();
        let session_tab = self.active_session().and_then(|s| s.browser_tab_id);

        let headline = if !chrome_up {
            "Google Chrome is not running".to_string()
        } else if let Some(id) = session_tab {
            format!("Linked to Chrome tab #{id}")
        } else if self.active_session().is_some() {
            "No Chrome tab yet for this session".to_string()
        } else {
            "Open a session to use the Browser tab".to_string()
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .bg(rgb(0x1e1e2e));

        root = root.child(
            div()
                .text_size(px(13.0))
                .text_color(rgb(0xcdd6f4))
                .child(headline),
        );

        if let Some(url) = self
            .active_session()
            .and_then(|s| s.browser_last_url.as_ref())
        {
            root = root.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(0x89b4fa))
                    .child(format!("Preview URL: {url}")),
            );
        }

        if !self.browser_status.is_empty() {
            root = root.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(0x6c7086))
                    .child(self.browser_status.clone()),
            );
        }

        if chrome_up && self.active_session().is_some() {
            let mut buttons = div().flex().flex_row().gap(px(8.0));

            buttons = buttons.child(
                div()
                    .id("browser-sync-btn")
                    .cursor_pointer()
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(rgb(0x89b4fa))
                    .text_size(px(11.0))
                    .text_color(rgb(0x1e1e2e))
                    .hover(|s| s.bg(rgb(0x74c7ec)))
                    .child("Open in Chrome")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut Self, _event, _window, cx| {
                            this.pending_action =
                                Some(BrowserAction::SyncBrowserToActiveSession.into());
                            cx.notify();
                        }),
                    ),
            );

            if let (Some(cur), Some(_)) = (active, session_tab) {
                buttons = buttons.child(
                    div()
                        .id("browser-close-btn")
                        .cursor_pointer()
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(4.0))
                        .bg(rgb(0x45475a))
                        .text_size(px(11.0))
                        .text_color(rgb(0xcdd6f4))
                        .hover(|s| s.bg(rgb(0x585b70)))
                        .child("Close Chrome tab")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _event, _window, cx| {
                                this.pending_action = Some(
                                    BrowserAction::CloseBrowserTabForSession {
                                        project_idx: cur.project_idx,
                                        session_idx: cur.session_idx,
                                    }
                                    .into(),
                                );
                                cx.notify();
                            }),
                        ),
                );
            }

            root = root.child(buttons);
        }

        root = root.child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(0x6c7086))
                .child(
                    "Allow Automation for Google Chrome in System Settings \
                     → Privacy & Security → Automation if tab switching \
                     fails silently.",
                ),
        );

        root
    }

    /// Activate the active session's Chrome tab, creating one if the id
    /// is unset or stale. Updates `browser_status` for UI feedback and
    /// persists the resolved tab id.
    pub(crate) fn sync_browser_to_active(&mut self) {
        if !self.user_settings.browser_integration_enabled {
            self.browser_status.clear();
            return;
        }
        let Some(cursor) = self.active else {
            self.browser_status.clear();
            return;
        };
        if !browser::chrome_running() {
            self.browser_status =
                "Start Google Chrome and try again.".to_string();
            return;
        }

        let stored = self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .and_then(|s| s.browser_tab_id);
        let fallback_url = self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .and_then(|s| s.browser_last_url.clone())
            .unwrap_or_else(|| "about:blank".to_string());

        if let Some(id) = stored {
            if browser::activate_tab(id) {
                self.browser_status = format!("Activated tab #{id}");
                return;
            }
        }

        match browser::create_tab(&fallback_url) {
            Some(new_id) => {
                if let Some(session) = self
                    .projects
                    .get_mut(cursor.project_idx)
                    .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                {
                    session.browser_tab_id = Some(new_id);
                    if session.browser_last_url.is_none() {
                        session.browser_last_url = Some(fallback_url);
                    }
                }
                self.browser_status = format!("Created tab #{new_id}");
                self.mark_state_dirty();
            }
            None => {
                self.browser_status = "Could not create Chrome tab (check \
                    Automation permission)."
                    .to_string();
            }
        }
    }

    /// Floating right-click menu for a session row. Returns an empty `Div`
    /// when no context menu is open, so callers can attach unconditionally.
    fn render_session_context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div();
        let Some((cursor, position)) = self.session_context_menu else {
            return root;
        };
        let p_idx = cursor.project_idx;
        let s_idx = cursor.session_idx;

        let is_pinned = self
            .projects
            .get(p_idx)
            .and_then(|p| p.sessions.get(s_idx))
            .map(|s| s.pinned)
            .unwrap_or(false);
        let pin_label = if is_pinned { "Unpin" } else { "Pin" };

        let menu_item = |id: &'static str, label: &str, color: u32| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(color))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x45475a)))
                .child(label.to_string())
        };

        let separator = || {
            div()
                .w_full()
                .h(px(1.0))
                .my(px(4.0))
                .bg(rgb(0x313244))
        };

        let menu = div()
            .flex()
            .flex_col()
            .min_w(px(200.0))
            .py(px(4.0))
            .bg(rgb(0x181825))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded(px(6.0))
            .shadow_md()
            .child(
                menu_item("session-ctx-edit", "Edit Session…", 0xcdd6f4)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.pending_action = Some(SessionAction::EditSession { project_idx: p_idx, session_idx: s_idx }.into());
                        cx.notify();
                    })),
            )
            .child(separator())
            .child(
                menu_item("session-ctx-reveal", "Open in Finder", 0xcdd6f4)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.pending_action = Some(SessionAction::RevealSessionInFinder { project_idx: p_idx, session_idx: s_idx }.into());
                        cx.notify();
                    })),
            )
            .child(
                menu_item("session-ctx-copy-path", "Copy Path", 0xcdd6f4)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.pending_action = Some(SessionAction::CopySessionPath { project_idx: p_idx, session_idx: s_idx }.into());
                        cx.notify();
                    })),
            )
            .child(separator())
            .child(
                menu_item("session-ctx-pin", pin_label, 0xcdd6f4)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.pending_action = Some(SessionAction::TogglePinSession { project_idx: p_idx, session_idx: s_idx }.into());
                        cx.notify();
                    })),
            )
            .child(
                menu_item("session-ctx-comment", "Add Comment…", 0xcdd6f4)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.pending_action = Some(SessionAction::EditSession { project_idx: p_idx, session_idx: s_idx }.into());
                        cx.notify();
                    })),
            )
            .child(separator())
            .child(
                menu_item("session-ctx-delete", "Delete", 0xf38ba8)
                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        this.session_context_menu = None;
                        this.pending_action = Some(SessionAction::RequestDiscardSession { project_idx: p_idx, session_idx: s_idx }.into());
                        cx.notify();
                    })),
            );

        root = root.child(deferred(anchored().position(position).snap_to_window().child(menu)));
        root
    }

    /// Open the edit-session modal for an existing session.
    pub(crate) fn open_edit_session_modal(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(project_idx) else { return; };
        let Some(session) = project.sessions.get(session_idx) else { return; };

        // Extract the current branch name from the clone.
        // If it's a placeholder (session-<8hex> or legacy allele/session/<id>),
        // show empty so the placeholder text appears.
        let current_branch = session
            .clone_path
            .as_ref()
            .and_then(|cp| git::current_branch(cp))
            .unwrap_or_default();
        let default_branch = git::session_branch_name(&session.id);
        let legacy_branch = git::legacy_session_branch_name(&session.id);
        let branch_slug = if current_branch == default_branch
            || current_branch == legacy_branch
            || current_branch.starts_with("allele/session/")
        {
            String::new()
        } else {
            current_branch
        };

        let entity = cx.new(|cx| {
            new_session_modal::EditSessionModal::new(
                cx,
                project_idx,
                session_idx,
                &session.label,
                &branch_slug,
                session.comment.as_deref().unwrap_or(""),
                session.pinned,
            )
        });

        cx.subscribe(
            &entity,
            |this: &mut Self, _modal, event: &new_session_modal::EditSessionModalEvent, cx| {
                match event {
                    new_session_modal::EditSessionModalEvent::Apply {
                        project_idx,
                        session_idx,
                        label,
                        branch_slug,
                        comment,
                        pinned,
                    } => {
                        this.edit_session_modal = None;
                        this.pending_action = Some(SessionAction::ApplySessionEdit {
                            project_idx: *project_idx,
                            session_idx: *session_idx,
                            label: label.clone(),
                            branch_slug: branch_slug.clone(),
                            comment: comment.clone(),
                            pinned: *pinned,
                        }.into());
                        cx.notify();
                    }
                    new_session_modal::EditSessionModalEvent::Close => {
                        this.edit_session_modal = None;
                        this.pending_action = Some(SessionAction::FocusActive.into());
                        cx.notify();
                    }
                }
            },
        )
        .detach();

        let fh = entity.read(cx).focus_handle().clone();
        self.edit_session_modal = Some(entity);
        fh.focus(window, cx);
        cx.notify();
    }

    /// Reveal a path in macOS Finder. For files, Finder selects the file
    /// inside its containing folder; for directories, it opens them.
    pub(crate) fn reveal_in_finder(path: &std::path::Path) {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }

    /// Spawn the user-configured external editor with `path` as an argument.
    /// Defaults to Sublime Text's `subl` CLI when no override is set.
    pub(crate) fn open_in_external_editor(&self, path: &std::path::Path) {
        let cmd = self
            .user_settings
            .external_editor_command
            .as_deref()
            .unwrap_or(settings::DEFAULT_EXTERNAL_EDITOR);
        settings::spawn_external_editor(cmd, path, None);
    }

    /// Private to `checkpoint_persistence()`. External callers must use
    /// `mark_settings_dirty()` — see ARCHITECTURE.md §4.4.
    pub(crate) fn save_settings(&self) {
        // Start from the live user_settings so attention preferences
        // (sound/notification opt-ins) are preserved on every write, then
        // override only the fields that the AppState is the source of truth
        // for (sidebar width, project list, etc.).
        let settings = Settings {
            sidebar_visible: self.sidebar.visible,
            sidebar_width: self.sidebar.width,
            window_x: None,
            window_y: None,
            window_width: None,
            window_height: None,
            projects: self.projects.iter().map(|p| ProjectSave {
                id: p.id.clone(),
                name: p.name.clone(),
                source_path: p.source_path.clone(),
                settings: p.settings.clone(),
            }).collect(),
            drawer_height: self.drawer.height,
            drawer_visible: false,
            right_sidebar_visible: self.right_panel.visible,
            right_sidebar_width: self.right_panel.width,
            ..self.user_settings.clone()
        };
        if let Err(e) = self.repos.settings.save(&settings) {
            warn!("Failed to save settings.json: {e}");
        }
    }

    /// Persist every session across every project to `~/.allele/state.json`.
    /// Called after any mutation that creates, removes, or transitions a session.
    /// Errors are logged but not surfaced — losing a state write is survivable,
    /// the orphan sweep will clean up any mismatch on next startup.
    ///
    /// Private to `checkpoint_persistence()`. External callers must use
    /// `mark_state_dirty()` — see ARCHITECTURE.md §4.4.
    pub(crate) fn save_state(&self) {
        let mut persisted = PersistedState::default();
        for project in &self.projects {
            for session in &project.sessions {
                persisted
                    .sessions
                    .push(PersistedSession::from_session(session, &project.id));
            }
            persisted
                .archived_sessions
                .extend(project.archives.iter().cloned());
        }
        persisted.last_active_session_id = self.active.and_then(|cursor| {
            self.projects
                .get(cursor.project_idx)
                .and_then(|p| p.sessions.get(cursor.session_idx))
                .map(|s| s.id.clone())
        });
        persisted.scratch_pad_history = self.scratch_pad_history.clone();
        if let Err(e) = self.repos.state.save(&persisted) {
            warn!("Failed to save state.json: {e}");
        }
    }

    /// Open the native folder picker and queue an action to create a project.
    fn open_folder_picker(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select project folder".into()),
        });

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this: &mut Self, cx| {
                        this.pending_action = Some(ProjectAction::OpenProjectAtPath(path).into());
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// Create a new project from a source path. Does NOT auto-create a session.
    /// Returns the index of the new project.
    ///
    /// This is the sole user-triggered project-add path — rehydration from
    /// saved settings bypasses it and goes straight through `Project::new`,
    /// so the silent `git_init` below only runs on genuinely new adds.
    pub(crate) fn create_project(&mut self, source_path: PathBuf, cx: &mut Context<Self>) -> usize {
        let name = Project::name_from_path(&source_path);

        // Phase B: ensure the project is a git repo so session clones have
        // a base to anchor against. `git_init` is idempotent — a no-op on
        // existing repos — and non-fatal on failure.
        if let Err(e) = git::git_init(&source_path) {
            warn!(
                "git_init: {} failed: {e} (continuing without git integration)",
                source_path.display()
            );
        }

        let project = Project::new(name, source_path);
        self.projects.push(project);
        let idx = self.projects.len() - 1;
        self.mark_settings_dirty();
        cx.notify();
        idx
    }

    /// Open the "New session with details" modal for a project.
    pub(crate) fn open_new_session_modal(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Build the list of enabled agents with resolved paths.
        let agents: Vec<(String, String)> = self
            .user_settings
            .agents
            .iter()
            .filter(|a| a.enabled && a.path.is_some())
            .map(|a| (a.id.clone(), a.display_name.clone()))
            .collect();

        // Determine which agent index is the default for this project.
        let project_override = self
            .projects
            .get(project_idx)
            .and_then(|p| config::ProjectConfig::load(&p.source_path))
            .and_then(|c| c.agent);
        let resolved = agents::resolve(
            &self.user_settings.agents,
            self.user_settings.default_agent.as_deref(),
            project_override.as_deref(),
            None,
        );
        let default_agent_idx = resolved
            .and_then(|a| agents.iter().position(|(id, _)| id == &a.id))
            .unwrap_or(0);

        // Compute the default label that the + button would have used.
        let session_count = self
            .projects
            .get(project_idx)
            .map(|p| p.sessions.len() + p.loading_sessions.len() + 1)
            .unwrap_or(1);
        let default_label = resolved
            .map(|a| format!("{} {session_count}", a.display_name))
            .unwrap_or_else(|| format!("Shell {session_count}"));

        let entity = cx.new(|cx| {
            new_session_modal::NewSessionModal::new(
                cx,
                project_idx,
                agents,
                default_agent_idx,
                default_label,
            )
        });

        cx.subscribe(
            &entity,
            |this: &mut Self, _modal, event: &new_session_modal::NewSessionModalEvent, cx| {
                match event {
                    new_session_modal::NewSessionModalEvent::Create {
                        project_idx,
                        label,
                        branch_slug,
                        agent_id,
                        initial_prompt,
                    } => {
                        this.new_session_modal = None;
                        this.pending_action = Some(SessionAction::AddSessionWithDetails {
                            project_idx: *project_idx,
                            label: label.clone(),
                            branch_slug: branch_slug.clone(),
                            agent_id: agent_id.clone(),
                            initial_prompt: initial_prompt.clone(),
                        }.into());
                        cx.notify();
                    }
                    new_session_modal::NewSessionModalEvent::Close => {
                        this.new_session_modal = None;
                        this.pending_action = Some(SessionAction::FocusActive.into());
                        cx.notify();
                    }
                }
            },
        )
        .detach();

        let fh = entity.read(cx).focus_handle().clone();
        self.new_session_modal = Some(entity);
        fh.focus(window, cx);
        cx.notify();
    }

    /// Read `allele.json` from the session's clone path and apply it:
    /// allocate a port, pre-spawn a drawer tab per `terminals[]` entry, show
    /// the drawer, and open the preview URL in the system browser.
    ///
    /// No-op when the file is missing or malformed. Called from both
    /// `add_session_to_project` (after the clone lands) and `resume_session`
    /// (on every cold-resume), so edits to allele.json pick up naturally.
    fn apply_project_config(
        &mut self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clone_path = self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .and_then(|s| s.clone_path.clone());
        let Some(clone_path) = clone_path else { return };
        let Some(cfg) = config::ProjectConfig::load(&clone_path) else { return };

        let port = config::allocate_port();

        // Drop any pre-existing drawer tabs from a prior materialisation —
        // the config is the source of truth for this session's layout.
        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            session.drawer_tabs.clear();
            session.pending_drawer_tab_names.clear();
            session.drawer_active_tab = 0;
            session.allocated_port = port;
        }

        let startup = cfg
            .startup
            .as_ref()
            .map(|s| config::substitute(s, port, &clone_path))
            .filter(|s| !s.trim().is_empty());

        if let Some(startup_cmd) = startup {
            let clone_for_task = clone_path.clone();
            cx.spawn_in(window, async move |this, cx| {
                let status = cx
                    .background_executor()
                    .spawn(async move {
                        std::process::Command::new("sh")
                            .arg("-c")
                            .arg(&startup_cmd)
                            .current_dir(&clone_for_task)
                            .status()
                    })
                    .await;
                match status {
                    Ok(s) if !s.success() => {
                        warn!("allele: startup command exited with {s} — continuing");
                    }
                    Err(e) => {
                        warn!("allele: failed to run startup command: {e} — continuing");
                    }
                    _ => {}
                }
                let _ = this.update_in(cx, move |this: &mut Self, window, cx| {
                    this.spawn_terminals_and_preview(cursor, &cfg, port, &clone_path, window, cx);
                });
            })
            .detach();
        } else {
            self.spawn_terminals_and_preview(cursor, &cfg, port, &clone_path, window, cx);
        }
    }

    /// Spawn the drawer terminals and open the preview URL for a session
    /// whose `allele.json` has already been loaded. Split out of
    /// `apply_project_config` so it can be deferred until after an
    /// optional `startup` command has finished running.
    fn spawn_terminals_and_preview(
        &mut self,
        cursor: SessionCursor,
        cfg: &config::ProjectConfig,
        port: Option<u16>,
        clone_path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for term in &cfg.terminals {
            let substituted = config::substitute(&term.command, port, clone_path);
            // Always spawn an interactive shell (inherit default — None).
            // If a startup command was declared, push it into the PTY's
            // stdin buffer so the freshly-loaded shell reads and executes
            // it as if the user had typed it. When the command exits or is
            // interrupted (Ctrl+C), the shell is still there for the user
            // to restart or run anything else.
            self.spawn_drawer_tab(cursor, Some(term.label.clone()), None, window, cx);
            if !substituted.trim().is_empty() {
                if let Some(session) = self
                    .projects
                    .get(cursor.project_idx)
                    .and_then(|p| p.sessions.get(cursor.session_idx))
                {
                    if let Some(tab) = session.drawer_tabs.last() {
                        let mut line = substituted.into_bytes();
                        line.push(b'\n');
                        tab.view.read(cx).send_input(&line);
                    }
                }
            }
        }

        if !cfg.terminals.is_empty() {
            if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.drawer_active_tab = 0;
                session.drawer_visible = true;
            }
        }

        if let Some(preview) = &cfg.preview {
            let url = config::substitute(&preview.url, port, clone_path);
            // Always record the preview URL on the session so the Browser
            // tab visibility logic can key off it regardless of whether
            // Chrome integration is on right now.
            let tab_id = if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.browser_last_url = Some(url.clone());
                session.browser_tab_id
            } else {
                None
            };
            if self.user_settings.browser_integration_enabled {
                // Navigate an existing linked tab so allele.json edits pick
                // up on resume; if this session is active, run a full sync
                // so Chrome ends up on the right tab.
                if let Some(id) = tab_id {
                    let _ = browser::navigate_tab(id, &url);
                }
                if self.active == Some(cursor) {
                    self.sync_browser_to_active();
                }
            } else {
                // Integration off — fall back to the legacy "open in
                // default browser" behaviour so the preview URL still
                // lands somewhere useful. Routed through the Platform
                // adapter trait (phase 14 wiring); on macOS this ends
                // up as `open(1)`, on other OSes as `xdg-open`.
                self.platform.shell.open_url(&url);
            }
        }
    }

    /// Remove a project and all its sessions (deleting all clones asynchronously).
    pub(crate) fn remove_project(&mut self, project_idx: usize, _window: &mut Window, cx: &mut Context<Self>) {
        if project_idx >= self.projects.len() { return; }

        // Remove the project from the list immediately. The terminal entities
        // are dropped, which kills the PTYs.
        let project = self.projects.remove(project_idx);

        // Collect all clone paths for background deletion
        let clone_paths: Vec<PathBuf> = project
            .sessions
            .iter()
            .filter_map(|s| s.clone_path.clone())
            .collect();

        // Adjust the active cursor — if the removed project was active or
        // before the active one, shift accordingly.
        self.active = match self.active {
            Some(active) if active.project_idx == project_idx => {
                // Active was in the removed project — pick any other session
                self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                    if !p.sessions.is_empty() {
                        Some(SessionCursor { project_idx: p_idx, session_idx: 0 })
                    } else {
                        None
                    }
                })
            }
            Some(active) if active.project_idx > project_idx => {
                Some(SessionCursor {
                    project_idx: active.project_idx - 1,
                    session_idx: active.session_idx,
                })
            }
            other => other,
        };

        self.mark_settings_dirty();
        self.mark_state_dirty();
        cx.notify();

        // Spawn background cleanup for all clones — trash (rename) instead
        // of delete so this completes near-instantly. The trash purge at
        // startup handles actual deletion asynchronously.
        if !clone_paths.is_empty() {
            cx.spawn(async move |_this, cx| {
                cx.background_executor()
                    .spawn(async move {
                        for path in clone_paths {
                            if let Err(e) = clone::trash_clone(&path) {
                                warn!("Failed to trash clone at {}: {e}", path.display());
                            }
                        }
                    })
                    .await;
            })
            .detach();
        }
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Log to ~/.config/allele/crash.log
        if let Some(home) = dirs::home_dir() {
            let log_dir = home.join(".config").join("allele");
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join("crash.log");
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let location = info.location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown>".to_string());

            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic>".to_string()
            };

            let entry = format!(
                "\n=== PANIC @ {timestamp} ===\nLocation: {location}\nMessage: {payload}\n",
            );

            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut f| {
                    use std::io::Write;
                    f.write_all(entry.as_bytes())
                });

            error!("\n*** allele crashed ***");
            error!("{entry}");
            error!("Crash log: {}", log_path.display());
        }

        // Call the default hook to print the normal backtrace too
        default_hook(info);
    }));
}

/// Install the native macOS app menu ("Allele" with About + Quit, plus a
/// View menu for sidebar/drawer toggles).
///
/// Without this, a focused Allele window shows whatever menu the previously
/// focused app left on screen, and standard shortcuts like ⌘Q are no-ops.
fn install_app_menu(cx: &mut App) {
    // NOTE: the Quit action is handled per-window (see the App::on_action
    // block inside main()) so it can check for running sessions first.
    cx.on_action(|_: &About, _cx| show_about_panel());

    // All key bindings — app-wide, ComposeBar-scoped, and TextInput-scoped
    // — are declared in `assets/default-keymap.json` and registered here.
    // Users may override any binding via `~/.allele/keymap.json`.
    keymap::load(cx);

    cx.set_menus(vec![
        Menu {
            name: "Allele".into(),
            items: vec![
                MenuItem::action("About Allele", About),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit Allele", Quit),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Show/Hide Sidebar", ToggleSidebarAction),
                MenuItem::action("Show/Hide Terminal", ToggleDrawerAction),
                MenuItem::separator(),
                MenuItem::action("Open Scratch Pad", OpenScratchPadAction),
            ],
        },
    ]);
}

/// Open the standard macOS About panel, populated with app details and a
/// clickable link to the GitHub repo.
fn show_about_panel() {
    #[cfg(target_os = "macos")]
    unsafe {
        use cocoa::appkit::NSApp;
        use cocoa::base::{id, nil};
        use cocoa::foundation::NSString;
        use objc::{class, msg_send, sel, sel_impl};

        #[repr(C)]
        struct NSRange {
            location: usize,
            length: usize,
        }

        let name: id = NSString::alloc(nil).init_str("Allele");
        let version: id = NSString::alloc(nil)
            .init_str(concat!("Version ", env!("CARGO_PKG_VERSION")));
        let copyright: id = NSString::alloc(nil).init_str(
            "Claude Code session manager — APFS clone management for parallel variant workflows.",
        );

        // Credits: plain ASCII so UTF-16 offsets line up with byte offsets for
        // the NSLink range below.
        const URL: &str = "https://github.com/devergehq/allele";
        const BODY: &str = "Claude Code session manager\nAPFS clone management for parallel variant workflows.\n\n";
        let credits_text = format!("{BODY}{URL}");

        let ns_credits_str: id = NSString::alloc(nil).init_str(&credits_text);
        let credits: id = msg_send![class!(NSMutableAttributedString), alloc];
        let credits: id = msg_send![credits, initWithString: ns_credits_str];

        let url_str: id = NSString::alloc(nil).init_str(URL);
        let url: id = msg_send![class!(NSURL), URLWithString: url_str];
        let link_key: id = NSString::alloc(nil).init_str("NSLink");
        let range = NSRange {
            location: BODY.len(),
            length: URL.len(),
        };
        let _: () = msg_send![credits, addAttribute: link_key value: url range: range];

        // Load the embedded icon for the About panel
        let icon_data: &[u8] = include_bytes!("../assets/icons/allele-icon-256.png");
        let ns_icon_data: id = msg_send![class!(NSData), dataWithBytes: icon_data.as_ptr() length: icon_data.len()];
        let icon_image: id = msg_send![class!(NSImage), alloc];
        let icon_image: id = msg_send![icon_image, initWithData: ns_icon_data];

        let keys: [id; 5] = [
            NSString::alloc(nil).init_str("ApplicationName"),
            NSString::alloc(nil).init_str("ApplicationVersion"),
            NSString::alloc(nil).init_str("Copyright"),
            NSString::alloc(nil).init_str("Credits"),
            NSString::alloc(nil).init_str("ApplicationIcon"),
        ];
        let vals: [id; 5] = [name, version, copyright, credits, icon_image];
        let options: id = msg_send![
            class!(NSDictionary),
            dictionaryWithObjects: vals.as_ptr()
            forKeys: keys.as_ptr()
            count: 5usize
        ];

        let app = NSApp();
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let _: () = msg_send![app, orderFrontStandardAboutPanelWithOptions: options];
    }
}

fn main() {
    errors::init_tracing();
    install_panic_hook();

    // OS-abstraction layer. Detected once and installed into the
    // process-wide OnceLock so call sites without an AppState handle
    // (panic hooks, early-error paths) can still reach the platform
    // via platform::global(). AppState construction reads the global
    // via clone_arcs() to obtain its own owned bundle. See
    // ARCHITECTURE.md §3.2 + §4.1.
    platform::Platform::detect().install_global();

    // Hard dependency check: Allele treats git as non-optional. Fail
    // loudly before any window opens if it's missing.
    if !git::git_available() {
        const MSG: &str = "Allele requires git but none was found on PATH.\n\n\
                           Install the Xcode Command Line Tools with:\n\n    xcode-select --install";
        error!("{MSG}");
        hooks::show_fatal_dialog("Allele", MSG);
        std::process::exit(1);
    }

    // One-shot cleanup of `~/.allele/browsers/` — stale per-task Chrome
    // user-data-dirs from an earlier embedding approach. Safe to delete;
    // browser integration now lives entirely in AppleScript against the
    // user's real Chrome.
    if let Some(home) = dirs::home_dir() {
        let stale = home.join(".allele").join("browsers");
        if stale.exists() {
            let _ = std::fs::remove_dir_all(&stale);
        }
    }

    let application = Application::new();

    // macOS: clicking the dock icon while the app is hidden (window was
    // closed via the red ✕) should bring the window back.
    application.on_reopen(|cx: &mut App| {
        cx.activate(true);
    });

    application.run(move |cx: &mut App| {
        // Load bundled fonts so we have a deterministic monospace font
        // regardless of what's installed on the system.
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice()),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf").as_slice()),
            ])
            .expect("failed to load bundled fonts");

        install_app_menu(cx);

        // Load persisted settings
        let loaded_settings = Settings::load();
        info!(
            "Loaded settings: sidebar_width={}, font_size={}",
            loaded_settings.sidebar_width, loaded_settings.font_size
        );

        // Load persisted session state (may be empty on first run).
        let loaded_state = PersistedState::load();
        info!("Loaded persisted state: {} sessions", loaded_state.sessions.len());

        // Install the Allele hook receiver and settings file so every
        // claude spawn can route attention signals back into the UI. Failure
        // is non-fatal — the app still runs, it just won't get hook events.
        let hooks_settings_path: Option<PathBuf> = match hooks::install_if_missing() {
            Ok(path) => {
                info!("Installed Allele hooks at {}", path.display());
                Some(path)
            }
            Err(e) => {
                warn!("Failed to install Allele hooks: {e} (attention routing disabled)");
                None
            }
        };

        // Conservative orphan sweep + trash purge + archive ref pruning.
        // Runs on a background thread so the UI opens immediately —
        // these are pure filesystem/git operations with no UI interaction.
        // Orphan clones aren't in persisted state so the sidebar is
        // unaffected; the sweep just reclaims disk space.
        let referenced = state::referenced_clone_paths(&loaded_state);
        let project_sources: HashMap<String, PathBuf> = loaded_settings
            .projects
            .iter()
            .map(|p| (p.name.clone(), p.source_path.clone()))
            .collect();
        let project_paths_for_prune: Vec<PathBuf> = loaded_settings
            .projects
            .iter()
            .map(|p| p.source_path.clone())
            .collect();
        std::thread::spawn(move || {
            match clone::sweep_orphans(&referenced, &project_sources) {
                Ok(0) => {}
                Ok(n) => info!("Orphan sweep trashed {n} unreferenced clone(s)"),
                Err(e) => warn!("Orphan sweep failed: {e}"),
            }
            match clone::purge_trash_older_than_days(clone::TRASH_TTL_DAYS) {
                Ok(0) => {}
                Ok(n) => info!("Trash purge removed {n} expired entry/entries"),
                Err(e) => warn!("Trash purge failed: {e}"),
            }
            // Prune archive refs older than the trash TTL so they don't
            // accumulate indefinitely in canonical repos.
            for source_path in &project_paths_for_prune {
                if let Err(e) = git::prune_archive_refs(source_path, clone::TRASH_TTL_DAYS) {
                    warn!(
                        "prune_archive_refs failed for {}: {e}",
                        source_path.display()
                    );
                }
            }
        });

        // Log resolved agent paths at startup for diagnostics. Agent
        // detection is owned by the Settings seeder (runs on load).
        for agent in &loaded_settings.agents {
            match &agent.path {
                Some(p) => info!("Agent '{}' at: {p}", agent.id),
                None => warn!("Agent '{}' not found", agent.id),
            }
        }

        let window_bounds = match (
            loaded_settings.window_x,
            loaded_settings.window_y,
            loaded_settings.window_width,
            loaded_settings.window_height,
        ) {
            (Some(x), Some(y), Some(w), Some(h)) => Some(WindowBounds::Windowed(Bounds::new(
                point(px(x), px(y)),
                size(px(w), px(h)),
            ))),
            _ => None,
        };

        let settings_for_window = loaded_settings.clone();
        let loaded_state_for_window = loaded_state.clone();
        let hooks_settings_path_for_window = hooks_settings_path.clone();

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Allele".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(800.0), px(600.0))),
                window_bounds,
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx: &mut Context<AppState>| {
                    // Observe window bounds changes and persist them.
                    cx.observe_window_bounds(window, |this: &mut AppState, window, _cx| {
                        let viewport = window.viewport_size();
                        let settings = Settings {
                            sidebar_width: this.sidebar.width,
                            window_x: None,
                            window_y: None,
                            window_width: Some(f32::from(viewport.width)),
                            window_height: Some(f32::from(viewport.height)),
                            projects: this.projects.iter().map(|p| ProjectSave {
                                id: p.id.clone(),
                                name: p.name.clone(),
                                source_path: p.source_path.clone(),
                                settings: p.settings.clone(),
                            }).collect(),
                            ..this.user_settings.clone()
                        };
                        if let Err(e) = this.repos.settings.save(&settings) {
                            warn!("Failed to save settings.json on window-bounds change: {e}");
                        }
                    }).detach();

                    // Rehydrate projects from settings.
                    let mut projects: Vec<Project> = settings_for_window.projects.iter().map(|p| {
                        let mut proj = Project::new(p.name.clone(), p.source_path.clone());
                        proj.id = p.id.clone();
                        proj.settings = p.settings.clone();
                        proj
                    }).collect();

                    // Rehydrate archived sessions from state.json so the
                    // archive browser shows human-readable labels.
                    for archived in &loaded_state_for_window.archived_sessions {
                        if let Some(project) = projects.iter_mut().find(|p| p.id == archived.project_id) {
                            project.archives.push(archived.clone());
                        }
                    }

                    // Reconcile: any git archive refs without a state.json
                    // entry (e.g., sessions archived before this change
                    // landed) get a synthetic entry with the session ID as
                    // the label so they still appear in the browser.
                    for project in &mut projects {
                        let known_ids: std::collections::HashSet<String> =
                            project.archives.iter().map(|a| a.id.clone()).collect();
                        if let Ok(git_entries) = git::list_archive_refs(&project.source_path) {
                            for entry in git_entries {
                                if !known_ids.contains(&entry.session_id) {
                                    project.archives.push(ArchivedSession {
                                        id: entry.session_id.clone(),
                                        project_id: project.id.clone(),
                                        label: format!("Session {}", &entry.session_id[..8.min(entry.session_id.len())]),
                                        archived_at: entry.timestamp,
                                    });
                                }
                            }
                        }
                    }

                    // Rehydrate sessions from state.json as Suspended entries
                    // (no PTY, ⏸ icon). They show up in the sidebar immediately
                    // and cold-resume on click via `claude --resume <id>`.
                    // Sessions whose owning project no longer exists are
                    // silently dropped — on the next save_state the entries
                    // will be removed from disk too.
                    for persisted in &loaded_state_for_window.sessions {
                        let Some(project) = projects
                            .iter_mut()
                            .find(|p| p.id == persisted.project_id)
                        else {
                            warn!(
                                "Dropping persisted session {} — owning project {} is gone",
                                persisted.id, persisted.project_id
                            );
                            continue;
                        };

                        let mut session = Session::suspended_from_persisted(
                            persisted.id.clone(),
                            persisted.label.clone(),
                            persisted.started_at,
                            persisted.last_active,
                            persisted.clone_path.clone(),
                            persisted.merged,
                        )
                        .with_drawer_tabs(
                            persisted.drawer_tab_names.clone(),
                            persisted.drawer_active_tab,
                        )
                        .with_browser(
                            persisted.browser_tab_id,
                            persisted.browser_last_url.clone(),
                        )
                        .with_agent_id(persisted.agent_id.clone());
                        session.pinned = persisted.pinned;
                        session.comment = persisted.comment.clone();
                        session.branch_name = persisted.branch_name.clone();
                        project.sessions.push(session);
                    }

                    // Spawn the hook-event polling task. Runs for the life
                    // of the app, reads ~/.allele/events/*.jsonl every
                    // 250ms, and routes each new event into apply_hook_event.
                    //
                    // Fast-forward existing files so we don't flood the user
                    // with pre-existing events from a previous app session.
                    cx.spawn(async move |this, cx| {
                        let mut watcher = hooks::EventWatcher::new();
                        watcher.initialize_offsets();

                        loop {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(250))
                                .await;

                            let events = watcher.poll();
                            if events.is_empty() {
                                continue;
                            }

                            if this
                                .update(cx, |this: &mut AppState, cx| {
                                    for event in events {
                                        this.apply_hook_event(event, cx);
                                    }
                                })
                                .is_err()
                            {
                                break; // AppState dropped — app is exiting
                            }
                        }
                    })
                    .detach();

                    // Rich Sidecar transcript tailer poll. Runs on a much
                    // gentler cadence than the old 120ms spinner timer so
                    // it doesn't drive full AppState re-renders that trigger
                    // spurious terminal resizes and destroy scrollback.
                    cx.spawn(async move |this, cx| {
                        loop {
                            cx.background_executor()
                                .timer(std::time::Duration::from_millis(500))
                                .await;

                            if this
                                .update(cx, |state: &mut AppState, cx| {
                                    // Drain the transcript tailer (if built) and
                                    // feed events into the RichView. Runs
                                    // regardless of which main tab is visible so
                                    // the document stays current when the user
                                    // flips to Transcript. No cx.notify() here —
                                    // the RichView entity updates itself.
                                    state.poll_transcript_tailer(cx);
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .detach();
                    // App-level handlers for menu-dispatched actions. Registering
                    // at App scope (not on the element tree) guarantees the
                    // menu items stay enabled regardless of focus state.
                    let toggle_handle = cx.entity().downgrade();

                    // Quit interception — confirm before quitting when
                    // sessions are still running.
                    App::on_action::<Quit>(cx, {
                        let handle = toggle_handle.clone();
                        move |_, cx| {
                            let should_quit = handle
                                .update(cx, |state: &mut AppState, cx| {
                                    let active_count = state
                                        .projects
                                        .iter()
                                        .flat_map(|p| &p.sessions)
                                        .filter(|s| {
                                            matches!(
                                                s.status,
                                                SessionStatus::Running | SessionStatus::Idle
                                            )
                                        })
                                        .count();
                                    if active_count > 0 {
                                        state.confirming.quit = true;
                                        cx.notify();
                                        false
                                    } else {
                                        true
                                    }
                                })
                                .unwrap_or(true);
                            if should_quit {
                                cx.quit();
                            }
                        }
                    });
                    App::on_action::<ToggleSidebarAction>(cx, {
                        let handle = toggle_handle.clone();
                        move |_, cx| {
                            handle
                                .update(cx, |this: &mut AppState, cx| {
                                    this.pending_action = Some(SidebarAction::ToggleSidebar.into());
                                    cx.notify();
                                })
                                .ok();
                        }
                    });
                    App::on_action::<ToggleDrawerAction>(cx, {
                        let handle = toggle_handle.clone();
                        move |_, cx| {
                            handle
                                .update(cx, |this: &mut AppState, cx| {
                                    this.pending_action = Some(DrawerAction::ToggleDrawer.into());
                                    cx.notify();
                                })
                                .ok();
                        }
                    });
                    App::on_action::<OpenScratchPadAction>(cx, {
                        let handle = toggle_handle.clone();
                        move |_, cx| {
                            handle
                                .update(cx, |this: &mut AppState, cx| {
                                    this.pending_action = Some(OverlayAction::OpenScratchPad.into());
                                    cx.notify();
                                })
                                .ok();
                        }
                    });
                    App::on_action::<ToggleTranscriptTabAction>(cx, {
                        let handle = toggle_handle.clone();
                        move |_, cx| {
                            handle
                                .update(cx, |this: &mut AppState, cx| {
                                    this.main_tab = match this.main_tab {
                                        MainTab::Transcript => MainTab::Claude,
                                        _ => MainTab::Transcript,
                                    };
                                    cx.notify();
                                })
                                .ok();
                        }
                    });
                    App::on_action::<OpenSettings>(cx, {
                        let handle = toggle_handle.clone();
                        move |_, cx| {
                            // Must happen here (not via PendingAction) — the
                            // pending-action dispatch runs inside render(),
                            // and cx.open_window() during a render tears
                            // GPUI's element arena apart with
                            // "attempted to dereference an ArenaRef after
                            // its Arena was cleared".
                            let Some(strong) = handle.upgrade() else { return };
                            let (existing, paths, external_editor, browser_integration, agents_list, default_agent, font_size, git_pull_before_new_session, promote_attention_sessions) = strong.update(cx, |state: &mut AppState, _cx| {
                                (
                                    state.settings_window,
                                    state.user_settings.session_cleanup_paths.clone(),
                                    state
                                        .user_settings
                                        .external_editor_command
                                        .clone()
                                        .unwrap_or_default(),
                                    state.user_settings.browser_integration_enabled,
                                    state.user_settings.agents.clone(),
                                    state.user_settings.default_agent.clone(),
                                    state.user_settings.font_size,
                                    state.user_settings.git_pull_before_new_session,
                                    state.user_settings.promote_attention_sessions,
                                )
                            });

                            if let Some(win) = existing {
                                if win
                                    .update(cx, |_state, window, _cx| {
                                        window.activate_window();
                                    })
                                    .is_ok()
                                {
                                    return;
                                }
                            }

                            let weak = handle.clone();
                            match settings_window::open_settings_window(cx, weak, paths, external_editor, browser_integration, agents_list, default_agent, font_size, git_pull_before_new_session, promote_attention_sessions) {
                                Ok(new_handle) => {
                                    strong
                                        .update(cx, |state: &mut AppState, _cx| {
                                            state.settings_window = Some(new_handle);
                                        });
                                }
                                Err(e) => {
                                    warn!("Failed to open settings window: {e}");
                                }
                            }
                        }
                    });

                    // macOS convention: the red ✕ hides the window rather
                    // than quitting the app. Clicking the dock icon will
                    // reactivate it (see on_reopen below).
                    window.on_window_should_close(cx, move |_window, cx| {
                        cx.hide();
                        false // never actually close the window
                    });

                    // Locate the session to auto-resume on launch. We look up
                    // `last_active_session_id` from the loaded state and, if
                    // its clone path is still on disk, pre-select it + queue
                    // a ResumeSession so the first render tick spawns the
                    // resumed PTY. If the clone is gone (user deleted it
                    // externally), fall back to no auto-selection.
                    let (initial_active, initial_pending) = loaded_state_for_window
                        .last_active_session_id
                        .as_deref()
                        .and_then(|target_id| {
                            for (p_idx, project) in projects.iter().enumerate() {
                                for (s_idx, session) in project.sessions.iter().enumerate() {
                                    if session.id == target_id {
                                        let resumable = session
                                            .clone_path
                                            .as_ref()
                                            .map(|p| p.exists())
                                            .unwrap_or(false);
                                        let cursor = SessionCursor {
                                            project_idx: p_idx,
                                            session_idx: s_idx,
                                        };
                                        let pending = if resumable {
                                            Some(SessionAction::ResumeSession {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            }.into())
                                        } else {
                                            None
                                        };
                                        return Some((Some(cursor), pending));
                                    }
                                }
                            }
                            None
                        })
                        .unwrap_or((None, None));

                    let sidebar_filter_input = cx.new(|cx| {
                        text_input::TextInput::new(cx, "", "Search sessions…")
                    });
                    cx.subscribe(&sidebar_filter_input, |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                        if matches!(event, text_input::TextInputEvent::Changed) {
                            this.sidebar_filter = input.read(cx).text().to_lowercase();
                            cx.notify();
                        }
                    }).detach();

                    AppState {
                        projects,
                        active: initial_active,
                        pending_action: initial_pending,
                        sidebar: SidebarState {
                            visible: settings_for_window.sidebar_visible,
                            width: settings_for_window.sidebar_width
                                .max(SIDEBAR_MIN_WIDTH),
                            resizing: false,
                        },
                        right_panel: RightPanelState {
                            visible: settings_for_window.right_sidebar_visible,
                            width: settings_for_window.right_sidebar_width
                                .max(RIGHT_SIDEBAR_MIN_WIDTH),
                            resizing: false,
                        },
                        drawer: DrawerState {
                            height: settings_for_window.drawer_height
                                .max(DRAWER_MIN_HEIGHT),
                            resizing: false,
                            rename: None,
                            rename_focus: None,
                        },
                        editor: EditorState {
                            selected_path: None,
                            expanded_dirs: HashSet::new(),
                            preview: None,
                            context_menu: None,
                        },
                        confirming: ConfirmationState {
                            discard: None,
                            dirty_session: None,
                            quit: false,
                        },
                        rich: RichState {
                            view: None,
                            transcript_tailer: None,
                            cursor: None,
                        },
                        hooks_settings_path: hooks_settings_path_for_window,
                        editing_project_settings: None,
                        user_settings: settings_for_window.clone(),
                        settings_window: None,
                        pull_warning: None,
                        main_tab: MainTab::Claude,
                        browser_status: String::new(),
                        scratch_pad: None,
                        scratch_pad_history: loaded_state.scratch_pad_history.clone(),
                        new_session_modal: None,
                        session_context_menu: None,
                        edit_session_modal: None,
                        sidebar_filter_input,
                        sidebar_filter: String::new(),
                        state_dirty: false,
                        settings_dirty: false,
                        repos: repositories::Repositories::production(),
                        platform: crate::platform::global().clone_arcs(),
                    }
                })
            },
        )
        .expect("open main window");
    });
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process pending actions — dispatcher lives in src/pending_actions.rs.
        self.dispatch_pending_action(window, cx);

        // If the user is on the Browser tab but it's no longer eligible
        // (flag turned off, switched to a session without a preview URL,
        // or project config lost the preview entry), fall back to Claude
        // so the main pane keeps showing something useful.
        if self.main_tab == MainTab::Browser && !self.browser_tab_available() {
            self.main_tab = MainTab::Claude;
        }

        // Update session statuses from PTY state.
        // Any attached session (Running, Idle, AwaitingInput, ResponseReady)
        // can transition to Done when its PTY actually exits. Done and
        // Suspended sessions are already terminal/attached-less and are
        // skipped.
        let mut pty_state_dirty = false;
        let now = std::time::Instant::now();
        for project in &mut self.projects {
            for session in &mut project.sessions {
                if matches!(
                    session.status,
                    SessionStatus::Done | SessionStatus::Suspended
                ) {
                    continue;
                }
                let Some(tv) = session.terminal_view.as_ref() else { continue; };
                if tv.read(cx).has_exited() {
                    // If we're still inside the resume grace window, treat
                    // this as a resume failure — revert to Suspended and
                    // drop the PTY so the user can try again or the UI can
                    // prompt them — rather than silently locking them into
                    // the "Session ended" overlay.
                    let resume_failed = session
                        .resuming_until
                        .map(|deadline| now < deadline)
                        .unwrap_or(false);
                    if resume_failed {
                        warn!(
                            "Resume failed for session {} — PTY exited inside grace window",
                            session.id
                        );
                        session.terminal_view = None;
                        session.status = SessionStatus::Suspended;
                    } else {
                        warn!(
                            "PTY exited for session {} ({}) — marking Done",
                            session.id, session.label
                        );
                        session.status = SessionStatus::Done;
                    }
                    session.last_active = std::time::SystemTime::now();
                    session.resuming_until = None;
                    pty_state_dirty = true;
                } else if let Some(deadline) = session.resuming_until {
                    if now >= deadline {
                        session.resuming_until = None;
                    }
                }
            }
        }
        if pty_state_dirty {
            self.mark_state_dirty();
        }

        // Build sidebar items: for each project, a header then its sessions
        let sidebar_items = crate::sidebar::render::build_sidebar_items(self, window, cx);

        // Status summary
        let total_projects = self.projects.len();
        let total_sessions: usize = self.projects.iter().map(|p| p.sessions.len()).sum();
        let running: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::Running)
            .count();
        let awaiting: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::AwaitingInput)
            .count();
        let response_ready: usize = self.projects.iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::ResponseReady)
            .count();

        let fps = self.active_session()
            .and_then(|s| s.terminal_view.as_ref())
            .map(|tv| tv.read(cx).current_fps)
            .unwrap_or(0);

        let active_is_done = self.active_session()
            .map(|s| s.status == SessionStatus::Done)
            .unwrap_or(false);

        // Can the currently-Done session be revived with its prior conversation?
        // Needs both the clone directory still on disk *and* Claude's history
        // jsonl for this session id. When true, the "Session ended" bar shows
        // a primary "Resume" button; otherwise it falls back to "New Session".
        let active_is_resumable = self
            .active_session()
            .map(|s| {
                s.clone_path
                    .as_ref()
                    .map(|p| p.exists())
                    .unwrap_or(false)
                    && claude_session_history_exists(&s.id)
            })
            .unwrap_or(false);

        let sidebar_w = self.sidebar.width;
        let sidebar_visible = self.sidebar.visible;
        let is_resizing = self.sidebar.resizing;
        let drawer_is_resizing = self.drawer.resizing;
        let drawer_visible = self.active_session()
            .map(|s| s.drawer_visible)
            .unwrap_or(false);
        let right_sidebar_visible = self.right_panel.visible;
        let right_sidebar_w = self.right_panel.width;
        let right_sidebar_resizing = self.right_panel.resizing;

        // Outer non-flex container that hosts the flex row AND the drag overlay.
        // Keeping the overlay OUTSIDE the flex container ensures Taffy's layout
        // engine doesn't try to allocate flex space to an absolutely-positioned element.
        let mut flex_row = div()
            .id("app-root")
            .flex()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4));

        // --- Left sidebar (conditional on sidebar_visible) ---
        if sidebar_visible {
            flex_row = flex_row.child(
                // Sidebar
                div()
                    .w(px(sidebar_w))
                    .flex_shrink_0()
                    .h_full()
                    .bg(rgb(0x181825))
                    .border_r_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_col()
                    // Header
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(10.0))
                            .border_b_1()
                            .border_color(rgb(0x313244))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Allele"),
                            )
                            .child(
                                // "Open project" button
                                div()
                                    .id("new-project-btn")
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .text_size(px(16.0))
                                    .text_color(rgb(0x6c7086))
                                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xa6e3a1)))
                                    .child("+")
                                    .tooltip(|_window, cx| {
                                        cx.new(|_| SimpleTooltip { text: "Open project".into() }).into()
                                    })
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        this.open_folder_picker(cx);
                                    })),
                            ),
                    )
                    // Search filter
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(4.0))
                            .border_b_1()
                            .border_color(rgb(0x313244))
                            .child(
                                div()
                                    .w_full()
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .rounded(px(4.0))
                                    .bg(rgb(0x11111b))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0xcdd6f4))
                                    .overflow_hidden()
                                    .child(self.sidebar_filter_input.clone()),
                            ),
                    )
                    // Session list
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .children(sidebar_items),
                    )
                    // Status bar — attention summary lives here.
                    .child({
                        let mut bar = div()
                            .px(px(12.0))
                            .py(px(8.0))
                            .border_t_1()
                            .border_color(rgb(0x313244))
                            .text_size(px(10.0))
                            .text_color(rgb(0x6c7086))
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .items_center()
                            .child(format!(
                                "{total_projects}p · {total_sessions}s · {running} running · {fps} fps"
                            ));

                        if awaiting > 0 {
                            bar = bar.child(
                                div()
                                    .text_color(rgb(SessionStatus::AwaitingInput.color()))
                                    .child(format!("⚠ {awaiting} need input")),
                            );
                        }
                        if response_ready > 0 {
                            bar = bar.child(
                                div()
                                    .text_color(rgb(SessionStatus::ResponseReady.color()))
                                    .child(format!("★ {response_ready} ready")),
                            );
                        }
                        bar
                    }),
            );
            // Resize handle — 6px wide invisible hover zone over the sidebar border.
            flex_row = flex_row.child(
                div()
                    .id("sidebar-resize-handle")
                    .w(px(6.0))
                    .h_full()
                    .cursor_col_resize()
                    .hover(|s| s.bg(rgb(0x45475a)))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                        this.sidebar.resizing = true;
                        cx.notify();
                    })),
            );
        }

        flex_row = flex_row.child({
                // Right-hand content column: main terminal + optional drawer
                let mut content_col = div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .h_full()
                    .flex()
                    .flex_col();

                // --- Main-area tab strip: Claude / Editor ---
                content_col = content_col.child(self.render_main_tab_strip(cx));

                // --- Main terminal area (flex_1, takes remaining space) ---
                {
                    let mut main_area = div()
                        .flex_1()
                        .min_h(px(100.0))
                        .overflow_hidden()
                        .relative();

                    match self.main_tab {
                        MainTab::Claude => {
                            main_area = main_area.pt(px(6.0));
                            if let Some(tv) = self.active_session().and_then(|s| s.terminal_view.clone()) {
                                // Tell the main terminal how much space the drawer
                                // reserves below it so the PTY resize is correct.
                                let inset = if drawer_visible {
                                    // 6px resize handle + ~30px header + drawer panel
                                    6.0 + 30.0 + self.drawer.height
                                } else {
                                    0.0
                                };
                                tv.update(cx, |tv, _cx| {
                                    tv.bottom_inset = inset;
                                });
                                main_area = main_area.child(tv);
                            } else {
                                main_area = main_area.child(
                                    div()
                                        .size_full()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .justify_center()
                                        .gap(px(16.0))
                                        .bg(rgb(0x1e1e2e))
                                        .child(
                                            div()
                                                .text_size(px(16.0))
                                                .text_color(rgb(0x6c7086))
                                                .child("No active session"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(rgb(0x45475a))
                                                .child("Click + in the sidebar to open a project"),
                                        ),
                                );
                            }
                        }
                        MainTab::Editor => {
                            main_area = main_area.child(self.render_editor_view(cx));
                        }
                        MainTab::Browser => {
                            main_area = main_area.child(self.render_browser_placeholder(cx));
                        }
                        MainTab::Transcript => {
                            main_area = main_area.child(self.render_transcript_view(cx));
                        }
                    }

                    if active_is_done {
                        let mut buttons = div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0));

                        if active_is_resumable {
                            buttons = buttons.child(
                                div()
                                    .id("resume-btn")
                                    .cursor_pointer()
                                    .px(px(10.0))
                                    .py(px(4.0))
                                    .rounded(px(4.0))
                                    .bg(rgb(0x89b4fa))
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x1e1e2e))
                                    .hover(|s| s.bg(rgb(0x74c7ec)))
                                    .child("Resume")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        if let Some(active) = this.active {
                                            this.pending_action = Some(SessionAction::ResumeSession {
                                                project_idx: active.project_idx,
                                                session_idx: active.session_idx,
                                            }.into());
                                            cx.notify();
                                        }
                                    })),
                            );
                        }

                        buttons = buttons.child(
                            div()
                                .id("restart-btn")
                                .cursor_pointer()
                                .px(px(10.0))
                                .py(px(4.0))
                                .rounded(px(4.0))
                                .bg(rgb(0x45475a))
                                .text_size(px(11.0))
                                .text_color(rgb(0xcdd6f4))
                                .hover(|s| s.bg(rgb(0x585b70)))
                                .child("New Session")
                                .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                    if let Some(active) = this.active {
                                        this.pending_action = Some(SessionAction::AddSessionToProject(active.project_idx).into());
                                        cx.notify();
                                    }
                                })),
                        );

                        main_area = main_area.child(
                            // "Session ended" overlay bar at bottom
                            div()
                                .absolute()
                                .bottom(px(0.0))
                                .left(px(0.0))
                                .right(px(0.0))
                                .px(px(16.0))
                                .py(px(10.0))
                                .bg(rgb(0x313244))
                                .border_t_1()
                                .border_color(rgb(0x45475a))
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(0x6c7086))
                                        .child("Session ended"),
                                )
                                .child(buttons),
                        );
                    }

                    // --- Quit confirmation banner (absolute overlay at top) ---
                    if self.confirming.quit {
                        let active_count = self
                            .projects
                            .iter()
                            .flat_map(|p| &p.sessions)
                            .filter(|s| {
                                matches!(
                                    s.status,
                                    SessionStatus::Running | SessionStatus::Idle
                                )
                            })
                            .count();
                        let label = if active_count == 1 {
                            "1 session is still running — quit anyway?".to_string()
                        } else {
                            format!("{active_count} sessions are still running — quit anyway?")
                        };
                        main_area = main_area.child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .right(px(0.0))
                                .px(px(16.0))
                                .py(px(10.0))
                                .bg(rgb(0x3b1e1e)) // subtle red tint
                                .border_b_1()
                                .border_color(rgb(0xf38ba8))
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .text_color(rgb(0xf38ba8)) // red
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .id("quit-confirm-btn")
                                                .cursor_pointer()
                                                .px(px(10.0))
                                                .py(px(4.0))
                                                .rounded(px(4.0))
                                                .bg(rgb(0xf38ba8))
                                                .text_size(px(11.0))
                                                .text_color(rgb(0x1e1e2e))
                                                .hover(|s| s.bg(rgb(0xeba0ac)))
                                                .child("Quit")
                                                .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                                    this.confirming.quit = false;
                                                    cx.quit();
                                                })),
                                        )
                                        .child(
                                            div()
                                                .id("quit-cancel-btn")
                                                .cursor_pointer()
                                                .px(px(10.0))
                                                .py(px(4.0))
                                                .rounded(px(4.0))
                                                .bg(rgb(0x45475a))
                                                .text_size(px(11.0))
                                                .text_color(rgb(0xcdd6f4))
                                                .hover(|s| s.bg(rgb(0x585b70)))
                                                .child("Cancel")
                                                .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                                    this.confirming.quit = false;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        );
                    }

                    // --- Pull warning banner (absolute overlay at top) ---
                    if let Some(ref warning) = self.pull_warning {
                        let label = format!("git pull failed: {warning}");
                        main_area = main_area.child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .left(px(0.0))
                                .right(px(0.0))
                                .px(px(16.0))
                                .py(px(10.0))
                                .bg(rgb(0x2e2a1e)) // subtle amber tint
                                .border_b_1()
                                .border_color(rgb(0xf9e2af)) // yellow
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .text_color(rgb(0xf9e2af)) // yellow
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .id("pull-warning-dismiss-btn")
                                        .cursor_pointer()
                                        .px(px(10.0))
                                        .py(px(4.0))
                                        .rounded(px(4.0))
                                        .bg(rgb(0x45475a))
                                        .text_size(px(11.0))
                                        .text_color(rgb(0xcdd6f4))
                                        .hover(|s| s.bg(rgb(0x585b70)))
                                        .child("Dismiss")
                                        .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                            this.pull_warning = None;
                                            cx.notify();
                                        })),
                                ),
                        );
                    }

                    content_col = content_col.child(main_area);
                }

                // --- Drawer terminal (fixed height, shown per-session) ---
                if drawer_visible {
                    content_col =
                        content_col.children(crate::drawer::build_drawer_items(self, window, cx));
                }

                content_col
            });

        // --- Right sidebar (conditional on right_sidebar_visible) ---
        if right_sidebar_visible {
            // Resize handle — 6px wide on left edge of right sidebar
            flex_row = flex_row.child(
                div()
                    .id("right-sidebar-resize-handle")
                    .w(px(6.0))
                    .h_full()
                    .cursor_col_resize()
                    .hover(|s| s.bg(rgb(0x45475a)))
                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                        this.right_panel.resizing = true;
                        cx.notify();
                    })),
            );
            flex_row = flex_row.child(
                div()
                    .w(px(right_sidebar_w))
                    .flex_shrink_0()
                    .h_full()
                    .bg(rgb(0x181825))
                    .border_l_1()
                    .border_color(rgb(0x313244))
                    .flex()
                    .flex_col()
                    // Header
                    .child(
                        div()
                            .px(px(12.0))
                            .py(px(10.0))
                            .border_b_1()
                            .border_color(rgb(0x313244))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Inspector"),
                            )
                            .child(
                                div()
                                    .id("right-sidebar-close-btn")
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .text_size(px(14.0))
                                    .text_color(rgb(0x6c7086))
                                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                                    .child("×")
                                    .tooltip(|_window, cx| {
                                        cx.new(|_| SimpleTooltip { text: "Close inspector".into() }).into()
                                    })
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        this.pending_action = Some(SidebarAction::ToggleRightSidebar.into());
                                        cx.notify();
                                    })),
                            ),
                    )
                    // Body placeholder
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.0))
                            .text_color(rgb(0x45475a))
                            .child("No inspector content"),
                    ),
            );
        }

        // Outer wrapper: non-flex, relative-positioned container hosting both
        // the flex row and the optional drag overlay as siblings.
        let mut outer = div()
            .id("app-outer")
            .size_full()
            .relative()
            .child(flex_row);

        // Sidebar drag overlay
        if is_resizing {
            outer = outer.child(
                div()
                    .id("sidebar-drag-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor_col_resize()
                    .on_mouse_move(cx.listener(|this: &mut Self, event: &MouseMoveEvent, window, cx| {
                        let viewport_w = f32::from(window.viewport_size().width);
                        let max = (viewport_w - 100.0).max(SIDEBAR_MIN_WIDTH);
                        let new_width = f32::from(event.position.x).clamp(SIDEBAR_MIN_WIDTH, max);
                        if (new_width - this.sidebar.width).abs() > 0.5 {
                            this.sidebar.width = new_width;
                            window.refresh();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                        this.sidebar.resizing = false;
                        this.mark_settings_dirty();
                        cx.notify();
                    })),
            );
        }

        // Right sidebar drag overlay
        if right_sidebar_resizing {
            outer = outer.child(
                div()
                    .id("right-sidebar-drag-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor_col_resize()
                    .on_mouse_move(cx.listener(|this: &mut Self, event: &MouseMoveEvent, window, cx| {
                        let viewport_w = f32::from(window.viewport_size().width);
                        let mouse_x = f32::from(event.position.x);
                        // Right sidebar width = distance from right edge to mouse
                        let new_width = (viewport_w - mouse_x).clamp(RIGHT_SIDEBAR_MIN_WIDTH, viewport_w - 200.0);
                        if (new_width - this.right_panel.width).abs() > 0.5 {
                            this.right_panel.width = new_width;
                            window.refresh();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                        this.right_panel.resizing = false;
                        this.mark_settings_dirty();
                        cx.notify();
                    })),
            );
        }

        // Drawer drag overlay
        if drawer_is_resizing {
            outer = outer.child(crate::drawer::build_drawer_drag_overlay(self, window, cx));
        }

        if let Some(pad) = self.scratch_pad.clone() {
            outer = outer.child(pad);
        }

        if let Some(modal) = self.new_session_modal.clone() {
            outer = outer.child(modal);
        }

        if let Some(modal) = self.edit_session_modal.clone() {
            outer = outer.child(modal);
        }

        outer = outer.child(self.render_session_context_menu(cx));

        // Coalesce per-frame mutations into at most one write per file.
        // See ARCHITECTURE.md §3.4.
        self.checkpoint_persistence();

        outer
    }
}
