mod accessibility;
mod actions;
mod agents;
mod app_state;
mod assets;
mod attention_bar;
mod base_infra;
mod browser;
mod changes;
mod cli;
mod clone;
mod config;
mod conversation_picker;
mod conversations;
mod debug_capture;
mod dispatch;
mod drawer;
mod errors;
mod fd_limit;
mod git;
mod hook_events;
mod hooks;
mod icon;
mod interrupted;
mod keymap;
mod mac_menu;
mod memory_watchdog;
mod naming;
mod new_session_modal;
mod paths;
mod pending_actions;
mod platform;
mod project;
mod reader;
mod remote_browser;
mod render;
mod repositories;
mod rich;
mod sandbox;
mod scratch_pad;
mod session;
mod session_ops;
mod settings;
mod settings_window;
mod shell_env;
mod sidebar;
mod startup;
mod state;
mod stream;
mod sync;
mod terminal;
mod text_input;
mod theme;
mod transcript;
mod trust;

use crate::theme::theme;
use actions::{ProjectAction, SessionAction, SessionCursor, SettingsAction};
use app_state::{
    AppState, ChangesPanelState, ConfirmationState, DrawerState, MainTab, ReaderState, RichState,
    RightPanelState, SidebarState, StructuralStamp, DRAWER_MIN_HEIGHT, RIGHT_SIDEBAR_MIN_WIDTH,
    SIDEBAR_MIN_WIDTH,
};
use gpui::*;
use project::Project;
actions!(
    allele,
    [
        About,
        Quit,
        ToggleSidebarAction,
        ToggleActiveOnlyAction,
        ToggleDrawerAction,
        OpenSettings,
        OpenScratchPadAction,
        ToggleTranscriptTabAction,
        CycleAttentionSession,
        CaptureUi,
        OpenFilePaletteAction,
        OpenSearchAction,
        OpenCommandPaletteAction
    ]
);
use session::{Session, SessionStatus};
use settings::{ProjectSave, Settings};
use state::{ArchivedSession, PersistedSession, PersistedState};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

/// Ghost chip rendered under the cursor while dragging a sidebar row.
pub(crate) struct DragPreview(pub String);

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(theme().bg_raised)
            .border_1()
            .border_color(theme().border_default)
            .shadow_lg()
            .text_size(px(12.0))
            .text_color(theme().text_primary)
            .child(self.0.clone())
    }
}

/// What the session summary header's single action button does when clicked.
///
/// Kept as a value rather than a closure so the label and the behaviour are
/// decided together in one `match` — the header previously rendered an
/// accent-coloured string that looked like a button and did nothing.
#[derive(Clone, Copy)]
enum HeaderAction {
    GoToTerminal,
    GoToTranscript,
    ReviewChanges,
    Resume,
}

/// A minimal tooltip view for hover text on buttons.
pub(crate) struct SimpleTooltip {
    pub(crate) text: SharedString,
}

impl Render for SimpleTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .bg(theme().bg_base)
            .border_1()
            .border_color(theme().border_default)
            .text_size(px(11.0))
            .text_color(theme().text_primary)
            .child(self.text.clone())
    }
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

    /// The single header action, and what clicking it does.
    ///
    /// Every variant except `Resume` is pure navigation — it moves the user to
    /// the place where they act, and nothing else. `Resume` is the one
    /// side-effectful case (it spawns a PTY and runs the project's startup
    /// command), so it is only ever offered behind the same resumability gate
    /// the "Session ended" overlay uses.
    fn header_action(
        status: SessionStatus,
        dirty: bool,
        resumable: bool,
    ) -> (&'static str, HeaderAction) {
        match status {
            SessionStatus::AwaitingInput => ("Answer in terminal", HeaderAction::GoToTerminal),
            SessionStatus::ResponseReady => ("Review transcript", HeaderAction::GoToTranscript),
            SessionStatus::Done | SessionStatus::Suspended if resumable => {
                ("Resume session", HeaderAction::Resume)
            }
            // Not resumable: send the user to the tab that owns the
            // Resume/Restart overlay rather than offering a dead action.
            SessionStatus::Done | SessionStatus::Suspended => {
                ("Open session", HeaderAction::GoToTerminal)
            }
            _ if dirty => ("Review changes", HeaderAction::ReviewChanges),
            _ => ("Open terminal", HeaderAction::GoToTerminal),
        }
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
                |this: &mut Self, _pad, event: &scratch_pad::ScratchPadEvent, cx| match event {
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
    ///
    /// Delivered through [`crate::dispatch::pty::deliver`], which is what
    /// actually matches the clipboard path in `terminal_view.rs`: it checks
    /// `TermMode::BRACKETED_PASTE` before emitting the markers. This comment
    /// used to claim the logic was mirrored here while omitting that check —
    /// a doc asserting a mirror that is not there tells the next reader not
    /// to look (DEV-603).
    fn scratch_pad_send(
        &mut self,
        text: String,
        attachments: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.active_session() else {
            return;
        };
        let Some(tv) = session.terminal_view.clone() else {
            return;
        };

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

        // Delivered through the shared primitive so this path gets the same
        // readiness gate as session creation: the bracketed-paste markers are
        // only meaningful once the agent has enabled the mode, and writing
        // them before that puts escape bytes in the input box.
        //
        // No submit retries, deliberately. This targets the session the user
        // is looking at, and they may be typing in it — a late extra Enter
        // could submit a half-written message of theirs. A newly created
        // session has nobody at the keyboard, which is why creation retries
        // and this does not. See `dispatch::pty::deliver` (DEV-603).
        dispatch::pty::deliver(&tv, payload, &[], cx);
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
        let Some(tailer) = self.rich.transcript_tailer.as_mut() else {
            return;
        };
        let events = tailer.poll();
        if events.is_empty() {
            return;
        }
        let Some(view) = self.rich.view.as_ref().cloned() else {
            return;
        };
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

        let (allele_session_id, claude_session_id, cwd, agent_kind) = {
            let project = self.projects.get(active.project_idx)?;
            let session = project.sessions.get(active.session_idx)?;
            let cwd = session
                .clone_path
                .clone()
                .unwrap_or_else(|| project.source_path.clone());
            // Attachments scope by the stable workspace id (must not move on
            // /clear); the transcript tails the *current* Claude conversation.
            //
            // Resolve which agent format the transcript is in, so the tailer
            // uses the matching normalizing adapter (DEV-32). Defaults to
            // Claude when the session has no recorded agent.
            let agent_kind = session
                .agent_id
                .as_ref()
                .and_then(|id| self.user_settings.agents.iter().find(|a| &a.id == id))
                .map(|a| a.kind)
                .unwrap_or(crate::settings::AgentKind::Claude);
            (
                session.id.clone(),
                session.claude_session_id().to_string(),
                cwd,
                agent_kind,
            )
        };
        // Transcript density differs from terminal density — the
        // terminal is tuned for cramming rows, whereas the Rich view is
        // reading prose with nested cards. Bias the transcript font up
        // relative to the terminal setting, with a 15pt floor so it
        // stays legible even when the user has shrunk the terminal.
        let font_size = (self.user_settings.font_size + 2.0).max(15.0);

        let tool_visibility = self.user_settings.tool_visibility.clone();
        let view = cx.new(|cx| {
            rich::RichView::new(cx, allele_session_id.clone(), font_size, tool_visibility)
        });

        // ComposeBar submits bubble up as RichViewEvent::Submit. Route
        // them into the active PTY via the same bracketed-paste path
        // the Scratch Pad uses. Nothing here spawns or talks to the
        // `claude` binary directly.
        cx.subscribe(
            &view,
            |this: &mut Self, _v, event: &rich::RichViewEvent, cx| match event {
                rich::RichViewEvent::Submit { text, attachments } => {
                    let paths: Vec<PathBuf> = attachments.iter().map(|a| a.path.clone()).collect();
                    this.scratch_pad_send(text.clone(), paths, cx);
                }
                rich::RichViewEvent::AllowPermission => {
                    if let Some(cursor) = this.active {
                        if let Some(session) = this
                            .projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            if let Some(ref tv) = session.terminal_view {
                                tv.read(cx).send_input(b"\r");
                            }
                            session.status = session::SessionStatus::Running;
                            session.attention_context = None;
                        }
                    }
                    cx.notify();
                }
                rich::RichViewEvent::RejectPermission => {
                    // Decline the tool call by sending Escape to the PTY —
                    // Claude Code's permission prompt treats Esc as "No" (DEV-78).
                    if let Some(cursor) = this.active {
                        if let Some(session) = this
                            .projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            if let Some(ref tv) = session.terminal_view {
                                tv.read(cx).send_input(b"\x1b");
                            }
                            session.status = session::SessionStatus::Running;
                            session.attention_context = None;
                        }
                    }
                    cx.notify();
                }
                rich::RichViewEvent::OpenTerminal => {
                    // Hand the prompt to the raw terminal so the user can
                    // resolve it manually — switch to the terminal tab without
                    // resolving the prompt ourselves (DEV-78).
                    this.main_tab = MainTab::Claude;
                    cx.notify();
                }
            },
        )
        .detach();

        // Scope the tailer to the active session's JSONL. We point at
        // the ANTICIPATED path (derived from session id + dashed cwd)
        // even if the file doesn't exist yet — `TailedFile::read_new`
        // silently no-ops on a missing file and picks up content from
        // byte 0 the moment Claude Code writes it. This means opening
        // the Transcript tab on a brand-new session shows the internal
        // empty state ("Send a message to start.") and auto-populates
        // as soon as the first turn lands on disk, without any re-wire.
        self.rich.transcript_tailer = transcript::expected_session_jsonl(&cwd, &claude_session_id)
            .map(|jsonl| transcript::TranscriptTailer::new(jsonl, agent_kind));
        self.rich.view = Some(view);
        self.rich.cursor = Some(active);
        self.rich.view.clone()
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
        // Lightweight sessions get no automatic Chrome tab. This path is
        // independent of `apply_project_config`, so without the guard a session
        // created to ask one question would still pop an about:blank tab — the
        // exact ceremony the flag exists to avoid. Explicit "Open in Chrome"
        // still works; only the automatic sync is suppressed. Keyed on
        // terminals rather than startup: the preview points at a dev server
        // only the terminals bring up. See DEV-400 and DEV-415.
        if self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .is_some_and(|s| !s.orchestration.runs_terminals())
        {
            self.browser_status.clear();
            return;
        }
        if !browser::chrome_running() {
            self.browser_status = "Start Google Chrome and try again.".to_string();
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
                self.browser_status = "Could not create the Chrome tab. Allow Allele to control Google Chrome in System Settings > Privacy & Security > Automation, then click Open in Chrome to retry."
                    .to_string();
            }
        }
    }

    /// Toggle the inline project-settings panel, seeding the branch/remote
    /// inputs from the project's settings when opening.
    pub(crate) fn toggle_project_settings_panel(&mut self, p_idx: usize, cx: &mut Context<Self>) {
        if self.editing_project_settings == Some(p_idx) {
            self.editing_project_settings = None;
        } else {
            self.editing_project_settings = Some(p_idx);
            let (branch, remote) = self
                .projects
                .get(p_idx)
                .map(|p| {
                    (
                        p.settings.default_branch.clone().unwrap_or_default(),
                        p.settings.remote.clone().unwrap_or_default(),
                    )
                })
                .unwrap_or_default();
            self.project_branch_input
                .update(cx, |i, cx| i.set_text_silent(&branch, cx));
            self.project_remote_input
                .update(cx, |i, cx| i.set_text_silent(&remote, cx));
        }
        cx.notify();
    }

    /// Wrap a floating `menu` (anchored at `position`) in a full-screen
    /// transparent backdrop, so clicking anywhere outside it — or right-clicking
    /// — dismisses it. `dismiss` clears whatever state hides the popover. This is
    /// the shared dismiss affordance for Allele's floating menus (DEV-67); menu
    /// items `stop_propagation`, so clicks on them never reach the backdrop.
    pub(crate) fn dismissable_popover(
        &self,
        position: Point<Pixels>,
        menu: impl IntoElement,
        dismiss: impl Fn(&mut Self, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_left = dismiss.clone();
        let on_right = dismiss;
        deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .child(
                    div()
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _e, _w, cx| on_left(this, cx)),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this: &mut Self, _e, _w, cx| on_right(this, cx)),
                        ),
                )
                .child(anchored().position(position).snap_to_window().child(menu)),
        )
    }

    /// Open the edit-session modal for an existing session.
    pub(crate) fn open_edit_session_modal(
        &mut self,
        project_idx: usize,
        session_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(project_idx) else {
            return;
        };
        let Some(session) = project.sessions.get(session_idx) else {
            return;
        };

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
                session.orchestration,
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
                        orchestration,
                    } => {
                        this.edit_session_modal = None;
                        this.pending_action = Some(
                            SessionAction::ApplySessionEdit {
                                project_idx: *project_idx,
                                session_idx: *session_idx,
                                label: label.clone(),
                                branch_slug: branch_slug.clone(),
                                comment: comment.clone(),
                                pinned: *pinned,
                                orchestration: *orchestration,
                            }
                            .into(),
                        );
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
    /// The in-memory snapshot that gets written to `settings.json`.
    ///
    /// Split from the write for the same reason as `state_snapshot`: building
    /// it needs `&self` and is cheap, writing it blocks and is not (DEV-623).
    pub(crate) fn settings_snapshot(&self) -> Settings {
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
            projects: self
                .projects
                .iter()
                .map(|p| ProjectSave {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    source_path: p.source_path.clone(),
                    settings: p.settings.clone(),
                })
                .collect(),
            drawer_height: self.drawer.height,
            drawer_visible: false,
            right_sidebar_visible: self.right_panel.visible,
            right_sidebar_width: self.right_panel.width,
            ..self.user_settings.clone()
        };
        settings
    }

    /// Write `settings.json` synchronously.
    ///
    /// Private to `checkpoint_persistence()` and the quit path — everything
    /// else must use `mark_settings_dirty()`, see ARCHITECTURE.md §4.4.
    pub(crate) fn save_settings(&self) {
        if let Err(e) = self.repos.settings.save(&self.settings_snapshot()) {
            warn!("Failed to save settings.json: {e}");
        }
    }

    /// The in-memory snapshot that gets written to `state.json`.
    ///
    /// Split out from the write so the value can be built on the foreground —
    /// where the data lives — and handed to a background task to write. The
    /// write is the part that blocks, and it was blocking the thread that
    /// draws. See `AppState::checkpoint_persistence` (DEV-609).
    pub(crate) fn state_snapshot(&self) -> PersistedState {
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
        persisted
    }

    /// Persist every session across every project to `~/.allele/state.json`,
    /// synchronously.
    ///
    /// Errors are logged but not surfaced — losing a state write is survivable,
    /// the orphan sweep cleans up any mismatch on next startup.
    ///
    /// Private to `checkpoint_persistence()` and the quit path — everything
    /// else must use `mark_state_dirty()`, see ARCHITECTURE.md §4.4.
    pub(crate) fn save_state(&self) {
        if let Err(e) = self.repos.state.save(&self.state_snapshot()) {
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

        // Existing local branches in the project's source repo, so the modal
        // can flag when a typed branch name will be checked out vs. created.
        let existing_branches = self
            .projects
            .get(project_idx)
            .map(|p| git::list_local_branches(&p.source_path))
            .unwrap_or_default();

        let entity = cx.new(|cx| {
            new_session_modal::NewSessionModal::new(
                cx,
                project_idx,
                agents,
                default_agent_idx,
                default_label,
                existing_branches,
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
                        orchestration,
                    } => {
                        this.new_session_modal = None;
                        this.pending_action = Some(
                            SessionAction::AddSessionWithDetails {
                                project_idx: *project_idx,
                                label: label.clone(),
                                branch_slug: branch_slug.clone(),
                                agent_id: agent_id.clone(),
                                initial_prompt: initial_prompt.clone(),
                                orchestration: *orchestration,
                            }
                            .into(),
                        );
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
        // A session that runs none of the project's setup opts out of the whole
        // of this: no port allocation, no `startup` command, no drawer
        // terminals, no preview URL. Checked here rather than at the call sites
        // because this runs on cold-resume and on Retry Startup as well as at
        // creation, and every one of those must stay quiet. See DEV-400.
        //
        // `StartupOnly` continues past this point and is gated further down, in
        // `spawn_terminals_and_preview` — the port is still allocated, because
        // the startup command's `{port}` substitution depends on it. See DEV-415.
        if self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .is_some_and(|s| !s.orchestration.runs_startup())
        {
            return;
        }

        let (clone_path, project_settings) = match self.projects.get(cursor.project_idx) {
            Some(project) => {
                let cp = project
                    .sessions
                    .get(cursor.session_idx)
                    .and_then(|s| s.clone_path.clone());
                (cp, project.settings.clone())
            }
            None => return,
        };
        let Some(clone_path) = clone_path else { return };
        // allele.json in the project root takes precedence (backwards compat),
        // then fall back to orchestration fields in project settings.
        let Some(cfg) = config::ProjectConfig::load(&clone_path)
            .or_else(|| config::ProjectConfig::from_settings(&project_settings))
        else {
            return;
        };

        // Skip ports already claimed by other sessions. Two sources of
        // truth, unioned: (1) the durable Traefik route files, which a
        // suspended session keeps even with its dev server down, and (2)
        // in-memory ports held by other live sessions this run. Without
        // this, a resumed session can be handed a port a suspended session
        // still owns, colliding on a single port. The current session's own
        // route file is excluded so it can reclaim its previous port.
        let self_stem = clone_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| format!("session-{s}"));
        let mut reserved = crate::base_infra::registered_ports(self_stem.as_deref());
        for (pi, project) in self.projects.iter().enumerate() {
            for (si, session) in project.sessions.iter().enumerate() {
                if (pi, si) == (cursor.project_idx, cursor.session_idx) {
                    continue;
                }
                if let Some(p) = session.allocated_port {
                    reserved.insert(p);
                }
            }
        }
        let port = config::allocate_port(&reserved);

        // Drop any pre-existing drawer tabs from a prior materialisation —
        // the config is the source of truth for this session's layout.
        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            session.drawer_tabs.clear();
            session.parked_drawer_tabs.clear();
            session.drawer_parked_at = None;
            session.drawer_active_tab = 0;
            session.allocated_port = port;
        }

        let project_name = self
            .projects
            .get(cursor.project_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let startup = cfg
            .startup
            .as_ref()
            .map(|s| config::resolve_script_command(s, &project_name))
            .map(|s| config::substitute(&s, port, &clone_path))
            .filter(|s| !s.trim().is_empty());

        // How long a per-project `startup` command may run before we treat it
        // as wedged, kill it, and surface a retryable error. Generous headroom
        // for a legitimate first-run migrate + seed; a healthy run finishes in
        // seconds.
        const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

        // The project's declared environment reaches `startup` too, so a
        // migrate/seed script resolves the same toolchain the session will
        // (DEV-485). `cfg` is already loaded, so no second config read.
        let inherited_path = std::env::var("PATH").ok();
        let startup_env = config::ProjectEnv::from_config(&cfg, &project_settings).materialise(
            port,
            &clone_path,
            inherited_path.as_deref(),
        );

        if let Some(startup_cmd) = startup {
            // Show initial status in sidebar while startup runs.
            if let Some(session) = self
                .projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.startup_status = Some("Starting…".into());
            }
            cx.notify();

            let clone_for_task = clone_path.clone();
            cx.spawn(async move |this, cx| {
                let (tx, rx) = std::sync::mpsc::channel::<String>();
                // Carries the child's process-group id back to the poll loop so
                // it can kill the whole startup tree (sh → script → php …) if the
                // command hangs past the timeout.
                let (pgid_tx, pgid_rx) = std::sync::mpsc::channel::<u32>();

                // Run the startup command on the background executor.
                // Lines from stdout are sent via the channel so the UI
                // thread can poll them without blocking.
                cx.background_executor()
                    .spawn({
                        let cmd = startup_cmd.clone();
                        let cwd = clone_for_task.clone();
                        let env = startup_env.clone();
                        async move {
                            use std::os::unix::process::CommandExt;
                            let child = std::process::Command::new("sh")
                                .arg("-c")
                                .arg(&cmd)
                                .current_dir(&cwd)
                                .envs(env)
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::piped())
                                // New process group (pgid == child pid) so a
                                // timeout can SIGKILL the entire tree at once.
                                .process_group(0)
                                .spawn();
                            match child {
                                Ok(mut child) => {
                                    let _ = pgid_tx.send(child.id());
                                    // Drain stderr on its own thread. If we leave
                                    // the stderr pipe unread, a startup command that
                                    // logs verbosely (e.g. a first-run DB seed) fills
                                    // the OS pipe buffer (~64 KB) and then blocks on
                                    // its next write — the child never exits, stdout
                                    // stops, and the sidebar status freezes forever.
                                    let stderr_drain = child.stderr.take().map(|stderr| {
                                        std::thread::spawn(move || {
                                            use std::io::BufRead;
                                            for line in std::io::BufReader::new(stderr)
                        .lines()
                        .map_while(Result::ok)
                    {
                                                warn!("allele: startup command (stderr): {line}");
                                            }
                                        })
                                    });
                                    if let Some(stdout) = child.stdout.take() {
                                        use std::io::BufRead;
                                        for line in std::io::BufReader::new(stdout)
                        .lines()
                        .map_while(Result::ok)
                    {
                                            let _ = tx.send(line);
                                        }
                                    }
                                    if let Some(handle) = stderr_drain {
                                        let _ = handle.join();
                                    }
                                    match child.wait() {
                                        Ok(s) if !s.success() => {
                                            warn!("allele: startup command exited with {s} — continuing");
                                        }
                                        Err(e) => {
                                            warn!("allele: failed to wait on startup command: {e} — continuing");
                                        }
                                        _ => {}
                                    }
                                }
                                Err(e) => {
                                    warn!("allele: failed to run startup command: {e} — continuing");
                                }
                            }
                            drop(tx);
                        }
                    })
                    .detach();

                // Poll the channel for status lines and update the sidebar.
                // A startup command that hangs (e.g. a DB seed blocked on a
                // contended shared server) would otherwise freeze the status
                // forever and never spawn the drawer terminals — so we bound
                // the wait and kill the tree if it overruns.
                let deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
                let mut child_pgid: Option<u32> = None;
                let mut last_status: Option<String> = None;
                let mut timed_out = false;
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(100))
                        .await;

                    if child_pgid.is_none() {
                        if let Ok(pgid) = pgid_rx.try_recv() {
                            child_pgid = Some(pgid);
                        }
                    }

                    let mut last_line = None;
                    let mut done = false;
                    loop {
                        match rx.try_recv() {
                            Ok(line) => { last_line = Some(line); }
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                done = true;
                                break;
                            }
                        }
                    }
                    if let Some(line) = last_line {
                        let trimmed = line.trim().to_string();
                        if !trimmed.is_empty() {
                            last_status = Some(trimmed.clone());
                            let _ = this.update(cx, |this: &mut Self, cx| {
                                if let Some(session) = this
                                    .projects
                                    .get_mut(cursor.project_idx)
                                    .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                                {
                                    session.startup_status = Some(trimmed);
                                }
                                cx.notify();
                            });
                        }
                    }
                    if done { break; }
                    if std::time::Instant::now() >= deadline {
                        timed_out = true;
                        // SIGKILL the whole process group. The reader task's
                        // stdout then hits EOF, `child.wait()` reaps sh, and the
                        // background closure ends on its own.
                        if let Some(pgid) = child_pgid {
                            unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
                        }
                        break;
                    }
                }

                if timed_out {
                    // Leave the session in a recoverable error state: surface
                    // the failure with a one-click Retry instead of spawning
                    // half-configured terminals. Retry re-runs the whole flow.
                    let stalled_on = last_status
                        .map(|s| format!(" during: {s}"))
                        .unwrap_or_default();
                    warn!("allele: startup command timed out{stalled_on} — killed");
                    let _ = this.update(cx, move |this: &mut Self, cx| {
                        if let Some(session) = this
                            .projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            session.startup_status = None;
                            session.operation_error = Some(crate::session::OperationError {
                                kind: crate::session::OperationErrorKind::Startup,
                                message: format!("Startup timed out{stalled_on}"),
                            });
                        }
                        cx.notify();
                    });
                    return;
                }

                // Clear status and spawn terminals — needs window access,
                // so we schedule a pending action that the render loop picks up.
                let _ = this.update(cx, move |this: &mut Self, cx| {
                    if let Some(session) = this
                        .projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        session.startup_status = None;
                    }
                    this.pending_startup = Some((cursor, cfg, port, clone_path));
                    this.pending_action = Some(SessionAction::SpawnStartupTerminals(cursor).into());
                    cx.notify();
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
        // `StartupOnly` gets the startup command but none of this (DEV-415):
        // a session that needs a database provisioned so it can run tests does
        // not need a server, a queue worker, a scheduler and a bundler it will
        // never look at. Gated here rather than at the two call sites so both
        // the with-startup and without-startup paths are covered by one check.
        if self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .is_some_and(|s| !s.orchestration.runs_terminals())
        {
            return;
        }

        for term in &cfg.terminals {
            let substituted = config::substitute(&term.command, port, clone_path);
            // Always spawn an interactive shell (inherit default — None).
            // If a startup command was declared, push it into the PTY's
            // stdin buffer so the freshly-loaded shell reads and executes
            // it as if the user had typed it. When the command exits or is
            // interrupted (Ctrl+C), the shell is still there for the user
            // to restart or run anything else.
            self.spawn_drawer_tab(
                cursor,
                Some(term.label.clone()),
                None,
                Some(substituted),
                window,
                cx,
            );
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

    /// Snapshot the sidebar's current shape. See `StructuralStamp`.
    pub(crate) fn structural_stamp(&self) -> StructuralStamp {
        app_state::structural_stamp(&self.projects)
    }

    /// Arm a confirmation gate, recording the shape of the world it was armed
    /// against.
    ///
    /// Every gate goes through here rather than assigning the field directly,
    /// so a confirmation added later inherits the staleness check for free
    /// instead of depending on someone remembering it exists.
    pub(crate) fn arm_confirmation(&mut self, arm: impl FnOnce(&mut ConfirmationState)) {
        let stamp = self.structural_stamp();
        arm(&mut self.confirming);
        self.confirming.armed_at = Some(stamp);
    }

    /// True when a gate is armed against a shape the sidebar no longer has —
    /// something was added or removed underneath the prompt, so whatever index
    /// it holds can no longer be trusted to mean what the user picked.
    ///
    /// A pure reorder leaves all three counts untouched and so is invisible
    /// here; the reorder handlers clear the gates themselves.
    pub(crate) fn confirmation_is_stale(&self) -> bool {
        self.confirming.any_armed()
            && self
                .confirming
                .armed_at
                .is_some_and(|armed| armed != self.structural_stamp())
    }

    /// Remove a project and all its sessions (deleting all clones asynchronously).
    pub(crate) fn remove_project(
        &mut self,
        project_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if project_idx >= self.projects.len() {
            return;
        }

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
                        Some(SessionCursor {
                            project_idx: p_idx,
                            session_idx: 0,
                        })
                    } else {
                        None
                    }
                })
            }
            Some(active) if active.project_idx > project_idx => Some(SessionCursor {
                project_idx: active.project_idx - 1,
                session_idx: active.session_idx,
            }),
            other => other,
        };

        // Every later project just shifted index. The stamp check would catch
        // this on the next render anyway, but dismissing here means the prompt
        // never survives even one frame past the removal.
        self.confirming.dismiss_armed();

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
        if let Some(log_path) = paths::crash_log_file() {
            if let Some(log_dir) = log_path.parent() {
                let _ = std::fs::create_dir_all(log_dir);
            }
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let location = info
                .location()
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

fn main() {
    // `--mcp-serve` never returns; it must claim stdout before anything logs.
    dispatch::mcp::exit_if_serving();

    // Refuse unrecognised arguments before anything with a side effect runs.
    // Allele has no CLI, and everything below this point — tracing, the
    // descriptor ceiling, the orphan sweep, `state.json` — assumes it is the
    // app. `allele sessions status <id>` used to reach all of it and open a
    // second window. See `cli` for why that is worse than it looks.
    let (home_override, sandbox_mode) =
        match cli::classify(&std::env::args().skip(1).collect::<Vec<_>>()) {
            cli::Launch::Usage { code } => {
                eprintln!("{}", cli::USAGE);
                std::process::exit(code);
            }
            cli::Launch::Gui { home, sandbox } => (home, sandbox),
        };

    // Fix the data root before ANYTHING reads a path or spawns a child. Every
    // line below — tracing, the panic hook, the orphan sweep, state.json, the
    // MCP control socket — resolves through `paths`, so a root chosen any later
    // would leave half the process writing to one tree and half to another.
    let root = if sandbox_mode {
        paths::default_sandbox_root()
    } else {
        home_override
    };
    paths::init(root);
    if sandbox_mode {
        sandbox::seed();
    }

    errors::init_tracing();
    install_panic_hook();

    // Raise the descriptor ceiling before anything spawns. launchd gives GUI
    // apps a soft RLIMIT_NOFILE of 256, which a few dozen sessions' worth of
    // PTYs exhausts — new sessions then die with EMFILE. Children inherit the
    // limit in force when they fork, so this has to come first.
    fd_limit::raise_open_file_limit();

    // Fix launchd's bare GUI PATH before anything spawns — PTY sessions,
    // the git availability check, and agent detection all inherit it.
    shell_env::fix_launchd_path();

    if std::env::args().any(|arg| arg == "--capture-ui") {
        match debug_capture::request_capture_and_wait() {
            Ok(path) => println!("{}", path.display()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }

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
    if let Some(stale) = paths::legacy_browsers_dir() {
        if stale.exists() {
            let _ = std::fs::remove_dir_all(&stale);
        }
    }

    let application = Application::new().with_assets(crate::assets::Assets);

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
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf").as_slice(),
                ),
            ])
            .expect("failed to load bundled fonts");

        mac_menu::install_app_menu(cx);

        // Load persisted settings
        let loaded_settings = Settings::load();
        info!(
            "Loaded settings: sidebar_width={}, font_size={}",
            loaded_settings.sidebar_width, loaded_settings.font_size
        );

        // Load persisted session state (may be empty on first run).
        let loaded_state = PersistedState::load();
        info!(
            "Loaded persisted state: {} sessions",
            loaded_state.sessions.len()
        );

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

        // Install each agent adapter's on-disk event integration (e.g. the
        // opencode events plugin). Idempotent; failures are logged and
        // swallowed so a broken integration never blocks launch.
        agents::install_integrations();

        // Memory watchdog — monitors RSS and force-quits at 8 GB to
        // prevent runaway leaks from bricking the system.
        memory_watchdog::spawn(cx);

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
                    // The window title is the only marker visible when the
                    // app is not focused, so it carries the sandbox flag too.
                    title: Some(if paths::is_redirected() {
                        "Allele — SANDBOX".into()
                    } else {
                        "Allele".into()
                    }),
                    // Content extends under the titlebar; the 38px header/tab
                    // rows below center against the traffic lights.
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.0), px(13.0))),
                }),
                // Vibrancy: the sidebar paints a translucent surface over this.
                window_background: WindowBackgroundAppearance::Blurred,
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
                            projects: this
                                .projects
                                .iter()
                                .map(|p| ProjectSave {
                                    id: p.id.clone(),
                                    name: p.name.clone(),
                                    source_path: p.source_path.clone(),
                                    settings: p.settings.clone(),
                                })
                                .collect(),
                            ..this.user_settings.clone()
                        };
                        if let Err(e) = this.repos.settings.save(&settings) {
                            warn!("Failed to save settings.json on window-bounds change: {e}");
                        }
                    })
                    .detach();

                    // Rehydrate projects from settings.
                    let mut projects: Vec<Project> = settings_for_window
                        .projects
                        .iter()
                        .map(|p| {
                            let mut proj = Project::new(p.name.clone(), p.source_path.clone());
                            proj.id = p.id.clone();
                            proj.settings = p.settings.clone();
                            proj
                        })
                        .collect();

                    // Rehydrate archived sessions from state.json so the
                    // archive browser shows human-readable labels.
                    for archived in &loaded_state_for_window.archived_sessions {
                        if let Some(project) =
                            projects.iter_mut().find(|p| p.id == archived.project_id)
                        {
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
                                        label: format!(
                                            "Session {}",
                                            &entry.session_id[..8.min(entry.session_id.len())]
                                        ),
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
                        let Some(project) =
                            projects.iter_mut().find(|p| p.id == persisted.project_id)
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
                            std::time::Duration::from_secs(persisted.active_runtime_secs),
                            persisted.clone_path.clone(),
                        )
                        .with_drawer_tabs(persisted.drawer_tabs(), persisted.drawer_active_tab)
                        .with_browser(persisted.browser_tab_id, persisted.browser_last_url.clone())
                        .with_agent_id(persisted.agent_id.clone())
                        .with_claude_session_id(persisted.claude_session_id.clone())
                        .with_conversation_choice_explicit(persisted.conversation_choice_explicit);
                        session.pinned = persisted.pinned;
                        session.comment = persisted.comment.clone();
                        session.branch_name = persisted.branch_name.clone();
                        session.branch_locked = persisted.branch_locked;
                        session.orchestration = persisted.orchestration();
                        conversations::repair_session_pointer(&mut session);
                        session.origin = persisted.origin.clone();
                        project.sessions.push(session);
                    }

                    dispatch::spawn_control_socket(cx);

                    startup::pollers::spawn_all(cx);
                    startup::actions::register_all(cx);

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
                                            Some(
                                                SessionAction::ResumeSession {
                                                    project_idx: p_idx,
                                                    session_idx: s_idx,
                                                }
                                                .into(),
                                            )
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

                    let sidebar_filter_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "Search sessions…"));
                    let project_branch_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "auto"));
                    cx.subscribe(
                        &project_branch_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            if matches!(event, text_input::TextInputEvent::Changed) {
                                if let Some(p_idx) = this.editing_project_settings {
                                    if let Some(project) = this.projects.get_mut(p_idx) {
                                        let t = input.read(cx).text().trim().to_string();
                                        project.settings.default_branch =
                                            if t.is_empty() { None } else { Some(t) };
                                        this.mark_settings_dirty();
                                    }
                                }
                            }
                        },
                    )
                    .detach();
                    let project_remote_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "origin"));
                    cx.subscribe(
                        &project_remote_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            if matches!(event, text_input::TextInputEvent::Changed) {
                                if let Some(p_idx) = this.editing_project_settings {
                                    if let Some(project) = this.projects.get_mut(p_idx) {
                                        let t = input.read(cx).text().trim().to_string();
                                        project.settings.remote =
                                            if t.is_empty() { None } else { Some(t) };
                                        this.mark_settings_dirty();
                                    }
                                }
                            }
                        },
                    )
                    .detach();
                    cx.subscribe(
                        &sidebar_filter_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            if matches!(event, text_input::TextInputEvent::Changed) {
                                this.sidebar_filter = input.read(cx).text().to_lowercase();
                                cx.notify();
                            }
                        },
                    )
                    .detach();
                    let reader_find_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "Find in file…"));
                    cx.subscribe(
                        &reader_find_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            match event {
                                text_input::TextInputEvent::Changed => {
                                    this.reader.find_query = input.read(cx).text().to_string();
                                    this.recompute_find_matches();
                                    // Jump to the first hit as you type.
                                    this.focus_current_find_match();
                                    cx.notify();
                                }
                                // Enter in the find field advances to the next match.
                                text_input::TextInputEvent::Submitted => {
                                    this.find_step(1);
                                    cx.notify();
                                }
                            }
                        },
                    )
                    .detach();
                    let file_palette_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "Go to file…"));
                    cx.subscribe(
                        &file_palette_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            if matches!(event, text_input::TextInputEvent::Changed) {
                                let q = input.read(cx).text().to_string();
                                if let Some(palette) = this.file_palette.as_mut() {
                                    palette.recompute(&q);
                                }
                                cx.notify();
                            }
                        },
                    )
                    .detach();
                    let search_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "Search project…"));
                    cx.subscribe(
                        &search_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            if matches!(event, text_input::TextInputEvent::Changed) {
                                let q = input.read(cx).text().to_string();
                                this.run_search(&q, cx);
                            }
                        },
                    )
                    .detach();
                    let command_palette_input =
                        cx.new(|cx| text_input::TextInput::new(cx, "", "Type a command…"));
                    cx.subscribe(
                        &command_palette_input,
                        |this: &mut AppState, input, event: &text_input::TextInputEvent, cx| {
                            if matches!(event, text_input::TextInputEvent::Changed) {
                                let q = input.read(cx).text().to_string();
                                this.set_command_query(&q, cx);
                            }
                        },
                    )
                    .detach();

                    // Auto-start the base infrastructure (Traefik + network)
                    // if enabled. Fire-and-forget on the background executor —
                    // it must not block window creation, and failures are
                    // logged (the user sees status when they open Settings).
                    if settings_for_window.base_infra_enabled {
                        cx.background_executor()
                            .spawn(async move {
                                if let Err(e) = crate::base_infra::up() {
                                    warn!("base-infra auto-start failed: {e}");
                                }
                            })
                            .detach();
                    }

                    AppState {
                        projects,
                        active: initial_active,
                        overview_project_id: None,
                        pending_action: initial_pending,
                        sidebar: SidebarState {
                            visible: settings_for_window.sidebar_visible,
                            width: settings_for_window.sidebar_width.max(SIDEBAR_MIN_WIDTH),
                            resizing: false,
                        },
                        right_panel: RightPanelState {
                            visible: settings_for_window.right_sidebar_visible,
                            width: settings_for_window
                                .right_sidebar_width
                                .max(RIGHT_SIDEBAR_MIN_WIDTH),
                            resizing: false,
                        },
                        changes: ChangesPanelState::default(),
                        drawer: DrawerState {
                            height: settings_for_window.drawer_height.max(DRAWER_MIN_HEIGHT),
                            resizing: false,
                            rename: None,
                            rename_focus: None,
                            main_area_top: Default::default(),
                        },
                        reader: ReaderState {
                            selected_path: None,
                            expanded_dirs: HashSet::new(),
                            preview: None,
                            context_menu: None,
                            find_query: String::new(),
                            find_active: false,
                            find_matches: Vec::new(),
                            find_current: 0,
                            md_view_source: false,
                            md_scroll: gpui::ScrollHandle::new(),
                            recent: Vec::new(),
                            reveal_line: None,
                            source_scroll: gpui::ScrollHandle::new(),
                            active_root: None,
                            sessions: std::collections::HashMap::new(),
                        },
                        confirming: ConfirmationState {
                            discard: None,
                            dirty_session: None,
                            quit: false,
                            remove_project: None,
                            delete_archive: None,
                            delete_all_archives: None,
                            armed_at: None,
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
                        sync_notice: None,
                        main_tab: MainTab::Claude,
                        browser_status: String::new(),
                        scratch_pad: None,
                        scratch_pad_history: loaded_state.scratch_pad_history.clone(),
                        new_session_modal: None,
                        session_context_menu: None,
                        project_context_menu: None,
                        edit_session_modal: None,
                        naming_modal: None,
                        conversation_picker: None,
                        pending_conversation_choice: None,
                        conversation_choice_confirmed: None,
                        remote_browser: None,
                        sidebar_filter_input,
                        reader_find_input,
                        file_palette: None,
                        file_palette_input,
                        file_index: Default::default(),
                        search: None,
                        search_input,
                        command_palette: None,
                        command_palette_input,
                        project_branch_input,
                        project_remote_input,
                        sidebar_filter: String::new(),
                        pending_startup: None,
                        base_infra_status: None,
                        state_dirty: false,
                        settings_dirty: false,
                        state_gate: Default::default(),
                        settings_gate: Default::default(),
                        persist_flush_scheduled: false,
                        repos: repositories::Repositories::production(),
                        platform: crate::platform::global().clone_arcs(),
                        capture_ui_requested: false,
                        // Captured on the first render (see `Render`).
                        main_window: None,
                        pending_dispatch_origins: Default::default(), // DEV-415
                    }
                })
            },
        )
        .expect("open main window");
    });
}
