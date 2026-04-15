mod browser;
mod terminal;
mod sidebar;
mod clone;
mod config;
mod git;
mod hooks;
mod project;
mod scratch_pad;
mod session;
mod settings;
mod settings_window;
mod state;
mod trust;

use gpui::*;
use project::Project;
actions!(allele, [About, Quit, ToggleSidebarAction, ToggleDrawerAction, OpenSettings, OpenScratchPadAction]);
use session::{DrawerTab, Session, SessionStatus};
use settings::{ProjectSave, Settings};
use state::{ArchivedSession, PersistedSession, PersistedState};
use terminal::{ShellCommand, TerminalEvent, TerminalView};
use terminal::pty_terminal::PtyTerminal;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Which view is shown in the main (center) column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MainTab {
    Claude,
    Editor,
    Browser,
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

#[derive(Debug)]
enum PendingAction {
    NewSessionInActiveProject,
    CloseActiveSession,
    FocusActive,
    OpenProjectAtPath(PathBuf),
    AddSessionToProject(usize), // project index
    RemoveProject(usize),
    /// Kill the PTY, keep the clone, mark Suspended. Next click cold-resumes.
    CloseSessionKeepClone { project_idx: usize, session_idx: usize },
    /// Ask for confirmation before discarding — sets `confirming_discard`.
    RequestDiscardSession { project_idx: usize, session_idx: usize },
    /// Cancel an in-flight discard confirmation.
    CancelDiscard,
    /// Permanently delete the clone and remove the session from state.
    DiscardSession { project_idx: usize, session_idx: usize },
    SelectSession { project_idx: usize, session_idx: usize },
    /// Merge an archived session ref into canonical's working tree.
    MergeArchive { project_idx: usize, archive_idx: usize },
    /// Delete an archive ref without merging.
    DeleteArchive { project_idx: usize, archive_idx: usize },
    /// Merge session work into canonical and close (archive + merge + delete clone).
    MergeAndClose { project_idx: usize, session_idx: usize },
    /// Toggle the bottom drawer terminal panel.
    ToggleDrawer,
    /// Create a new drawer terminal tab in the active session.
    NewDrawerTab,
    /// Switch the active drawer tab.
    SwitchDrawerTab(usize),
    /// Close a drawer tab by index. Closing the last tab hides the drawer.
    CloseDrawerTab(usize),
    /// Enter rename mode for a drawer tab.
    StartRenameDrawerTab(usize),
    /// Commit the current rename buffer as the tab's new name.
    CommitRenameDrawerTab,
    /// Cancel rename mode without saving.
    CancelRenameDrawerTab,
    /// Toggle the left sidebar visibility.
    ToggleSidebar,
    /// Toggle the right sidebar visibility.
    ToggleRightSidebar,
    /// Source path missing — open folder picker so the user can relocate.
    RelocateProject(usize),
    /// Proceed with session creation despite dirty canonical.
    ProceedDirtySession(usize),
    /// Cancel dirty-state session creation.
    CancelDirtySession,
    /// Replace the session-cleanup-paths list with a new value and persist.
    /// Emitted by the Settings window on every edit.
    UpdateCleanupPaths(Vec<String>),
    /// Replace the external-editor command with a new value and persist.
    /// Emitted by the Settings window on every edit.
    UpdateExternalEditor(String),
    /// Toggle Chrome browser integration on/off. When toggled off we clear
    /// the current sync status so the Browser tab shows the disabled state.
    UpdateBrowserIntegration(bool),
    /// Auto-resume a session after launch. Fires once from the first render
    /// tick so `resume_session` has a valid `window` / `cx`.
    ResumeSession { project_idx: usize, session_idx: usize },
    /// Activate the Chrome tab linked to the currently-active session,
    /// creating one if the session has no tab id yet or the stored id is
    /// stale. Fired on session switch, session resume, and Browser-tab
    /// click.
    SyncBrowserToActiveSession,
    /// Close the Chrome tab linked to the given session and clear its
    /// stored tab id. User-initiated via the Browser tab's Close button.
    CloseBrowserTabForSession { project_idx: usize, session_idx: usize },
    /// Open (or re-focus) the scratch pad compose overlay.
    OpenScratchPad,
}

/// Position of a session in the project tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SessionCursor {
    project_idx: usize,
    session_idx: usize,
}

struct AppState {
    projects: Vec<Project>,
    active: Option<SessionCursor>,
    claude_path: Option<String>,
    pending_action: Option<PendingAction>,
    // Sidebar state
    sidebar_visible: bool,
    sidebar_width: f32,
    sidebar_resizing: bool,
    /// Inline confirmation gate for the Discard action. When `Some(cursor)`
    /// the sidebar row at that cursor shows a confirm/cancel prompt instead
    /// of the usual buttons.
    confirming_discard: Option<SessionCursor>,
    /// Project index awaiting dirty-state confirmation before session create.
    confirming_dirty_session: Option<usize>,
    /// Absolute path to the Allele hooks.json, passed to claude via
    /// `--settings <path>` at every spawn. `None` if install_if_missing
    /// failed — in that case hooks are silently disabled and the app still
    /// functions normally.
    hooks_settings_path: Option<PathBuf>,
    /// Current user settings (sound/notification preferences).
    user_settings: Settings,
    // Drawer terminal state (visibility is per-session on Session struct)
    drawer_height: f32,
    drawer_resizing: bool,
    /// Active inline tab rename: (session cursor, tab index, current buffer).
    /// When Some, the tab strip renders that tab as an editable label.
    drawer_rename: Option<(SessionCursor, usize, String)>,
    /// Focus handle for the inline rename input. Created lazily when rename
    /// mode first activates in a given AppState instance.
    drawer_rename_focus: Option<FocusHandle>,
    // Right sidebar state
    right_sidebar_visible: bool,
    right_sidebar_width: f32,
    right_sidebar_resizing: bool,
    /// When true, a quit confirmation banner is shown because running sessions exist.
    confirming_quit: bool,
    /// Project index whose settings panel is currently open in the sidebar.
    editing_project_settings: Option<usize>,
    /// Live handle to an open Settings window. Keeps ⌘, from spawning
    /// duplicates — when set, the action re-activates the existing window
    /// instead of opening a new one. Cleared when the window closes.
    settings_window: Option<WindowHandle<settings_window::SettingsWindowState>>,
    /// Which view the center column is currently showing.
    main_tab: MainTab,
    /// File path currently selected in the Editor tab's file tree.
    editor_selected_path: Option<PathBuf>,
    /// Directories expanded in the Editor tab's file tree.
    editor_expanded_dirs: HashSet<PathBuf>,
    /// Cached (path, contents) of the currently previewed file.
    editor_preview: Option<(PathBuf, String)>,
    /// Right-click context menu target for the Editor file tree.
    /// Stores (right-clicked path, window-space position of the click).
    editor_context_menu: Option<(PathBuf, Point<Pixels>)>,
    /// Status text for the Browser tab panel (e.g. "Chrome is not
    /// running", "Linked to tab #…"). Updated by SyncBrowserToActiveSession
    /// and rendered by render_browser_placeholder.
    browser_status: String,
    /// Scratch pad compose overlay. `Some` while the overlay is visible.
    scratch_pad: Option<Entity<scratch_pad::ScratchPad>>,
}

const SIDEBAR_MIN_WIDTH: f32 = 160.0;
const DRAWER_MIN_HEIGHT: f32 = 100.0;
const RIGHT_SIDEBAR_MIN_WIDTH: f32 = 160.0;

impl AppState {
    /// Get the currently active session, if any.
    fn active_session(&self) -> Option<&Session> {
        let cursor = self.active?;
        self.projects
            .get(cursor.project_idx)?
            .sessions
            .get(cursor.session_idx)
    }

    /// Open the scratch pad compose overlay, or re-focus it if already open.
    fn open_scratch_pad(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scratch_pad.is_none() {
            let entity = cx.new(|cx| scratch_pad::ScratchPad::new(cx));
            cx.subscribe(
                &entity,
                |this: &mut Self, _pad, event: &scratch_pad::ScratchPadEvent, cx| {
                    match event {
                        scratch_pad::ScratchPadEvent::Send { text, attachments } => {
                            this.scratch_pad_send(text.clone(), attachments.clone(), cx);
                            this.scratch_pad = None;
                            this.pending_action = Some(PendingAction::FocusActive);
                            cx.notify();
                        }
                        scratch_pad::ScratchPadEvent::Close => {
                            this.scratch_pad = None;
                            this.pending_action = Some(PendingAction::FocusActive);
                            cx.notify();
                        }
                    }
                },
            )
            .detach();
            self.scratch_pad = Some(entity);
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

    /// Root directory for the Editor tab's file tree: the active session's
    /// clone path if present, otherwise the project's source path.
    fn editor_workspace_root(&self) -> Option<PathBuf> {
        let cursor = self.active?;
        let project = self.projects.get(cursor.project_idx)?;
        let session = project.sessions.get(cursor.session_idx)?;
        Some(
            session
                .clone_path
                .clone()
                .unwrap_or_else(|| project.source_path.clone()),
        )
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
                                Some(PendingAction::SyncBrowserToActiveSession);
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
            .child(tab("main-tab-editor", "Editor", MainTab::Editor));
        if self.browser_tab_available() {
            strip = strip.child(tab("main-tab-browser", "Browser", MainTab::Browser));
        }
        strip
    }

    /// Two-column Editor view: file tree on the left, file preview on the right.
    fn render_editor_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let root = self.editor_workspace_root();

        let tree_col = {
            let mut col = div()
                .id("editor-tree-scroll")
                .w(px(240.0))
                .flex_shrink_0()
                .h_full()
                .overflow_y_scroll()
                .bg(rgb(0x181825))
                .border_r_1()
                .border_color(rgb(0x313244))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4));

            if let Some(root_path) = root.clone() {
                let mut rows: Vec<AnyElement> = Vec::new();
                let mut counter: usize = 0;
                self.collect_tree_rows(&root_path, 0, &mut rows, &mut counter, cx);
                if rows.is_empty() {
                    col = col.child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .text_color(rgb(0x6c7086))
                            .child("(empty workspace)"),
                    );
                } else {
                    for row in rows {
                        col = col.child(row);
                    }
                }
            } else {
                col = col.child(
                    div()
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_color(rgb(0x6c7086))
                        .child("No active session"),
                );
            }

            col
        };

        let preview_col = {
            let mut col = div()
                .id("editor-preview-scroll")
                .flex_1()
                .min_w(px(0.0))
                .h_full()
                .overflow_scroll()
                .bg(rgb(0x1e1e2e))
                .p(px(12.0))
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4))
                .font_family("monospace");

            match (&self.editor_selected_path, &self.editor_preview) {
                (Some(sel), Some((p, contents))) if p == sel => {
                    col = col.child(
                        div()
                            .whitespace_normal()
                            .child(contents.clone()),
                    );
                }
                (Some(sel), _) => {
                    col = col.child(format!("Loading {}…", sel.display()));
                }
                _ => {
                    col = col
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(rgb(0x6c7086))
                        .child("Select a file to preview");
                }
            }

            col
        };

        let mut root = div()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(0x1e1e2e))
            .child(tree_col)
            .child(preview_col);

        if self.editor_context_menu.is_some() {
            root = root.child(self.render_editor_context_menu(cx));
        }

        root
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
                                Some(PendingAction::SyncBrowserToActiveSession);
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
                                    PendingAction::CloseBrowserTabForSession {
                                        project_idx: cur.project_idx,
                                        session_idx: cur.session_idx,
                                    },
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
    fn sync_browser_to_active(&mut self) {
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
                self.save_state();
            }
            None => {
                self.browser_status = "Could not create Chrome tab (check \
                    Automation permission)."
                    .to_string();
            }
        }
    }

    /// Floating right-click menu for the file tree. Rendered via
    /// `deferred` so it paints on top of sibling content, and positioned
    /// in window coordinates at the click site.
    fn render_editor_context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (path, position) = self.editor_context_menu.clone().unwrap();

        let item = |id: &'static str, label: &'static str, path: PathBuf, reveal: bool| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(0xcdd6f4))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x45475a)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _event, _window, cx| {
                        cx.stop_propagation();
                        if reveal {
                            Self::reveal_in_finder(&path);
                        } else {
                            this.open_in_external_editor(&path);
                        }
                        this.editor_context_menu = None;
                        cx.notify();
                    }),
                )
        };

        let menu = div()
            .flex()
            .flex_col()
            .min_w(px(220.0))
            .py(px(4.0))
            .bg(rgb(0x181825))
            .border_1()
            .border_color(rgb(0x45475a))
            .rounded(px(6.0))
            .shadow_md()
            .child(item(
                "editor-ctx-reveal",
                "Reveal in Finder",
                path.clone(),
                true,
            ))
            .child(item(
                "editor-ctx-open-external",
                "Open in External Editor",
                path,
                false,
            ));

        deferred(anchored().position(position).snap_to_window().child(menu))
    }

    /// Recursively build file-tree rows starting at `dir`.
    /// Directories render as "▸"/"▾" rows; files as plain rows.
    fn collect_tree_rows(
        &self,
        dir: &std::path::Path,
        depth: usize,
        out: &mut Vec<AnyElement>,
        counter: &mut usize,
        cx: &mut Context<Self>,
    ) {
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut entries: Vec<(PathBuf, bool, String)> = read
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    return None;
                }
                let is_dir = e.file_type().ok()?.is_dir();
                Some((e.path(), is_dir, name))
            })
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));

        let indent = px((depth * 12) as f32 + 8.0);

        for (path, is_dir, name) in entries {
            let is_expanded = self.editor_expanded_dirs.contains(&path);
            let is_selected = self.editor_selected_path.as_ref() == Some(&path);

            let label = if is_dir {
                let glyph = if is_expanded { "▾" } else { "▸" };
                format!("{glyph} {name}")
            } else {
                format!("  {name}")
            };

            let row_bg = if is_selected { 0x313244 } else { 0x181825 };
            let path_for_click = path.clone();

            let row_id = *counter;
            *counter += 1;
            let path_for_right_click = path.clone();
            let row = div()
                .id(("editor-tree-row", row_id))
                .flex()
                .flex_row()
                .items_center()
                .pl(indent)
                .pr(px(8.0))
                .py(px(2.0))
                .bg(rgb(row_bg))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(0x313244)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _event, _window, cx| {
                        let p = path_for_click.clone();
                        if p.is_dir() {
                            if this.editor_expanded_dirs.contains(&p) {
                                this.editor_expanded_dirs.remove(&p);
                            } else {
                                this.editor_expanded_dirs.insert(p);
                            }
                        } else {
                            this.editor_selected_path = Some(p.clone());
                            this.load_preview(p);
                        }
                        this.editor_context_menu = None;
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this: &mut Self, event: &MouseDownEvent, _window, cx| {
                        this.editor_context_menu =
                            Some((path_for_right_click.clone(), event.position));
                        cx.notify();
                    }),
                )
                .into_any_element();

            out.push(row);

            if is_dir && is_expanded {
                self.collect_tree_rows(&path, depth + 1, out, counter, cx);
            }
        }
    }

    /// Reveal a path in macOS Finder. For files, Finder selects the file
    /// inside its containing folder; for directories, it opens them.
    fn reveal_in_finder(path: &std::path::Path) {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }

    /// Spawn the user-configured external editor with `path` as an argument.
    /// Defaults to Sublime Text's `subl` CLI when no override is set.
    fn open_in_external_editor(&self, path: &std::path::Path) {
        let cmd = self
            .user_settings
            .external_editor_command
            .as_deref()
            .unwrap_or(settings::DEFAULT_EXTERNAL_EDITOR);
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return;
        }
        // Split on whitespace so users can set flags (e.g. `"code -g"`).
        let mut parts = trimmed.split_whitespace();
        let program = match parts.next() {
            Some(p) => p,
            None => return,
        };
        let mut command = std::process::Command::new(program);
        for arg in parts {
            command.arg(arg);
        }
        command.arg(path);
        if let Err(e) = command.spawn() {
            eprintln!(
                "Failed to launch external editor '{}': {e}",
                trimmed
            );
        }
    }

    /// Load a file into the preview cache. Skips binary files and anything
    /// over 512 KB with a placeholder string.
    fn load_preview(&mut self, path: PathBuf) {
        const MAX: u64 = 512 * 1024;
        let contents = match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX => "File too large to preview".to_string(),
            Ok(_) => match std::fs::read(&path) {
                Ok(bytes) => {
                    if bytes.contains(&0) {
                        "Binary file".to_string()
                    } else {
                        String::from_utf8_lossy(&bytes).into_owned()
                    }
                }
                Err(e) => format!("Could not read file: {e}"),
            },
            Err(e) => format!("Could not stat file: {e}"),
        };
        self.editor_preview = Some((path, contents));
    }

    fn save_settings(&self) {
        // Start from the live user_settings so attention preferences
        // (sound/notification opt-ins) are preserved on every write, then
        // override only the fields that the AppState is the source of truth
        // for (sidebar width, project list, etc.).
        let settings = Settings {
            sidebar_visible: self.sidebar_visible,
            sidebar_width: self.sidebar_width,
            font_size: 13.0,
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
            drawer_height: self.drawer_height,
            drawer_visible: false,
            right_sidebar_visible: self.right_sidebar_visible,
            right_sidebar_width: self.right_sidebar_width,
            ..self.user_settings.clone()
        };
        settings.save();
    }

    /// Persist every session across every project to `~/.allele/state.json`.
    /// Called after any mutation that creates, removes, or transitions a session.
    /// Errors are logged but not surfaced — losing a state write is survivable,
    /// the orphan sweep will clean up any mismatch on next startup.
    fn save_state(&self) {
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
        if let Err(e) = persisted.save() {
            eprintln!("Failed to save state.json: {e}");
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
                        this.pending_action = Some(PendingAction::OpenProjectAtPath(path));
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
    fn create_project(&mut self, source_path: PathBuf, cx: &mut Context<Self>) -> usize {
        let name = Project::name_from_path(&source_path);

        // Phase B: ensure the project is a git repo so session clones have
        // a base to anchor against. `git_init` is idempotent — a no-op on
        // existing repos — and non-fatal on failure.
        if let Err(e) = git::git_init(&source_path) {
            eprintln!(
                "git_init: {} failed: {e} (continuing without git integration)",
                source_path.display()
            );
        }

        let project = Project::new(name, source_path);
        self.projects.push(project);
        let idx = self.projects.len() - 1;
        self.save_settings();
        cx.notify();
        idx
    }

    /// Create a new session inside a project. Runs the APFS clone on a
    /// background task so the UI stays responsive. A "Cloning..." placeholder
    /// appears in the sidebar while the clone is in flight.
    fn add_session_to_project(
        &mut self,
        project_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(project_idx) else { return; };

        // Guard: if the source directory no longer exists (e.g. repo was
        // moved), prompt the user to relocate rather than failing mid-clone.
        if !project.source_path.exists() {
            eprintln!(
                "Project source path missing: {} — prompting for relocation",
                project.source_path.display()
            );
            self.pending_action = Some(PendingAction::RelocateProject(project_idx));
            cx.notify();
            return;
        }

        // If the working tree has uncommitted changes, prompt the user
        // before creating a session. The user can choose to proceed (the
        // dirty state will be present in the clone) or cancel to clean up.
        if git::is_working_tree_dirty(&project.source_path) && self.confirming_dirty_session.is_none() {
            self.confirming_dirty_session = Some(project_idx);
            cx.notify();
            return;
        }
        // Clear any prior dirty confirmation (user chose to proceed).
        self.confirming_dirty_session = None;

        let source_path = project.source_path.clone();
        let project_name = project.name.clone();
        let session_count = project.sessions.len() + project.loading_sessions.len() + 1;

        let session_id = uuid::Uuid::new_v4().to_string();
        let display_label = if self.claude_path.is_some() {
            format!("Claude {session_count}")
        } else {
            format!("Shell {session_count}")
        };

        // Build the claude command with --session-id + --name so our internal
        // UUID *is* Claude's session ID. This is what enables cold-resume later:
        // `claude --resume <same-uuid>` picks up the conversation in the same
        // clone path. --settings injects Allele's attention-routing
        // hooks so the Notification/Stop events flow back into the sidebar.
        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let command = self.claude_path.as_ref().map(|path| {
            let mut args = vec![
                "--session-id".to_string(),
                session_id.clone(),
                "--name".to_string(),
                display_label.clone(),
            ];
            if let Some(hooks) = hooks_path_str {
                args.push("--settings".to_string());
                args.push(hooks);
            }
            ShellCommand::with_args(path.clone(), args)
        });

        // Add a loading placeholder immediately so the user sees feedback
        project.loading_sessions.push(project::LoadingSession {
            id: session_id.clone(),
            label: display_label.clone(),
        });
        cx.notify();

        // Spawn the clone on a background task, then finish on the main thread
        let source_for_task = source_path.clone();
        let project_name_for_task = project_name.clone();
        // Two copies: one moves into the background clonefile closure (where
        // it's used as the short-ID source), the other is captured by the
        // main-thread update_in closure to set Session.id.
        let session_id_for_clone = session_id.clone();
        let session_id_for_session = session_id.clone();
        let display_label_for_task = display_label.clone();

        cx.spawn_in(window, async move |this, cx| {
            let clone_result = cx
                .background_executor()
                .spawn(async move {
                    clone::create_session_clone(
                        &source_for_task,
                        &project_name_for_task,
                        &session_id_for_clone,
                    )
                })
                .await;

            // Back on the main thread with window access
            let _ = this.update_in(cx, move |this: &mut Self, window, cx| {
                let clone_path = match clone_result {
                    Ok(p) => {
                        eprintln!("Created APFS clone at: {}", p.display());
                        p
                    }
                    Err(e) => {
                        eprintln!("Failed to create APFS clone: {e}");
                        source_path.clone()
                    }
                };

                let clone_succeeded = clone_path != source_path;

                // Purge stale runtime files (Overmind/Foreman sockets, server
                // pid files, etc.) that the parent left in the working tree —
                // clonefile(2) faithfully copied them. Must happen before any
                // drawer tab spawns its command.
                if clone_succeeded {
                    clone::cleanup_stale_runtime(
                        &clone_path,
                        &this.user_settings.session_cleanup_paths,
                    );
                }

                // Find the project again (indices may have shifted if user removed projects)
                let Some(project) = this.projects.get_mut(project_idx) else {
                    let _ = clone::delete_clone(&clone_path);
                    return;
                };

                // Remove the loading placeholder
                project.loading_sessions.retain(|l| l.id != session_id);

                // Create the session branch in the clone rooted at HEAD.
                // Only do this when clonefile succeeded — when we fell back
                // to source_path we must NOT mutate canonical's HEAD.
                if clone_succeeded {
                    if let Err(e) = git::create_session_branch(
                        &clone_path,
                        &session_id_for_session,
                    ) {
                        eprintln!(
                            "create_session_branch failed for {session_id_for_session}: {e}"
                        );
                    }
                }

                // Create the terminal view with the clone as PWD
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(window, cx, command, Some(clone_path.clone()))
                });

                // Subscribe to terminal events
                cx.subscribe(&terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
                    match event {
                        TerminalEvent::NewSession => {
                            this.pending_action = Some(PendingAction::NewSessionInActiveProject);
                            cx.notify();
                        }
                        TerminalEvent::CloseSession => {
                            this.pending_action = Some(PendingAction::CloseActiveSession);
                            cx.notify();
                        }
                        TerminalEvent::SwitchSession(target) => {
                            let target = *target;
                            let mut flat_idx = 0;
                            'outer: for (p_idx, project) in this.projects.iter().enumerate() {
                                for (s_idx, _) in project.sessions.iter().enumerate() {
                                    if flat_idx == target {
                                        this.active = Some(SessionCursor { project_idx: p_idx, session_idx: s_idx });
                                        this.pending_action = Some(PendingAction::FocusActive);
                                        cx.notify();
                                        break 'outer;
                                    }
                                    flat_idx += 1;
                                }
                            }
                        }
                        TerminalEvent::PrevSession => {
                            this.navigate_session(-1, cx);
                        }
                        TerminalEvent::NextSession => {
                            this.navigate_session(1, cx);
                        }
                        TerminalEvent::ToggleDrawer => {
                            this.pending_action = Some(PendingAction::ToggleDrawer);
                            cx.notify();
                        }
                        TerminalEvent::ToggleSidebar => {
                            this.pending_action = Some(PendingAction::ToggleSidebar);
                            cx.notify();
                        }
                        TerminalEvent::ToggleRightSidebar => {
                            this.pending_action = Some(PendingAction::ToggleRightSidebar);
                            cx.notify();
                        }
                        TerminalEvent::OpenScratchPad => {
                            this.pending_action = Some(PendingAction::OpenScratchPad);
                            cx.notify();
                        }
                    }
                }).detach();

                let session = Session::new_with_id(
                    session_id_for_session,
                    display_label_for_task,
                    terminal_view,
                )
                .with_clone(clone_path);
                let Some(project) = this.projects.get_mut(project_idx) else { return; };
                project.sessions.push(session);
                let session_idx = project.sessions.len() - 1;
                let cursor = SessionCursor { project_idx, session_idx };
                this.active = Some(cursor);
                this.apply_project_config(cursor, window, cx);
                this.save_state();
                cx.notify();
            });
        })
        .detach();
    }

    /// Close a session without deleting its clone.
    ///
    /// The PTY is killed (dropping the terminal_view entity triggers
    /// `PtyTerminal::drop` → `Msg::Shutdown`), the clone stays on disk,
    /// the session stays in `state.json` with status `Suspended`, and the
    /// sidebar row stays visible with a ⏸ icon. A later click on that row
    /// cold-resumes via `claude --resume <id>`.
    fn close_session_keep_clone(
        &mut self,
        cursor: SessionCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(cursor.project_idx) else { return; };
        let Some(session) = project.sessions.get_mut(cursor.session_idx) else { return; };

        // Drop the terminal_view and drawer — Drop impl on PtyTerminal sends
        // Msg::Shutdown, killing the subprocesses. The clone on disk is untouched.
        session.terminal_view = None;
        // Drop the drawer PTYs but preserve the names so the next open
        // restores the same tab layout (matches the rehydration path).
        let names: Vec<String> = session.drawer_tabs.iter().map(|t| t.name.clone()).collect();
        session.drawer_tabs.clear();
        session.pending_drawer_tab_names = names;
        session.drawer_visible = false;
        session.status = SessionStatus::Suspended;
        session.last_active = std::time::SystemTime::now();

        // If this was the active session, clear the active cursor — the main
        // area will show the "No active session" placeholder until the user
        // clicks something else.
        if self.active == Some(cursor) {
            self.active = None;
        }

        self.save_state();
        cx.notify();
    }

    /// Spawn one drawer terminal tab in the given session with an optional
    /// pre-chosen name and optional shell command. Default name is
    /// "Terminal N" where N is 1-based; default command drops into the
    /// user's shell.
    fn spawn_drawer_tab(
        &mut self,
        cursor: SessionCursor,
        name: Option<String>,
        command: Option<ShellCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let working_dir = self.projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .and_then(|s| s.clone_path.clone());
        let drawer_tv = cx.new(|cx| TerminalView::new(window, cx, command, working_dir));
        cx.subscribe(
            &drawer_tv,
            |this: &mut Self,
             _tv: Entity<TerminalView>,
             event: &TerminalEvent,
             cx: &mut Context<Self>| {
                if let TerminalEvent::ToggleDrawer = event {
                    this.pending_action = Some(PendingAction::ToggleDrawer);
                    cx.notify();
                }
            },
        )
        .detach();

        if let Some(session) = self.projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            let tab_name = name.unwrap_or_else(|| {
                format!("Terminal {}", session.drawer_tabs.len() + 1)
            });
            session.drawer_tabs.push(DrawerTab {
                view: drawer_tv,
                name: tab_name,
            });
        }
    }

    /// Materialise drawer tabs for a session that has none yet. Uses saved
    /// names from `pending_drawer_tab_names` if present, else creates one
    /// default "Terminal 1" tab.
    fn ensure_drawer_tabs(
        &mut self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (needs_default, pending) = {
            let session = self.projects
                .get(cursor.project_idx)
                .and_then(|p| p.sessions.get(cursor.session_idx));
            match session {
                Some(s) if !s.drawer_tabs.is_empty() => (false, Vec::new()),
                Some(s) => {
                    if s.pending_drawer_tab_names.is_empty() {
                        (true, Vec::new())
                    } else {
                        (false, s.pending_drawer_tab_names.clone())
                    }
                }
                None => return,
            }
        };

        if needs_default {
            self.spawn_drawer_tab(cursor, None, None, window, cx);
        } else if !pending.is_empty() {
            for name in pending {
                self.spawn_drawer_tab(cursor, Some(name), None, window, cx);
            }
            if let Some(session) = self.projects
                .get_mut(cursor.project_idx)
                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
            {
                session.pending_drawer_tab_names.clear();
                if session.drawer_active_tab >= session.drawer_tabs.len() {
                    session.drawer_active_tab = session.drawer_tabs.len().saturating_sub(1);
                }
            }
        }
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

        for term in &cfg.terminals {
            let substituted = config::substitute(&term.command, port, &clone_path);
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

        if let Some(preview) = cfg.preview {
            let url = config::substitute(&preview.url, port, &clone_path);
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
                // lands somewhere useful.
                if let Err(e) = std::process::Command::new("open").arg(&url).spawn() {
                    eprintln!("allele: failed to open preview URL {url}: {e}");
                }
            }
        }
    }

    /// Focus the currently active drawer tab's terminal view (if any).
    fn focus_active_drawer_tab(
        &self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
        {
            if let Some(tab) = session.drawer_tabs.get(session.drawer_active_tab) {
                let fh = tab.view.read(cx).focus_handle.clone();
                fh.focus(window, cx);
            }
        }
    }

    /// Apply a single hook event to the matching session.
    ///
    /// Transition rules:
    /// - `Notification` → `AwaitingInput` (permission prompt / idle wait)
    /// - `Stop` → `ResponseReady` (Claude finished a response turn)
    /// - `PreToolUse` / `PostToolUse` → `Running` (Claude is actively executing
    ///   a tool, which means any prior permission prompt has been resolved)
    /// - `UserPromptSubmit` → `Running` (user submitted new input)
    /// - `SessionStart` → `Running`
    /// - `SessionEnd` → `Done`
    ///
    /// Note: `Stop` no longer has special handling for `AwaitingInput`.
    /// In practice Claude doesn't emit `Stop` while still blocked on a
    /// prompt — `Stop` means the response turn completed, which implies
    /// any prompt was resolved. The earlier "don't stomp" rule was
    /// overly defensive and caused stuck AwaitingInput states in the wild.
    fn apply_hook_event(&mut self, event: hooks::HookEvent, cx: &mut Context<Self>) {
        // Find the matching session by its internal ID (= Claude session ID).
        let Some((p_idx, s_idx)) = self.projects.iter().enumerate().find_map(|(p_idx, p)| {
            p.sessions
                .iter()
                .position(|s| s.id == event.session_id)
                .map(|s_idx| (p_idx, s_idx))
        }) else {
            // Event for an unknown session — probably stale, drop it.
            eprintln!(
                "hook-event: no matching session for {:?} kind={:?}",
                event.session_id, event.kind
            );
            return;
        };

        let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        else {
            return;
        };

        let prior = session.status;
        let now = std::time::SystemTime::now();
        session.last_active = now;

        use hooks::HookKind;
        use session::SessionStatus;

        // --- Auto-naming: trigger on any event while label is a placeholder ---
        // Fires on the first hook event (usually SessionStart) to start polling
        // for the .prompt file. If that attempt times out (user hadn't typed yet),
        // a retry fires on UserPromptSubmit when the .prompt file is guaranteed
        // to exist.
        let is_placeholder = session.label.starts_with("Claude ")
            || session.label.starts_with("Shell ");
        let auto_name_data = if is_placeholder {
            if !session.auto_naming_fired {
                // First attempt — start polling for the prompt file.
                session.auto_naming_fired = true;
                eprintln!(
                    "auto-naming: triggered for {} label={:?} on {:?}",
                    session.id, session.label, event.kind
                );
                Some((session.id.clone(), session.clone_path.clone()))
            } else if matches!(event.kind, HookKind::UserPromptSubmit) {
                // Retry — first attempt likely timed out before user typed.
                // The .prompt file is guaranteed to exist now.
                eprintln!(
                    "auto-naming: retrying for {} on UserPromptSubmit (label still {:?})",
                    session.id, session.label
                );
                Some((session.id.clone(), session.clone_path.clone()))
            } else {
                None
            }
        } else {
            None
        };

        let new_status = match event.kind {
            HookKind::Notification => Some(SessionStatus::AwaitingInput),
            HookKind::Stop => Some(SessionStatus::ResponseReady),
            HookKind::PreToolUse | HookKind::PostToolUse => {
                // Tool execution is the key clearing signal. If Claude is
                // running a tool, any prior permission prompt has been
                // resolved and we should be back in Running. If we were
                // already Running, this is a no-op (the prior==new guard
                // below drops it).
                Some(SessionStatus::Running)
            }
            HookKind::UserPromptSubmit => Some(SessionStatus::Running),
            HookKind::SessionStart => Some(SessionStatus::Running),
            HookKind::SessionEnd => Some(SessionStatus::Done),
            HookKind::Other => None,
        };

        let Some(new_status) = new_status else {
            // No status change, but still trigger auto-naming if applicable.
            if let Some((session_id, clone_path)) = auto_name_data {
                eprintln!("auto-naming: trigger fired for session {session_id}");
                self.trigger_auto_naming(session_id, clone_path, cx);
            }
            return;
        };
        if new_status == prior {
            // No status transition, but still trigger auto-naming if applicable.
            if let Some((session_id, clone_path)) = auto_name_data {
                eprintln!("auto-naming: trigger fired for session {session_id}");
                self.trigger_auto_naming(session_id, clone_path, cx);
            }
            return;
        }

        session.status = new_status;

        // Capture the label for notifications BEFORE we drop the borrow.
        let session_label = session.label.clone();
        let project_name = self
            .projects
            .get(p_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        // Persist the updated status.
        self.save_state();

        // Fire sound + notification affordances — ONLY on transitions INTO
        // an attention state, never on transitions out of one.
        match new_status {
            SessionStatus::AwaitingInput => {
                if self.user_settings.sound_on_awaiting_input {
                    let sound_path = self
                        .user_settings
                        .awaiting_input_sound_path
                        .clone()
                        .unwrap_or_else(|| settings::DEFAULT_AWAITING_INPUT_SOUND.to_string());
                    hooks::play_sound(&sound_path);
                }
                if self.user_settings.notifications_enabled {
                    hooks::show_notification(
                        &format!("{project_name} — needs input"),
                        &format!("{session_label} is blocked and waiting for you"),
                    );
                }
            }
            SessionStatus::ResponseReady => {
                if self.user_settings.sound_on_response_ready {
                    let sound_path = self
                        .user_settings
                        .response_ready_sound_path
                        .clone()
                        .unwrap_or_else(|| settings::DEFAULT_RESPONSE_READY_SOUND.to_string());
                    hooks::play_sound(&sound_path);
                }
                if self.user_settings.notifications_enabled {
                    hooks::show_notification(
                        &format!("{project_name} — response ready"),
                        &format!("{session_label} finished responding"),
                    );
                }
            }
            _ => {}
        }

        cx.notify();

        // Trigger auto-naming after all borrows are released.
        if let Some((session_id, clone_path)) = auto_name_data {
            eprintln!("auto-naming: trigger fired for session {session_id}");
            self.trigger_auto_naming(session_id, clone_path, cx);
        }
    }

    /// Spawn a background task that reads the first prompt from the hook
    /// events directory, extracts keywords to produce a 3-5 word slug, then
    /// updates the session label and renames the git branch.
    /// No external dependencies — pure Rust keyword extraction.
    fn trigger_auto_naming(
        &self,
        session_id: String,
        clone_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(events_dir) = hooks::events_dir() else { return; };

        cx.spawn(async move |this, cx| {
            // Read the .prompt file (written by the hook receiver on the first
            // UserPromptSubmit). Since auto-naming fires on the first hook event
            // (often session_start, before the user types), we poll generously:
            // 120 attempts × 2s = 4 minutes. The extraction itself is instant
            // so there's no cost to waiting longer.
            let prompt_path = events_dir.join(format!("{session_id}.prompt"));
            let mut prompt_text = None;
            for attempt in 0..120 {
                if let Ok(text) = std::fs::read_to_string(&prompt_path) {
                    if !text.trim().is_empty() {
                        prompt_text = Some(text);
                        break;
                    }
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(2000))
                    .await;
                if attempt == 0 {
                    eprintln!("auto-naming: waiting for prompt file for {session_id}");
                }
            }

            let Some(prompt) = prompt_text else {
                eprintln!("auto-naming: no prompt file found after 4min for {session_id}");
                return;
            };
            eprintln!(
                "auto-naming: prompt file read for {session_id} ({} chars)",
                prompt.len()
            );

            // Extract keywords — pure Rust, no LLM needed.
            let slug_raw = git::extract_slug_from_prompt(&prompt, 4);

            eprintln!("auto-naming: extracted slug_raw={slug_raw:?} for {session_id}");
            if slug_raw.is_empty() {
                eprintln!("auto-naming: empty slug from keyword extraction");
                return;
            }

            let slug = git::slugify(&slug_raw, 50);
            if slug.is_empty() {
                return;
            }

            // Human-readable label: replace hyphens with spaces, title case,
            // capped at 40 chars for sidebar display.
            let full_label: String = slug
                .split('-')
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(c) => {
                            let upper: String = c.to_uppercase().collect();
                            format!("{upper}{}", chars.as_str())
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let display_label = if full_label.len() > 40 {
                let mut truncated = full_label[..40].to_string();
                // Avoid cutting mid-word — trim back to last space.
                if let Some(last_space) = truncated.rfind(' ') {
                    truncated.truncate(last_space);
                }
                truncated
            } else {
                full_label
            };

            // Rename the git branch in the background (non-blocking).
            if let Some(ref cp) = clone_path {
                eprintln!("auto-naming: renaming branch for {session_id} with slug={slug:?}");
                if let Err(e) = git::rename_session_branch(cp, &session_id, &slug) {
                    eprintln!("auto-naming: branch rename failed: {e}");
                    // Continue — label update is still valuable
                } else {
                    eprintln!("auto-naming: branch rename succeeded for {session_id}");
                }
            }

            // Update session label on the main thread.
            eprintln!("auto-naming: updating label to {display_label:?} for {session_id}");
            let _ = this.update(cx, |this: &mut AppState, cx| {
                for project in &mut this.projects {
                    for session in &mut project.sessions {
                        if session.id == session_id {
                            eprintln!(
                                "auto-naming: label updated {:?} -> {:?} for {session_id}",
                                session.label, display_label
                            );
                            session.label = display_label.clone();
                            break;
                        }
                    }
                }
                this.save_state();
                cx.notify();
            });
        })
        .detach();
    }

    /// Cycle the active session pointer across all non-Suspended sessions
    /// in the flat order they appear in the sidebar. `delta = -1` = previous,
    /// `delta = 1` = next. Wraps at both ends. Suspended sessions are
    /// deliberately skipped — quick-flicking shouldn't auto-spawn resumed
    /// Claude processes; the user clicks the ⏸ row explicitly to resume.
    fn navigate_session(&mut self, delta: i32, cx: &mut Context<Self>) {
        // Build the flat list of (project_idx, session_idx) for every
        // attached (non-Suspended) session. This is the nav surface.
        let flat: Vec<SessionCursor> = self
            .projects
            .iter()
            .enumerate()
            .flat_map(|(p_idx, project)| {
                project
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.status != SessionStatus::Suspended)
                    .map(move |(s_idx, _)| SessionCursor {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    })
            })
            .collect();

        if flat.is_empty() {
            return;
        }

        // Find the active cursor's position in the flat list. If the current
        // active is None or points at a Suspended session (not in `flat`),
        // treat it as an implicit position before index 0 when moving forward,
        // and after the last index when moving backward.
        let current_pos = self
            .active
            .and_then(|active| flat.iter().position(|c| *c == active));

        let len = flat.len() as i32;
        let new_pos = match current_pos {
            Some(pos) => (pos as i32 + delta).rem_euclid(len) as usize,
            None if delta >= 0 => 0,
            None => (len - 1) as usize,
        };

        self.active = Some(flat[new_pos]);
        self.pending_action = Some(PendingAction::FocusActive);
        cx.notify();
    }

    /// Resume a Suspended session by spawning a fresh PTY with
    /// `claude --resume <id>` inside the stored clone_path.
    ///
    /// The session retains its original `id` — Claude picks up the
    /// conversation from its jsonl history.
    fn resume_session(
        &mut self,
        cursor: SessionCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get(cursor.project_idx) else { return; };
        let Some(session) = project.sessions.get(cursor.session_idx) else { return; };
        let Some(clone_path) = session.clone_path.clone() else {
            eprintln!(
                "Cannot resume session {} — no clone_path on record",
                session.id
            );
            return;
        };

        if !clone_path.exists() {
            eprintln!(
                "Cannot resume session {} — clone_path is missing on disk: {}",
                session.id,
                clone_path.display()
            );
            return;
        }

        let session_id = session.id.clone();
        let label = session.label.clone();

        // Pick the resume flag based on whether Claude actually has history
        // for this session ID on disk. Claude stores conversations under
        // `~/.claude/projects/<slug>/<id>.jsonl`. If no jsonl for this id
        // exists anywhere, `claude --resume <id>` bails out immediately,
        // which used to flip the session to Done. Fall back to
        // `--session-id <id>` in that case — same UUID, empty history,
        // still a continuable session.
        let has_history = claude_session_history_exists(&session_id);
        let hooks_path_str = self
            .hooks_settings_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let command = self.claude_path.as_ref().map(|path| {
            let mut args = if has_history {
                vec!["--resume".to_string(), session_id.clone()]
            } else {
                vec!["--session-id".to_string(), session_id.clone()]
            };
            args.push("--name".to_string());
            args.push(label.clone());
            if let Some(hooks) = hooks_path_str {
                args.push("--settings".to_string());
                args.push(hooks);
            }
            ShellCommand::with_args(path.clone(), args)
        });

        // Build the new TerminalView on the main thread with window access.
        let terminal_view = cx.new(|cx| {
            TerminalView::new(window, cx, command, Some(clone_path.clone()))
        });

        // Subscribe to terminal events so the resumed session wires up the
        // same shortcut actions (NewSession, CloseSession, SwitchSession)
        // as freshly-created ones.
        cx.subscribe(&terminal_view, |this: &mut Self, _tv: Entity<TerminalView>, event: &TerminalEvent, cx: &mut Context<Self>| {
            match event {
                TerminalEvent::NewSession => {
                    this.pending_action = Some(PendingAction::NewSessionInActiveProject);
                    cx.notify();
                }
                TerminalEvent::CloseSession => {
                    this.pending_action = Some(PendingAction::CloseActiveSession);
                    cx.notify();
                }
                TerminalEvent::SwitchSession(target) => {
                    // Mirror the fresh-spawn handler so Cmd+1..9 also works
                    // from resumed sessions.
                    let target = *target;
                    let mut flat_idx = 0;
                    'outer: for (p_idx, project) in this.projects.iter().enumerate() {
                        for (s_idx, _) in project.sessions.iter().enumerate() {
                            if flat_idx == target {
                                this.active = Some(SessionCursor { project_idx: p_idx, session_idx: s_idx });
                                this.pending_action = Some(PendingAction::FocusActive);
                                cx.notify();
                                break 'outer;
                            }
                            flat_idx += 1;
                        }
                    }
                }
                TerminalEvent::PrevSession => {
                    this.navigate_session(-1, cx);
                }
                TerminalEvent::NextSession => {
                    this.navigate_session(1, cx);
                }
                TerminalEvent::ToggleDrawer => {
                    this.pending_action = Some(PendingAction::ToggleDrawer);
                    cx.notify();
                }
                TerminalEvent::ToggleSidebar => {
                    this.pending_action = Some(PendingAction::ToggleSidebar);
                    cx.notify();
                }
                TerminalEvent::ToggleRightSidebar => {
                    this.pending_action = Some(PendingAction::ToggleRightSidebar);
                    cx.notify();
                }
                TerminalEvent::OpenScratchPad => {
                    this.pending_action = Some(PendingAction::OpenScratchPad);
                    cx.notify();
                }
            }
        }).detach();

        // Attach the new PTY to the existing session entry.
        if let Some(session) = self
            .projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
        {
            session.terminal_view = Some(terminal_view);
            session.status = SessionStatus::Running;
            session.last_active = std::time::SystemTime::now();
            // Grace window: if the PTY exits in the next 3s, the exit
            // watcher reverts to Suspended instead of flipping to Done.
            // Protects against `claude --resume` exiting immediately.
            session.resuming_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
            self.active = Some(cursor);
            self.pending_action = Some(PendingAction::FocusActive);
        }

        self.apply_project_config(cursor, window, cx);
        self.save_state();
        cx.notify();
    }

    /// Discard a session — kill the PTY, delete the APFS clone, remove from
    /// the sidebar, and drop the corresponding entry from `state.json`.
    ///
    /// This is the *destructive* path, reached only through the explicit
    /// Discard action with confirmation. The plain Close action uses
    /// `close_session_keep_clone` instead.
    fn remove_session(
        &mut self,
        cursor: SessionCursor,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.projects.get_mut(cursor.project_idx) else { return; };
        if cursor.session_idx >= project.sessions.len() { return; }

        // Pull the session out of the list immediately
        let removed = project.sessions.remove(cursor.session_idx);
        let clone_path = removed.clone_path.clone();
        let removed_label = removed.label.clone();
        let already_merged = removed.merged;
        let removed_session_id = removed.id.clone();
        let removed_browser_tab_id = removed.browser_tab_id;
        // Captured before drop(removed) / end of &mut project borrow.
        let canonical_for_task = project.source_path.clone();
        let session_id_for_task = removed.id.clone();

        // Preserve the session's metadata in the archive list so the
        // sidebar archive browser can show a human-readable label —
        // but skip this if the session was already merged (work is in canonical).
        if !already_merged {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            project.archives.push(ArchivedSession {
                id: removed.id.clone(),
                project_id: project.id.clone(),
                label: removed_label.clone(),
                archived_at: now,
            });
        }

        // Drop the Session — this frees the terminal_view entity (if any),
        // which in turn kills the PTY via the Drop impl on PtyTerminal.
        // Suspended sessions have `terminal_view = None` so there's no PTY
        // to kill; only the clone needs cleanup.
        drop(removed);

        // Close the linked Chrome tab (best-effort; ignored if Chrome is
        // not running or the tab id is stale). Only when integration is on.
        if self.user_settings.browser_integration_enabled {
            if let Some(id) = removed_browser_tab_id {
                let _ = browser::close_tab(id);
            }
        }
        let _ = removed_session_id; // reserved for future use

        // Show an "Archiving…" placeholder if there's a clone to clean up
        let placeholder_id = uuid::Uuid::new_v4().to_string();
        if clone_path.is_some() {
            project.loading_sessions.push(project::LoadingSession {
                id: placeholder_id.clone(),
                label: format!("{removed_label} (archiving)"),
            });
        }

        // If the removed session was the active one, clear active selection
        // (so the main content area shows the empty state immediately).
        if let Some(active) = self.active {
            if active == cursor {
                // Try to pick another session in the same project first
                let project = &self.projects[cursor.project_idx];
                self.active = if !project.sessions.is_empty() {
                    let new_session_idx = cursor.session_idx.min(project.sessions.len() - 1);
                    Some(SessionCursor { project_idx: cursor.project_idx, session_idx: new_session_idx })
                } else {
                    // Fall back to any session in any project
                    self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                        if !p.sessions.is_empty() {
                            Some(SessionCursor { project_idx: p_idx, session_idx: 0 })
                        } else {
                            None
                        }
                    })
                };
            } else if active.project_idx == cursor.project_idx && active.session_idx > cursor.session_idx {
                // Active session in same project shifted down by one
                self.active = Some(SessionCursor {
                    project_idx: active.project_idx,
                    session_idx: active.session_idx - 1,
                });
            }
        }

        // Persist the updated session list now that the entry is gone.
        self.save_state();
        cx.notify();

        // Spawn the archive-then-delete pipeline on a background task
        if let Some(clone_path) = clone_path {
            let project_idx = cursor.project_idx;
            let placeholder_id_for_task = placeholder_id.clone();
            cx.spawn(async move |this, cx| {
                let delete_result = cx
                    .background_executor()
                    .spawn(async move {
                        // Degenerate case: if the session's "clone path"
                        // is canonical itself (Phase C fallback when the
                        // clonefile syscall failed), skip the archive
                        // pipeline — no session branch exists, the fetch
                        // would be a no-op self-fetch, and trash_clone
                        // will bail on the workspace-dir safety check.
                        if clone_path == canonical_for_task {
                            return clone::delete_clone(&clone_path);
                        }
                        // Archive the session's work into canonical
                        // before the clone is trashed. Order is
                        // load-bearing — archive_session must run while
                        // the clone still exists.
                        if let Err(e) = git::archive_session(
                            &canonical_for_task,
                            &clone_path,
                            &session_id_for_task,
                        ) {
                            eprintln!(
                                "archive_session failed for {session_id_for_task}: {e}"
                            );
                        }
                        clone::trash_clone(&clone_path).map(|_| ())
                    })
                    .await;

                if let Err(e) = delete_result {
                    eprintln!("Failed to delete clone: {e}");
                }

                // Remove the placeholder on the main thread
                let _ = this.update(cx, |this: &mut Self, cx| {
                    if let Some(project) = this.projects.get_mut(project_idx) {
                        project.loading_sessions.retain(|l| l.id != placeholder_id_for_task);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Remove a project and all its sessions (deleting all clones asynchronously).
    fn remove_project(&mut self, project_idx: usize, _window: &mut Window, cx: &mut Context<Self>) {
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

        self.save_settings();
        self.save_state();
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
                                eprintln!("Failed to trash clone at {}: {e}", path.display());
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

            eprintln!("\n*** allele crashed ***");
            eprintln!("{entry}");
            eprintln!("Crash log: {}", log_path.display());
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

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-b", ToggleSidebarAction, None),
        KeyBinding::new("cmd-j", ToggleDrawerAction, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-k", OpenScratchPadAction, None),
    ]);

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

        let keys: [id; 4] = [
            NSString::alloc(nil).init_str("ApplicationName"),
            NSString::alloc(nil).init_str("ApplicationVersion"),
            NSString::alloc(nil).init_str("Copyright"),
            NSString::alloc(nil).init_str("Credits"),
        ];
        let vals: [id; 4] = [name, version, copyright, credits];
        let options: id = msg_send![
            class!(NSDictionary),
            dictionaryWithObjects: vals.as_ptr()
            forKeys: keys.as_ptr()
            count: 4usize
        ];

        let app = NSApp();
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
        let _: () = msg_send![app, orderFrontStandardAboutPanelWithOptions: options];
    }
}

fn main() {
    install_panic_hook();

    // Hard dependency check: Allele treats git as non-optional. Fail
    // loudly before any window opens if it's missing.
    if !git::git_available() {
        const MSG: &str = "Allele requires git but none was found on PATH.\n\n\
                           Install the Xcode Command Line Tools with:\n\n    xcode-select --install";
        eprintln!("{MSG}");
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
        eprintln!(
            "Loaded settings: sidebar_width={}, font_size={}",
            loaded_settings.sidebar_width, loaded_settings.font_size
        );

        // Load persisted session state (may be empty on first run).
        let loaded_state = PersistedState::load();
        eprintln!("Loaded persisted state: {} sessions", loaded_state.sessions.len());

        // Install the Allele hook receiver and settings file so every
        // claude spawn can route attention signals back into the UI. Failure
        // is non-fatal — the app still runs, it just won't get hook events.
        let hooks_settings_path: Option<PathBuf> = match hooks::install_if_missing() {
            Ok(path) => {
                eprintln!("Installed Allele hooks at {}", path.display());
                Some(path)
            }
            Err(e) => {
                eprintln!("Failed to install Allele hooks: {e} (attention routing disabled)");
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
                Ok(n) => eprintln!("Orphan sweep trashed {n} unreferenced clone(s)"),
                Err(e) => eprintln!("Orphan sweep failed: {e}"),
            }
            match clone::purge_trash_older_than_days(clone::TRASH_TTL_DAYS) {
                Ok(0) => {}
                Ok(n) => eprintln!("Trash purge removed {n} expired entry/entries"),
                Err(e) => eprintln!("Trash purge failed: {e}"),
            }
            // Prune archive refs older than the trash TTL so they don't
            // accumulate indefinitely in canonical repos.
            for source_path in &project_paths_for_prune {
                if let Err(e) = git::prune_archive_refs(source_path, clone::TRASH_TTL_DAYS) {
                    eprintln!(
                        "prune_archive_refs failed for {}: {e}",
                        source_path.display()
                    );
                }
            }
        });

        let claude_path = PtyTerminal::find_claude()
            .map(|p| p.to_string_lossy().to_string());

        if let Some(ref path) = claude_path {
            eprintln!("Found Claude Code at: {path}");
        } else {
            eprintln!("Claude Code not found — falling back to default shell");
        }

        let claude_path_clone = claude_path.clone();

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
                            sidebar_width: this.sidebar_width,
                            font_size: 13.0,
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
                        settings.save();
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
                            eprintln!(
                                "Dropping persisted session {} — owning project {} is gone",
                                persisted.id, persisted.project_id
                            );
                            continue;
                        };

                        let session = Session::suspended_from_persisted(
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
                        );
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
                                        state.confirming_quit = true;
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
                                    this.pending_action = Some(PendingAction::ToggleSidebar);
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
                                    this.pending_action = Some(PendingAction::ToggleDrawer);
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
                                    this.pending_action = Some(PendingAction::OpenScratchPad);
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
                            let (existing, paths, external_editor, browser_integration) = strong.update(cx, |state: &mut AppState, _cx| {
                                (
                                    state.settings_window,
                                    state.user_settings.session_cleanup_paths.clone(),
                                    state
                                        .user_settings
                                        .external_editor_command
                                        .clone()
                                        .unwrap_or_default(),
                                    state.user_settings.browser_integration_enabled,
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
                            match settings_window::open_settings_window(cx, weak, paths, external_editor, browser_integration) {
                                Ok(new_handle) => {
                                    strong
                                        .update(cx, |state: &mut AppState, _cx| {
                                            state.settings_window = Some(new_handle);
                                        });
                                }
                                Err(e) => {
                                    eprintln!("Failed to open settings window: {e}");
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
                                            Some(PendingAction::ResumeSession {
                                                project_idx: p_idx,
                                                session_idx: s_idx,
                                            })
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

                    AppState {
                        projects,
                        active: initial_active,
                        claude_path: claude_path_clone,
                        pending_action: initial_pending,
                        sidebar_visible: settings_for_window.sidebar_visible,
                        sidebar_width: settings_for_window.sidebar_width
                            .max(SIDEBAR_MIN_WIDTH),
                        sidebar_resizing: false,
                        confirming_discard: None,
                        confirming_dirty_session: None,
                        hooks_settings_path: hooks_settings_path_for_window,
                        drawer_height: settings_for_window.drawer_height
                            .max(DRAWER_MIN_HEIGHT),
                        drawer_resizing: false,
                        drawer_rename: None,
                        drawer_rename_focus: None,
                        right_sidebar_visible: settings_for_window.right_sidebar_visible,
                        right_sidebar_width: settings_for_window.right_sidebar_width
                            .max(RIGHT_SIDEBAR_MIN_WIDTH),
                        right_sidebar_resizing: false,
                        confirming_quit: false,
                        editing_project_settings: None,
                        user_settings: settings_for_window.clone(),
                        settings_window: None,
                        main_tab: MainTab::Claude,
                        editor_selected_path: None,
                        editor_expanded_dirs: HashSet::new(),
                        editor_preview: None,
                        editor_context_menu: None,
                        browser_status: String::new(),
                        scratch_pad: None,
                    }
                })
            },
        )
        .unwrap();
    });
}

impl Render for AppState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process pending actions
        if let Some(action) = self.pending_action.take() {
            let mut skip_refocus = false;
            match action {
                PendingAction::NewSessionInActiveProject => {
                    if let Some(active) = self.active {
                        self.add_session_to_project(active.project_idx, window, cx);
                    }
                }
                PendingAction::CloseActiveSession => {
                    // Keyboard/menu "close" — preserve the clone so the user
                    // can cold-resume later. Discard is an explicit gesture only.
                    if let Some(active) = self.active {
                        self.close_session_keep_clone(active, window, cx);
                    }
                }
                PendingAction::FocusActive => {
                    if let Some(session) = self.active_session() {
                        if let Some(tv) = session.terminal_view.as_ref() {
                            let fh = tv.read(cx).focus_handle.clone();
                            fh.focus(window, cx);
                        }
                    }
                }
                PendingAction::OpenProjectAtPath(path) => {
                    let idx = self.create_project(path, cx);
                    // Auto-create first session for the new project
                    self.add_session_to_project(idx, window, cx);
                }
                PendingAction::AddSessionToProject(project_idx) => {
                    self.add_session_to_project(project_idx, window, cx);
                }
                PendingAction::RemoveProject(project_idx) => {
                    self.remove_project(project_idx, window, cx);
                }
                PendingAction::CloseSessionKeepClone { project_idx, session_idx } => {
                    self.close_session_keep_clone(
                        SessionCursor { project_idx, session_idx },
                        window,
                        cx,
                    );
                }
                PendingAction::RequestDiscardSession { project_idx, session_idx } => {
                    // Arm the inline confirmation gate. The sidebar row will
                    // render Confirm/Cancel buttons on the next frame.
                    self.confirming_discard = Some(SessionCursor { project_idx, session_idx });
                    cx.notify();
                }
                PendingAction::CancelDiscard => {
                    self.confirming_discard = None;
                    cx.notify();
                }
                PendingAction::DiscardSession { project_idx, session_idx } => {
                    self.confirming_discard = None;
                    self.remove_session(
                        SessionCursor { project_idx, session_idx },
                        window,
                        cx,
                    );
                }
                PendingAction::MergeArchive { project_idx, archive_idx } => {
                    if let Some(project) = self.projects.get_mut(project_idx) {
                        if let Some(entry) = project.archives.get(archive_idx) {
                            let session_id = entry.id.clone();
                            let merge_result = match project.settings.merge_strategy {
                                crate::settings::MergeStrategy::Merge => {
                                    git::merge_archive(&project.source_path, &session_id)
                                }
                                crate::settings::MergeStrategy::Squash => {
                                    git::squash_merge_archive(&project.source_path, &session_id)
                                }
                                crate::settings::MergeStrategy::RebaseThenMerge => {
                                    git::rebase_merge_archive(&project.source_path, &session_id)
                                }
                            };
                            match merge_result {
                                Ok(git::MergeResult::Merged) => {
                                    let _ = git::delete_ref(
                                        &project.source_path,
                                        &git::archive_ref_name(&session_id),
                                    );
                                    project.archives.remove(archive_idx);
                                    eprintln!("Merged archive {session_id} into canonical");
                                }
                                Ok(git::MergeResult::AlreadyUpToDate) => {
                                    let _ = git::delete_ref(
                                        &project.source_path,
                                        &git::archive_ref_name(&session_id),
                                    );
                                    project.archives.remove(archive_idx);
                                    eprintln!(
                                        "Archive {session_id} had no new commits — nothing to merge (already up to date)"
                                    );
                                }
                                Err(e) => {
                                    eprintln!(
                                        "merge_archive failed for {session_id}: {e}"
                                    );
                                }
                            }
                        }
                    }
                    self.save_state();
                    cx.notify();
                }
                PendingAction::DeleteArchive { project_idx, archive_idx } => {
                    if let Some(project) = self.projects.get_mut(project_idx) {
                        if let Some(entry) = project.archives.get(archive_idx) {
                            let session_id = entry.id.clone();
                            let _ = git::delete_ref(
                                &project.source_path,
                                &git::archive_ref_name(&session_id),
                            );
                            project.archives.remove(archive_idx);
                            eprintln!("Deleted archive ref for {session_id}");
                        }
                    }
                    self.save_state();
                    cx.notify();
                }
                PendingAction::MergeAndClose { project_idx, session_idx } => {
                    let cursor = SessionCursor { project_idx, session_idx };
                    if let Some(project) = self.projects.get_mut(cursor.project_idx) {
                        if cursor.session_idx < project.sessions.len() {
                            let session = &mut project.sessions[cursor.session_idx];
                            let clone_path = session.clone_path.clone();
                            let session_id = session.id.clone();
                            let session_label = session.label.clone();
                            let canonical = project.source_path.clone();
                            let proj_settings = project.settings.clone();

                            // Capture session metadata for potential restoration on failure
                            // (must happen before the mutable borrow for loading_sessions).
                            let restore_started = session.started_at;
                            let restore_last_active = session.last_active;

                            // If no clone or clone == canonical, just remove (no git ops).
                            let needs_git = clone_path.as_ref().map_or(false, |cp| *cp != canonical);

                            if needs_git {
                                let clone_path = clone_path.unwrap(); // safe: needs_git is true
                                let restore_clone = clone_path.clone();

                                // Show a placeholder while the background pipeline runs.
                                let placeholder_id = uuid::Uuid::new_v4().to_string();
                                {
                                    let project = self.projects.get_mut(cursor.project_idx).unwrap();
                                    project.loading_sessions.push(project::LoadingSession {
                                        id: placeholder_id.clone(),
                                        label: format!("{session_label} (rebasing & merging)"),
                                    });

                                    // Remove session from the list (frees PTY via Drop).
                                    // We DON'T call remove_session() because its background
                                    // task would delete the clone — we need the clone alive
                                    // until we know the merge succeeded.
                                    project.sessions.remove(cursor.session_idx);
                                }

                                // Update active cursor if it pointed at the removed session.
                                if let Some(active) = self.active {
                                    if active == cursor {
                                        let project = &self.projects[cursor.project_idx];
                                        self.active = if !project.sessions.is_empty() {
                                            let new_idx = cursor.session_idx.min(project.sessions.len() - 1);
                                            Some(SessionCursor { project_idx: cursor.project_idx, session_idx: new_idx })
                                        } else {
                                            self.projects.iter().enumerate().find_map(|(p_idx, p)| {
                                                if !p.sessions.is_empty() {
                                                    Some(SessionCursor { project_idx: p_idx, session_idx: 0 })
                                                } else {
                                                    None
                                                }
                                            })
                                        };
                                    } else if active.project_idx == cursor.project_idx && active.session_idx > cursor.session_idx {
                                        self.active = Some(SessionCursor {
                                            project_idx: active.project_idx,
                                            session_idx: active.session_idx - 1,
                                        });
                                    }
                                }
                                self.save_state();
                                cx.notify();

                                // Clones for restoration on failure (originals move into the background task).
                                let restore_id = session_id.clone();
                                let restore_label = session_label.clone();

                                // Spawn the archive → rebase → merge → delete pipeline on the background executor.
                                let placeholder_id_for_task = placeholder_id.clone();
                                let project_idx_for_task = cursor.project_idx;
                                cx.spawn(async move |this, cx| {
                                    let result = cx
                                        .background_executor()
                                        .spawn(async move {
                                            // 1. Auto-commit + fetch session branch as archive ref
                                            git::archive_session(&canonical, &clone_path, &session_id)?;

                                            // 2. Optionally fetch remote & rebase canonical onto remote tip
                                            let remote = proj_settings.resolved_remote();
                                            if proj_settings.rebase_before_merge && git::has_remote(&canonical, remote) {
                                                let branch_override = proj_settings.default_branch.as_deref();
                                                if let Err(e) = git::fetch_and_rebase_onto_remote_branch(&canonical, remote, branch_override) {
                                                    eprintln!("Rebase onto {remote} failed for {session_id}: {e}");
                                                    // Clean up the archive ref only — preserve the clone
                                                    let _ = git::delete_ref(
                                                        &canonical,
                                                        &git::archive_ref_name(&session_id),
                                                    );
                                                    anyhow::bail!("Rebase failed — resolve conflicts in the session and merge again. {e}");
                                                }
                                                eprintln!("Rebased canonical onto {remote} for {session_id}");
                                            }

                                            // 3. Merge the archive ref using the configured strategy
                                            let merge_result = match proj_settings.merge_strategy {
                                                crate::settings::MergeStrategy::Merge => {
                                                    git::merge_archive(&canonical, &session_id)
                                                }
                                                crate::settings::MergeStrategy::Squash => {
                                                    git::squash_merge_archive(&canonical, &session_id)
                                                }
                                                crate::settings::MergeStrategy::RebaseThenMerge => {
                                                    git::rebase_merge_archive(&canonical, &session_id)
                                                }
                                            };

                                            // 4. Always delete the archive ref (cleanup even on merge failure)
                                            let _ = git::delete_ref(
                                                &canonical,
                                                &git::archive_ref_name(&session_id),
                                            );

                                            match merge_result {
                                                Ok(git::MergeResult::Merged) => {
                                                    eprintln!("Merged session {session_id} into canonical");
                                                }
                                                Ok(git::MergeResult::AlreadyUpToDate) => {
                                                    eprintln!("Session {session_id} already up to date — nothing to merge");
                                                }
                                                Err(e) => {
                                                    eprintln!("merge_archive failed for {session_id}: {e}");
                                                    // Preserve clone — don't delete it on merge failure
                                                    anyhow::bail!("Merge failed — resolve conflicts in the session and merge again. {e}");
                                                }
                                            }

                                            // 5. Trash the APFS clone (near-instant rename) on
                                            //    success. Actual deletion deferred to startup purge.
                                            if let Err(e) = clone::trash_clone(&clone_path) {
                                                eprintln!("Failed to trash clone after merge for {session_id}: {e}");
                                            }
                                            Ok(())
                                        })
                                        .await;

                                    // Update UI on the main thread
                                    let _ = this.update(cx, |this: &mut Self, cx| {
                                        if let Some(project) = this.projects.get_mut(project_idx_for_task) {
                                            project.loading_sessions.retain(|l| l.id != placeholder_id_for_task);
                                        }

                                        if let Err(e) = &result {
                                            eprintln!("Merge-and-close pipeline error: {e}");

                                            // Restore the session so the user can fix conflicts and retry
                                            let restored = Session::suspended_from_persisted(
                                                restore_id.clone(),
                                                restore_label.clone(),
                                                restore_started,
                                                restore_last_active,
                                                Some(restore_clone.clone()),
                                                false, // not merged — that's the point
                                            );
                                            if let Some(project) = this.projects.get_mut(project_idx_for_task) {
                                                project.sessions.push(restored);
                                            }

                                            hooks::show_notification(
                                                "Merge failed",
                                                &format!("{restore_label}: resolve conflicts and merge again"),
                                            );
                                        }

                                        this.save_state();
                                        cx.notify();
                                    });
                                })
                                .detach();
                            } else {
                                // No clone to manage — mark merged so remove_session
                                // skips creating an archive entry.
                                if let Some(project) = self.projects.get_mut(cursor.project_idx) {
                                    if cursor.session_idx < project.sessions.len() {
                                        project.sessions[cursor.session_idx].merged = true;
                                    }
                                }
                                self.remove_session(cursor, window, cx);
                            }
                        }
                    }
                }
                PendingAction::SelectSession { project_idx, session_idx } => {
                    let cursor = SessionCursor { project_idx, session_idx };
                    // Clicking a Suspended session cold-resumes it; clicking
                    // any other session just makes it the active one.
                    let is_suspended = self
                        .projects
                        .get(project_idx)
                        .and_then(|p| p.sessions.get(session_idx))
                        .map(|s| s.status == SessionStatus::Suspended)
                        .unwrap_or(false);

                    if is_suspended {
                        self.resume_session(cursor, window, cx);
                    } else {
                        self.active = Some(cursor);
                        if let Some(session) = self.active_session() {
                            if let Some(tv) = session.terminal_view.as_ref() {
                                let fh = tv.read(cx).focus_handle.clone();
                                fh.focus(window, cx);
                            }
                        }
                    }
                    // Keep Chrome's active tab aligned with the active session.
                    self.sync_browser_to_active();
                }
                PendingAction::ToggleDrawer => {
                    skip_refocus = true;
                    if let Some(cursor) = self.active {
                        let now_visible = {
                            let session = self.projects
                                .get_mut(cursor.project_idx)
                                .and_then(|p| p.sessions.get_mut(cursor.session_idx));
                            if let Some(s) = session {
                                s.drawer_visible = !s.drawer_visible;
                                s.drawer_visible
                            } else {
                                false
                            }
                        };
                        if now_visible {
                            self.ensure_drawer_tabs(cursor, window, cx);
                            self.focus_active_drawer_tab(cursor, window, cx);
                        } else {
                            if let Some(session) = self.active_session() {
                                if let Some(tv) = session.terminal_view.as_ref() {
                                    let fh = tv.read(cx).focus_handle.clone();
                                    fh.focus(window, cx);
                                }
                            }
                        }
                    }
                    self.save_state();
                }
                PendingAction::NewDrawerTab => {
                    skip_refocus = true;
                    if let Some(cursor) = self.active {
                        self.spawn_drawer_tab(cursor, None, None, window, cx);
                        if let Some(session) = self.projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            session.drawer_active_tab = session.drawer_tabs.len().saturating_sub(1);
                            session.drawer_visible = true;
                        }
                        self.focus_active_drawer_tab(cursor, window, cx);
                        self.save_state();
                    }
                }
                PendingAction::SwitchDrawerTab(idx) => {
                    skip_refocus = true;
                    if let Some(cursor) = self.active {
                        if let Some(session) = self.projects
                            .get_mut(cursor.project_idx)
                            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                        {
                            if idx < session.drawer_tabs.len() {
                                session.drawer_active_tab = idx;
                            }
                        }
                        self.drawer_rename = None;
                        self.focus_active_drawer_tab(cursor, window, cx);
                        self.save_state();
                    }
                }
                PendingAction::CloseDrawerTab(idx) => {
                    skip_refocus = true;
                    if let Some(cursor) = self.active {
                        let (remaining, hide_drawer) = {
                            let session = self.projects
                                .get_mut(cursor.project_idx)
                                .and_then(|p| p.sessions.get_mut(cursor.session_idx));
                            if let Some(s) = session {
                                if idx < s.drawer_tabs.len() {
                                    s.drawer_tabs.remove(idx);
                                }
                                if s.drawer_active_tab >= s.drawer_tabs.len() {
                                    s.drawer_active_tab = s.drawer_tabs.len().saturating_sub(1);
                                }
                                let empty = s.drawer_tabs.is_empty();
                                if empty {
                                    s.drawer_visible = false;
                                }
                                (s.drawer_tabs.len(), empty)
                            } else {
                                (0, true)
                            }
                        };
                        if let Some((rc, ri, _)) = &self.drawer_rename {
                            if *rc == cursor && *ri >= remaining {
                                self.drawer_rename = None;
                            }
                        }
                        if hide_drawer {
                            if let Some(session) = self.active_session() {
                                if let Some(tv) = session.terminal_view.as_ref() {
                                    let fh = tv.read(cx).focus_handle.clone();
                                    fh.focus(window, cx);
                                }
                            }
                        } else {
                            self.focus_active_drawer_tab(cursor, window, cx);
                        }
                        self.save_state();
                    }
                }
                PendingAction::StartRenameDrawerTab(idx) => {
                    skip_refocus = true;
                    if let Some(cursor) = self.active {
                        let initial = self.projects
                            .get(cursor.project_idx)
                            .and_then(|p| p.sessions.get(cursor.session_idx))
                            .and_then(|s| s.drawer_tabs.get(idx))
                            .map(|t| t.name.clone());
                        if let Some(name) = initial {
                            self.drawer_rename = Some((cursor, idx, name));
                            let fh = self.drawer_rename_focus
                                .get_or_insert_with(|| cx.focus_handle())
                                .clone();
                            fh.focus(window, cx);
                            cx.notify();
                        }
                    }
                }
                PendingAction::CommitRenameDrawerTab => {
                    skip_refocus = true;
                    if let Some((cursor, idx, buf)) = self.drawer_rename.take() {
                        let trimmed = buf.trim().to_string();
                        if !trimmed.is_empty() {
                            if let Some(session) = self.projects
                                .get_mut(cursor.project_idx)
                                .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                            {
                                if let Some(tab) = session.drawer_tabs.get_mut(idx) {
                                    tab.name = trimmed;
                                }
                            }
                        }
                        self.focus_active_drawer_tab(cursor, window, cx);
                        self.save_state();
                    }
                }
                PendingAction::CancelRenameDrawerTab => {
                    skip_refocus = true;
                    let cursor_opt = self.drawer_rename.take().map(|(c, _, _)| c);
                    if let Some(cursor) = cursor_opt {
                        self.focus_active_drawer_tab(cursor, window, cx);
                    }
                    cx.notify();
                }
                PendingAction::ToggleSidebar => {
                    self.sidebar_visible = !self.sidebar_visible;
                    self.save_settings();
                }
                PendingAction::ToggleRightSidebar => {
                    self.right_sidebar_visible = !self.right_sidebar_visible;
                    self.save_settings();
                }
                PendingAction::RelocateProject(project_idx) => {
                    let paths = cx.prompt_for_paths(PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: Some("Relocate project folder".into()),
                    });

                    cx.spawn(async move |this, cx| {
                        if let Ok(Ok(Some(paths))) = paths.await {
                            if let Some(new_path) = paths.into_iter().next() {
                                let _ = this.update(cx, |this: &mut Self, cx| {
                                    if let Some(project) = this.projects.get_mut(project_idx) {
                                        eprintln!(
                                            "Relocated project '{}': {} -> {}",
                                            project.name,
                                            project.source_path.display(),
                                            new_path.display()
                                        );
                                        project.source_path = new_path;
                                        project.name = Project::name_from_path(&project.source_path);
                                        this.save_settings();
                                    }
                                    cx.notify();
                                });
                            }
                        }
                    })
                    .detach();
                }
                PendingAction::ProceedDirtySession(project_idx) => {
                    // confirming_dirty_session stays Some so
                    // add_session_to_project skips the dirty check.
                    self.add_session_to_project(project_idx, window, cx);
                }
                PendingAction::CancelDirtySession => {
                    self.confirming_dirty_session = None;
                    cx.notify();
                }
                PendingAction::UpdateCleanupPaths(paths) => {
                    skip_refocus = true;
                    self.user_settings.session_cleanup_paths = paths;
                    // Persist. Settings::save() also needs the up-to-date
                    // projects/window-geometry fields — synthesise them
                    // from AppState before writing, mirroring the pattern
                    // used in observe_window_bounds.
                    let snapshot = Settings {
                        projects: self.projects.iter().map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        }).collect(),
                        ..self.user_settings.clone()
                    };
                    snapshot.save();
                }
                PendingAction::ResumeSession { project_idx, session_idx } => {
                    let cursor = SessionCursor { project_idx, session_idx };
                    self.resume_session(cursor, window, cx);
                    self.sync_browser_to_active();
                }
                PendingAction::SyncBrowserToActiveSession => {
                    skip_refocus = true;
                    self.sync_browser_to_active();
                }
                PendingAction::CloseBrowserTabForSession { project_idx, session_idx } => {
                    skip_refocus = true;
                    let cursor = SessionCursor { project_idx, session_idx };
                    let tab_id = self
                        .projects
                        .get(cursor.project_idx)
                        .and_then(|p| p.sessions.get(cursor.session_idx))
                        .and_then(|s| s.browser_tab_id);
                    if let Some(id) = tab_id {
                        let _ = browser::close_tab(id);
                    }
                    if let Some(session) = self
                        .projects
                        .get_mut(cursor.project_idx)
                        .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                    {
                        session.browser_tab_id = None;
                    }
                    self.browser_status = "Chrome tab closed.".to_string();
                    self.save_state();
                }
                PendingAction::OpenScratchPad => {
                    skip_refocus = true;
                    self.open_scratch_pad(window, cx);
                }
                PendingAction::UpdateBrowserIntegration(enabled) => {
                    skip_refocus = true;
                    self.user_settings.browser_integration_enabled = enabled;
                    if !enabled {
                        self.browser_status.clear();
                    }
                    let snapshot = Settings {
                        projects: self.projects.iter().map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        }).collect(),
                        ..self.user_settings.clone()
                    };
                    snapshot.save();
                }
                PendingAction::UpdateExternalEditor(cmd) => {
                    skip_refocus = true;
                    let trimmed = cmd.trim();
                    self.user_settings.external_editor_command = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                    let snapshot = Settings {
                        projects: self.projects.iter().map(|p| ProjectSave {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            source_path: p.source_path.clone(),
                            settings: p.settings.clone(),
                        }).collect(),
                        ..self.user_settings.clone()
                    };
                    snapshot.save();
                }
            }

            // After any sidebar-triggered action, re-focus the active
            // terminal so keyboard input goes back to Claude Code.
            // ToggleDrawer manages its own focus, so skip it.
            if !skip_refocus {
                if let Some(session) = self.active_session() {
                    if let Some(tv) = session.terminal_view.as_ref() {
                        let fh = tv.read(cx).focus_handle.clone();
                        fh.focus(window, cx);
                    }
                }
            }
        }

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
        let mut state_dirty = false;
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
                        eprintln!(
                            "Resume failed for session {} — PTY exited inside grace window",
                            session.id
                        );
                        session.terminal_view = None;
                        session.status = SessionStatus::Suspended;
                    } else {
                        session.status = SessionStatus::Done;
                    }
                    session.last_active = std::time::SystemTime::now();
                    session.resuming_until = None;
                    state_dirty = true;
                } else if let Some(deadline) = session.resuming_until {
                    if now >= deadline {
                        session.resuming_until = None;
                    }
                }
            }
        }
        if state_dirty {
            self.save_state();
        }

        // Build sidebar items: for each project, a header then its sessions
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
                                this.pending_action = Some(PendingAction::AddSessionToProject(p_idx));
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
                                this.pending_action = Some(PendingAction::RemoveProject(p_idx));
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );

            // Dirty-state confirmation prompt
            if self.confirming_dirty_session == Some(p_idx) {
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
                                .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.pending_action = Some(PendingAction::CancelDirtySession);
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
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
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
                let is_confirming = self.confirming_discard
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
                        this.pending_action = Some(PendingAction::SelectSession {
                            project_idx: p_idx,
                            session_idx: s_idx,
                        });
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
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
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
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
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
                                    .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
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
                                            .on_mouse_down(MouseButton::Left, cx.listener(move |this: &mut Self, _event, _window, cx| {
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

        let sidebar_w = self.sidebar_width;
        let sidebar_visible = self.sidebar_visible;
        let is_resizing = self.sidebar_resizing;
        let drawer_is_resizing = self.drawer_resizing;
        let drawer_visible = self.active_session()
            .map(|s| s.drawer_visible)
            .unwrap_or(false);
        let right_sidebar_visible = self.right_sidebar_visible;
        let right_sidebar_w = self.right_sidebar_width;
        let right_sidebar_resizing = self.right_sidebar_resizing;

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
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        this.open_folder_picker(cx);
                                    })),
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
                        this.sidebar_resizing = true;
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
                            if let Some(tv) = self.active_session().and_then(|s| s.terminal_view.clone()) {
                                // Tell the main terminal how much space the drawer
                                // reserves below it so the PTY resize is correct.
                                let inset = if drawer_visible {
                                    // 6px resize handle + ~30px header + drawer panel
                                    6.0 + 30.0 + self.drawer_height
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
                                            this.pending_action = Some(PendingAction::ResumeSession {
                                                project_idx: active.project_idx,
                                                session_idx: active.session_idx,
                                            });
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
                                        this.pending_action = Some(PendingAction::AddSessionToProject(active.project_idx));
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
                    if self.confirming_quit {
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
                                                    this.confirming_quit = false;
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
                                                    this.confirming_quit = false;
                                                    cx.notify();
                                                })),
                                        ),
                                ),
                        );
                    }

                    content_col = content_col.child(main_area);
                }

                // --- Drawer terminal (fixed height, shown per-session) ---
                let drawer_h = self.drawer_height;
                if drawer_visible {
                    // Resize handle — 6px tall invisible hover zone above drawer
                    content_col = content_col.child(
                        div()
                            .id("drawer-resize-handle")
                            .w_full()
                            .h(px(6.0))
                            .cursor_row_resize()
                            .bg(rgb(0x313244))
                            .hover(|s| s.bg(rgb(0x45475a)))
                            .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                this.drawer_resizing = true;
                                cx.notify();
                            })),
                    );

                    // --- Drawer header bar with tab strip ---
                    let active_cursor = self.active;
                    let (tabs_meta, active_tab_idx, active_tab_view): (
                        Vec<(usize, String)>,
                        usize,
                        Option<Entity<TerminalView>>,
                    ) = if let Some(session) = self.active_session() {
                        let data = session
                            .drawer_tabs
                            .iter()
                            .enumerate()
                            .map(|(i, t)| (i, t.name.clone()))
                            .collect();
                        let view = session
                            .drawer_tabs
                            .get(session.drawer_active_tab)
                            .map(|t| t.view.clone());
                        (data, session.drawer_active_tab, view)
                    } else {
                        (Vec::new(), 0, None)
                    };

                    let renaming_idx = self
                        .drawer_rename
                        .as_ref()
                        .filter(|(c, _, _)| Some(*c) == active_cursor)
                        .map(|(_, i, _)| *i);
                    let rename_buf = self
                        .drawer_rename
                        .as_ref()
                        .filter(|(c, _, _)| Some(*c) == active_cursor)
                        .map(|(_, _, buf)| buf.clone())
                        .unwrap_or_default();
                    let rename_focus = self.drawer_rename_focus.clone();

                    let mut tab_strip = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .flex_1()
                        .overflow_hidden();

                    for (idx, name) in tabs_meta {
                        let is_active = idx == active_tab_idx;
                        let is_renaming = renaming_idx == Some(idx);
                        let tab_bg = if is_active { 0x313244 } else { 0x1e1e2e };
                        let tab_fg = if is_active { 0xcdd6f4 } else { 0xa6adc8 };

                        let mut tab_el = div()
                            .id(("drawer-tab", idx))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .bg(rgb(tab_bg))
                            .text_size(px(11.0))
                            .text_color(rgb(tab_fg))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x45475a)));

                        if is_renaming {
                            let display = if rename_buf.is_empty() {
                                " ".to_string()
                            } else {
                                rename_buf.clone()
                            };
                            let mut label = div()
                                .min_w(px(40.0))
                                .px(px(4.0))
                                .border_1()
                                .border_color(rgb(0x89b4fa))
                                .rounded(px(2.0))
                                .bg(rgb(0x181825))
                                .text_color(rgb(0xcdd6f4))
                                .child(format!("{display}▎"));
                            if let Some(fh) = rename_focus.clone() {
                                label = label
                                    .track_focus(&fh)
                                    .on_key_down(cx.listener(
                                        |this: &mut Self, event: &KeyDownEvent, _window, cx| {
                                            let key = event.keystroke.key.as_str();
                                            let mods = &event.keystroke.modifiers;
                                            match key {
                                                "enter" => {
                                                    this.pending_action =
                                                        Some(PendingAction::CommitRenameDrawerTab);
                                                    cx.notify();
                                                }
                                                "escape" => {
                                                    this.pending_action =
                                                        Some(PendingAction::CancelRenameDrawerTab);
                                                    cx.notify();
                                                }
                                                "backspace" => {
                                                    if let Some((_, _, buf)) =
                                                        this.drawer_rename.as_mut()
                                                    {
                                                        buf.pop();
                                                        cx.notify();
                                                    }
                                                }
                                                _ => {
                                                    if let Some(ref ch) = event.keystroke.key_char {
                                                        if !mods.control && !mods.platform {
                                                            if let Some((_, _, buf)) =
                                                                this.drawer_rename.as_mut()
                                                            {
                                                                buf.push_str(ch);
                                                                cx.notify();
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        },
                                    ));
                            }
                            tab_el = tab_el.child(label);
                        } else {
                            tab_el = tab_el
                                .child(
                                    div()
                                        .child(name)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this: &mut Self, event: &MouseDownEvent, _window, cx| {
                                                if event.click_count >= 2 {
                                                    this.pending_action =
                                                        Some(PendingAction::StartRenameDrawerTab(idx));
                                                } else {
                                                    this.pending_action =
                                                        Some(PendingAction::SwitchDrawerTab(idx));
                                                }
                                                cx.notify();
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id(("drawer-tab-close", idx))
                                        .px(px(4.0))
                                        .rounded(px(3.0))
                                        .text_size(px(11.0))
                                        .text_color(rgb(0x6c7086))
                                        .hover(|s| {
                                            s.bg(rgb(0x585b70))
                                                .text_color(rgb(0xf38ba8))
                                        })
                                        .child("×")
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this: &mut Self, _event, _window, cx| {
                                                this.pending_action =
                                                    Some(PendingAction::CloseDrawerTab(idx));
                                                cx.notify();
                                            }),
                                        ),
                                );
                        }

                        tab_strip = tab_strip.child(tab_el);
                    }

                    // New tab button
                    tab_strip = tab_strip.child(
                        div()
                            .id("drawer-new-tab-btn")
                            .cursor_pointer()
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .text_size(px(13.0))
                            .text_color(rgb(0x6c7086))
                            .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                            .child("+")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _event, _window, cx| {
                                    this.pending_action = Some(PendingAction::NewDrawerTab);
                                    cx.notify();
                                }),
                            ),
                    );

                    content_col = content_col.child(
                        div()
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
                            .gap(px(8.0))
                            .child(tab_strip)
                            .child(
                                div()
                                    .id("drawer-close-btn")
                                    .cursor_pointer()
                                    .px(px(6.0))
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .text_size(px(12.0))
                                    .text_color(rgb(0x6c7086))
                                    .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                                    .child("×")
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        this.pending_action = Some(PendingAction::ToggleDrawer);
                                        cx.notify();
                                    })),
                            ),
                    );

                    // Drawer content — active tab's terminal view
                    let mut drawer_panel = div()
                        .w_full()
                        .h(px(drawer_h))
                        .flex_shrink_0()
                        .bg(rgb(0x1e1e2e));

                    if let Some(dt) = active_tab_view {
                        drawer_panel = drawer_panel.child(dt);
                    } else {
                        drawer_panel = drawer_panel.child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(11.0))
                                .text_color(rgb(0x45475a))
                                .child("Terminal drawer"),
                        );
                    }
                    content_col = content_col.child(drawer_panel);
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
                        this.right_sidebar_resizing = true;
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
                                    .on_mouse_down(MouseButton::Left, cx.listener(|this: &mut Self, _event, _window, cx| {
                                        this.pending_action = Some(PendingAction::ToggleRightSidebar);
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
                        if (new_width - this.sidebar_width).abs() > 0.5 {
                            this.sidebar_width = new_width;
                            window.refresh();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                        this.sidebar_resizing = false;
                        this.save_settings();
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
                        if (new_width - this.right_sidebar_width).abs() > 0.5 {
                            this.right_sidebar_width = new_width;
                            window.refresh();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                        this.right_sidebar_resizing = false;
                        this.save_settings();
                        cx.notify();
                    })),
            );
        }

        // Drawer drag overlay
        if drawer_is_resizing {
            outer = outer.child(
                div()
                    .id("drawer-drag-overlay")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .cursor_row_resize()
                    .on_mouse_move(cx.listener(|this: &mut Self, event: &MouseMoveEvent, window, cx| {
                        let viewport_h = f32::from(window.viewport_size().height);
                        let mouse_y = f32::from(event.position.y);
                        // Drawer height = distance from bottom of viewport to mouse
                        let new_height = (viewport_h - mouse_y).clamp(DRAWER_MIN_HEIGHT, viewport_h - 200.0);
                        if (new_height - this.drawer_height).abs() > 0.5 {
                            this.drawer_height = new_height;
                            window.refresh();
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this: &mut Self, _event: &MouseUpEvent, _window, cx| {
                        this.drawer_resizing = false;
                        this.save_settings();
                        cx.notify();
                    })),
            );
        }

        if let Some(pad) = self.scratch_pad.clone() {
            outer = outer.child(pad);
        }

        outer
    }
}
