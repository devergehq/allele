//! Changes panel — git working-tree status for the right panel.
//!
//! Shows the active session clone's changed files split into Staged and
//! Unstaged sections; clicking a row loads that file's diff into a
//! scrollable pane below the list. All git subprocess work runs on the
//! background executor; results carry a generation number so a stale
//! load can never clobber a newer one.

use gpui::*;

use crate::actions::SidebarAction;
use crate::app_state::AppState;
use crate::git::{ChangeKind, ChangedFile};

/// Most diff lines rendered before truncating with a notice. Renders are
/// per-frame, so an unbounded generated-file diff would tank the UI.
const DIFF_LINE_CAP: usize = 2000;

impl AppState {
    /// Render-time staleness check: when the panel is visible but its data
    /// was loaded for a different directory than the active session's clone
    /// (session switch, first open), kick off a refresh. The guard is the
    /// directory comparison itself — `refresh_changes` records the new
    /// directory synchronously, so this fires at most once per change.
    pub(crate) fn ensure_changes_fresh(&mut self, cx: &mut Context<Self>) {
        if !self.right_panel.visible {
            return;
        }
        let active_dir = self.active_session().and_then(|s| s.clone_path.clone());
        if self.changes.repo_dir != active_dir {
            self.refresh_changes(cx);
        }
    }

    /// Re-run `git status` for the active session's clone on the background
    /// executor and replace the panel's file list with the result.
    pub(crate) fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        let dir = self.active_session().and_then(|s| s.clone_path.clone());
        let dir_changed = self.changes.repo_dir != dir;

        // Coalesce same-directory refreshes: one git status in flight at a
        // time; a request arriving meanwhile re-fires when it completes.
        if !dir_changed && self.changes.loading {
            self.changes.refresh_queued = true;
            return;
        }

        self.changes.refresh_gen += 1;
        let generation = self.changes.refresh_gen;
        self.changes.repo_dir = dir.clone();
        if dir_changed {
            // Never show (or diff against) the previous repo's rows while
            // the new directory's status loads.
            self.changes.files.clear();
            self.changes.selected = None;
            self.changes.diff = None;
            self.changes.diff_gen += 1; // invalidate any in-flight diff
        }

        let Some(dir) = dir else {
            self.changes.is_repo = false;
            self.changes.loading = false;
            cx.notify();
            return;
        };

        self.changes.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !crate::git::is_git_repo(&dir) {
                        return None;
                    }
                    Some(crate::git::status_changes(&dir))
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.changes.refresh_gen != generation {
                    return;
                }
                this.changes.loading = false;
                match result {
                    None => {
                        this.changes.is_repo = false;
                        this.changes.files.clear();
                        this.changes.selected = None;
                        this.changes.diff = None;
                    }
                    Some(Ok(files)) => {
                        this.changes.is_repo = true;
                        // Drop the selection if its row disappeared (e.g.
                        // the file was committed or reverted).
                        if let Some((path, staged)) = this.changes.selected.clone() {
                            if !files.iter().any(|f| f.path == path && f.staged == staged) {
                                this.changes.selected = None;
                                this.changes.diff = None;
                                this.changes.diff_gen += 1; // drop in-flight diff
                            }
                        }
                        this.changes.files = files;
                        // Reload the surviving selection's diff — its
                        // content may be what just changed.
                        if let Some((path, staged)) = this.changes.selected.clone() {
                            this.load_changes_diff(path, staged, cx);
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!("changes panel: git status failed: {e}");
                        this.changes.files.clear();
                    }
                }
                cx.notify();
                // A refresh was requested while this one ran — re-fire so
                // the panel reflects the latest working-tree state.
                if this.changes.refresh_queued {
                    this.changes.refresh_queued = false;
                    this.refresh_changes(cx);
                }
            });
        })
        .detach();
    }

    /// Load the diff for one file row on the background executor.
    pub(crate) fn load_changes_diff(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let Some(dir) = self.changes.repo_dir.clone() else {
            return;
        };
        let kind = self
            .changes
            .files
            .iter()
            .find(|f| f.path == path && f.staged == staged)
            .map(|f| f.kind)
            .unwrap_or(ChangeKind::Modified);
        self.changes.diff_gen += 1;
        let generation = self.changes.diff_gen;
        self.changes.diff_loading = true;
        // Clear the previous file's diff so the pane shows the loading
        // state instead of stale content under the new header.
        self.changes.diff = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::git::diff_file(&dir, &path, kind, staged) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.changes.diff_gen != generation {
                    return;
                }
                this.changes.diff_loading = false;
                match result {
                    Ok(diff) => this.changes.diff = Some(diff),
                    Err(e) => {
                        tracing::warn!("changes panel: git diff failed: {e}");
                        this.changes.diff = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The right panel's body: Staged / Unstaged file sections on top, the
    /// selected file's diff below. Assumes the caller renders the panel
    /// chrome (header, close button) around it.
    pub(crate) fn render_changes_panel_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col();

        // Empty / fallback states first.
        if self.changes.repo_dir.is_none() {
            return body.child(centered_note("No active session"));
        }
        if !self.changes.is_repo {
            return body.child(centered_note("Not a git repository"));
        }
        if self.changes.files.is_empty() {
            let note = if self.changes.loading { "Loading…" } else { "No changes — working tree clean" };
            return body.child(centered_note(note));
        }

        let staged: Vec<ChangedFile> =
            self.changes.files.iter().filter(|f| f.staged).cloned().collect();
        let unstaged: Vec<ChangedFile> =
            self.changes.files.iter().filter(|f| !f.staged).cloned().collect();

        let mut list = div()
            .id("changes-file-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .py(px(4.0));

        let mut row_ix = 0usize;
        for (title, files) in [("Staged", &staged), ("Unstaged", &unstaged)] {
            if files.is_empty() {
                continue;
            }
            list = list.child(
                div()
                    .px(px(12.0))
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .text_size(px(10.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0x6c7086))
                    .child(format!("{} ({})", title.to_uppercase(), files.len())),
            );
            for file in files {
                list = list.child(self.render_change_row(file, row_ix, cx));
                row_ix += 1;
            }
        }
        body = body.child(list);

        // Diff pane below the list when a row is selected.
        if let Some((path, staged)) = self.changes.selected.clone() {
            body = body.child(self.render_changes_diff_pane(&path, staged, cx));
        }

        body
    }

    fn render_change_row(
        &self,
        file: &ChangedFile,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected =
            self.changes.selected.as_ref() == Some(&(file.path.clone(), file.staged));
        let badge_color = match file.kind {
            ChangeKind::Modified => rgb(0xf9e2af),
            ChangeKind::Added => rgb(0xa6e3a1),
            ChangeKind::Deleted => rgb(0xf38ba8),
            ChangeKind::Renamed | ChangeKind::Copied => rgb(0x89b4fa),
            ChangeKind::TypeChange => rgb(0xfab387),
            ChangeKind::Unmerged => rgb(0xf38ba8),
            ChangeKind::Untracked => rgb(0x6c7086),
        };
        let path = file.path.clone();
        let staged = file.staged;
        let mut row = div()
            .id(("changes-row", row_ix))
            .px(px(12.0))
            .py(px(3.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer();
        if selected {
            row = row.bg(rgb(0x313244));
        }
        row.hover(|s| s.bg(rgb(0x2a2a3c)))
            .child(
                div()
                    .w(px(12.0))
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .font_family("monospace")
                    .font_weight(FontWeight::BOLD)
                    .text_color(badge_color)
                    .child(file.kind.badge()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_size(px(11.0))
                    .text_color(rgb(0xcdd6f4))
                    .whitespace_nowrap()
                    .child(file.path.clone()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut Self, _event, _window, cx| {
                    this.pending_action = Some(
                        SidebarAction::SelectChangedFile {
                            path: path.clone(),
                            staged,
                        }
                        .into(),
                    );
                    cx.notify();
                }),
            )
    }

    fn render_changes_diff_pane(
        &self,
        path: &str,
        staged: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut pane = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(0x313244));

        // Diff header: file name + side + close button.
        pane = pane.child(
            div()
                .px(px(12.0))
                .py(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .bg(rgb(0x181825))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(0xcdd6f4))
                        .child(format!(
                            "{} {}",
                            path,
                            if staged { "(staged)" } else { "" }
                        )),
                )
                .child(
                    div()
                        .id("changes-diff-close")
                        .cursor_pointer()
                        .px(px(6.0))
                        .rounded(px(6.0))
                        .text_size(px(12.0))
                        .text_color(rgb(0x6c7086))
                        .hover(|s| s.bg(rgb(0x313244)).text_color(rgb(0xcdd6f4)))
                        .child("×")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this: &mut Self, _event, _window, cx| {
                                this.pending_action =
                                    Some(SidebarAction::ClearChangesSelection.into());
                                cx.notify();
                            }),
                        ),
                ),
        );

        let mut diff_body = div()
            .id("changes-diff-scroll")
            .flex_1()
            .min_h(px(0.0))
            .overflow_scroll()
            .p(px(8.0))
            .text_size(px(10.0))
            .font_family("monospace");

        match &self.changes.diff {
            None if self.changes.diff_loading => {
                diff_body = diff_body.child(centered_note("Loading diff…"));
            }
            None => {
                diff_body = diff_body.child(centered_note("No diff available"));
            }
            Some(diff) if diff.binary => {
                diff_body = diff_body.child(centered_note("Binary file — no diff"));
            }
            Some(diff) => {
                let mut shown = 0usize;
                for line in diff.text.lines() {
                    if shown >= DIFF_LINE_CAP {
                        break;
                    }
                    shown += 1;
                    let (color, bg) = match line.as_bytes().first() {
                        Some(b'+') => (rgb(0xa6e3a1), Some(rgba(0xa6e3a118))),
                        Some(b'-') => (rgb(0xf38ba8), Some(rgba(0xf38ba818))),
                        Some(b'@') => (rgb(0x89b4fa), None),
                        Some(b'd') | Some(b'i') => (rgb(0x6c7086), None), // diff --git / index headers
                        _ => (rgb(0xbac2de), None),
                    };
                    let mut row = div()
                        .px(px(4.0))
                        .whitespace_nowrap()
                        .text_color(color)
                        .child(line.to_string());
                    if let Some(bg) = bg {
                        row = row.bg(bg);
                    }
                    diff_body = diff_body.child(row);
                }
                if diff.truncated || diff.text.lines().count() > DIFF_LINE_CAP {
                    diff_body = diff_body.child(
                        div()
                            .px(px(4.0))
                            .py(px(6.0))
                            .text_color(rgb(0x6c7086))
                            .child("… diff truncated"),
                    );
                }
            }
        }

        pane.child(diff_body)
    }
}

/// Small centered grey note used for the panel's empty/fallback states.
fn centered_note(text: &'static str) -> Div {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .py(px(20.0))
        .text_size(px(11.0))
        .text_color(rgb(0x45475a))
        .child(text)
}
