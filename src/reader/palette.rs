//! Cmd+P fuzzy file retrieval (DEV-39).
//!
//! Collects the workspace's tracked + untracked-but-not-ignored files via
//! `git ls-files` (so `.gitignore` is respected exactly, not by blanket
//! dotfile hiding), scores them against the query with a lightweight fuzzy
//! matcher, and boosts recently-opened and session-changed files. The overlay
//! rendering lives in this module too; state hangs off `AppState`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::app_state::{AppState, MainTab};
use crate::reader::index::IndexStatus;
use crate::theme::theme;

/// A candidate file plus the signals that bias its ranking.
pub(crate) struct FilePalette {
    /// All candidate paths, absolute. Shared (cheaply cloned) from the
    /// background-built [`FileIndex`](crate::reader::index::FileIndex).
    pub(crate) files: Arc<Vec<PathBuf>>,
    /// Recently opened files (most-recent first), for the empty-query view and
    /// a ranking bonus. Absolute paths.
    pub(crate) recent: Vec<PathBuf>,
    /// Session-changed files (from the git changes panel), absolute.
    pub(crate) changed: HashSet<PathBuf>,
    /// Indices into `files`, best match first, after the current query.
    pub(crate) results: Vec<usize>,
    /// Highlighted row within `results`.
    pub(crate) selected: usize,
    /// Workspace root, for rendering paths relative and reloading.
    pub(crate) root: PathBuf,
}

/// Hard cap on files scanned/scored — keeps Cmd+P instant on huge repos.
/// DEV-40 replaces this synchronous scan with an incremental index.
const MAX_FILES: usize = 20_000;
/// Rows shown in the overlay at once.
pub(crate) const MAX_RESULTS: usize = 50;

impl FilePalette {
    pub(crate) fn new(
        root: PathBuf,
        files: Arc<Vec<PathBuf>>,
        recent: Vec<PathBuf>,
        changed: HashSet<PathBuf>,
    ) -> Self {
        let mut p = FilePalette {
            files,
            recent,
            changed,
            results: Vec::new(),
            selected: 0,
            root,
        };
        p.recompute("");
        p
    }

    /// Recompute `results` for `query`. Empty query surfaces recents, then
    /// changed files, then the rest by path.
    pub(crate) fn recompute(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() {
            let mut ordered: Vec<usize> = Vec::new();
            let mut seen = HashSet::new();
            // Recents first, in order.
            for r in &self.recent {
                if let Some(i) = self.files.iter().position(|f| f == r) {
                    if seen.insert(i) {
                        ordered.push(i);
                    }
                }
            }
            // Then changed.
            for (i, f) in self.files.iter().enumerate() {
                if self.changed.contains(f) && seen.insert(i) {
                    ordered.push(i);
                }
            }
            // Then everything else.
            for i in 0..self.files.len() {
                if seen.insert(i) {
                    ordered.push(i);
                }
            }
            ordered.truncate(MAX_RESULTS);
            self.results = ordered;
            self.selected = 0;
            return;
        }

        let ql = q.to_lowercase();
        let mut scored: Vec<(i32, usize)> = Vec::new();
        for (i, f) in self.files.iter().enumerate() {
            let rel = f.strip_prefix(&self.root).unwrap_or(f);
            let hay = rel.to_string_lossy().to_lowercase();
            if let Some(mut score) = fuzzy_score(&ql, &hay) {
                if self.recent.contains(f) {
                    score += 40;
                }
                if self.changed.contains(f) {
                    score += 20;
                }
                scored.push((score, i));
            }
        }
        // Highest score first; stable tiebreak on shorter path then path order.
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                self.files[a.1]
                    .as_os_str()
                    .len()
                    .cmp(&self.files[b.1].as_os_str().len())
            })
        });
        scored.truncate(MAX_RESULTS);
        self.results = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }

    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let len = self.results.len() as i32;
        let mut s = self.selected as i32 + delta;
        if s < 0 {
            s = len - 1;
        } else if s >= len {
            s = 0;
        }
        self.selected = s as usize;
    }

    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
        self.results
            .get(self.selected)
            .and_then(|&i| self.files.get(i))
            .cloned()
    }
}

/// Fuzzy subsequence score. Returns `None` if `query`'s chars don't all appear
/// in order in `hay`. Higher is better. Rewards consecutive runs and matches at
/// path/word boundaries, and weights the basename over parent directories.
pub(crate) fn fuzzy_score(query: &str, hay: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let hay_bytes: Vec<char> = hay.chars().collect();
    let basename_start = hay.rfind('/').map(|i| i + 1).unwrap_or(0);

    let mut score = 0i32;
    let mut hi = 0usize;
    let mut prev_matched = false;
    let mut matched_any = false;

    for qc in query.chars() {
        // Advance through hay until we find qc.
        let mut found = false;
        while hi < hay_bytes.len() {
            let hc = hay_bytes[hi];
            if hc == qc {
                let byte_idx: usize = hay.char_indices().nth(hi).map(|(b, _)| b).unwrap_or(0);
                let mut s = 1;
                if prev_matched {
                    s += 5; // consecutive run
                }
                // Boundary bonus: start, or after a separator.
                let is_boundary =
                    hi == 0 || matches!(hay_bytes[hi - 1], '/' | '_' | '-' | '.' | ' ');
                if is_boundary {
                    s += 8;
                }
                if byte_idx >= basename_start {
                    s += 4; // basename weight
                }
                score += s;
                hi += 1;
                prev_matched = true;
                found = true;
                matched_any = true;
                break;
            }
            hi += 1;
            prev_matched = false;
        }
        if !found {
            return None;
        }
    }
    let _ = matched_any;
    // Shorter haystacks that fully match rank a touch higher.
    score += (40i32).saturating_sub(hay_bytes.len() as i32 / 4);
    Some(score)
}

/// List candidate files. Prefers `git ls-files` (exact `.gitignore` semantics);
/// falls back to a bounded manual walk for non-git workspaces. Surfaces a
/// missing/unreadable root as an error so the index can show the failure.
pub(crate) fn collect_files_result(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Err(format!("Workspace path not found: {}", root.display()));
    }
    if let Some(files) = git_ls_files(root) {
        return Ok(files);
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.truncate(MAX_FILES);
    Ok(out)
}

fn git_ls_files(root: &Path) -> Option<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files: Vec<PathBuf> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| root.join(std::ffi::OsStr::from_bytes(s)))
        .collect();
    files.truncate(MAX_FILES);
    Some(files)
}

/// Bounded fallback walk that skips the usual heavyweight/noise directories.
fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    const SKIP: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "dist",
        "build",
        ".next",
        ".venv",
        "venv",
        "__pycache__",
        ".idea",
        ".DS_Store",
    ];
    if out.len() >= MAX_FILES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk(root, &path, out),
            Ok(ft) if ft.is_file() => out.push(path),
            _ => {}
        }
        if out.len() >= MAX_FILES {
            return;
        }
    }
}

impl AppState {
    /// Open the Cmd+P overlay: snapshot candidate files, recents, and changed
    /// files, then focus the query input.
    pub(crate) fn open_file_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.reader_workspace_root() else {
            return;
        };
        // Ensure a background index exists / is fresh for this root. The
        // palette renders whatever is cached now and re-points when the index
        // build completes (see refresh_file_index).
        self.ensure_file_index(cx);
        let files = self.file_index.files.clone();
        let recent = self.reader.recent.clone();
        let changed: HashSet<PathBuf> = self
            .changes
            .files
            .iter()
            .map(|f| root.join(&f.path))
            .collect();
        self.file_palette = Some(FilePalette::new(root, files, recent, changed));
        self.file_palette_input
            .update(cx, |i, cx| i.set_text_silent("", cx));
        self.file_palette_input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_file_palette(&mut self, cx: &mut Context<Self>) {
        if self.file_palette.take().is_some() {
            cx.notify();
        }
    }

    /// Open the highlighted file in the Reader and dismiss the overlay.
    pub(crate) fn confirm_file_palette(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file_palette.as_ref().and_then(|p| p.selected_path()) else {
            return;
        };
        // Reveal in the tree: expand every ancestor up to the workspace root.
        if let Some(root) = self.reader_workspace_root() {
            let mut cur = path.parent().map(|p| p.to_path_buf());
            while let Some(dir) = cur {
                if dir.starts_with(&root) && dir != root {
                    self.reader.expanded_dirs.insert(dir.clone());
                    cur = dir.parent().map(|p| p.to_path_buf());
                } else {
                    break;
                }
            }
        }
        self.reader.selected_path = Some(path.clone());
        self.main_tab = MainTab::Reader;
        self.load_preview(path);
        self.file_palette = None;
        cx.notify();
    }

    /// Full-screen dimmed overlay hosting the query input and result list.
    /// Returns an empty `div` when the palette is closed.
    pub(crate) fn render_file_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div();
        let Some(palette) = self.file_palette.as_ref() else {
            return root;
        };

        let mut list = div().flex().flex_col().py(px(4.0)).overflow_hidden();
        if palette.results.is_empty() {
            let msg = match &self.file_index.status {
                IndexStatus::Building => "Indexing workspace…",
                IndexStatus::Failed(_) => "Could not index workspace",
                _ => "No matching files",
            };
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .text_color(theme().text_faint)
                    .child(msg),
            );
        }
        for (row, &file_idx) in palette.results.iter().enumerate() {
            let Some(path) = palette.files.get(file_idx) else {
                continue;
            };
            let rel = path.strip_prefix(&palette.root).unwrap_or(path);
            let rel_str = rel.to_string_lossy().into_owned();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel_str.clone());
            let parent = rel
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty());
            let selected = row == palette.selected;
            let is_recent = palette.recent.contains(path);
            let is_changed = palette.changed.contains(path);

            list = list.child(
                div()
                    .id(("palette-row", row))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .py(px(5.0))
                    .when(selected, |d| d.bg(theme().bg_active))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme().bg_hover))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_primary)
                            .child(name),
                    )
                    .when_some(parent, |d, p| {
                        d.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(px(10.0))
                                .text_color(theme().text_ghost)
                                .child(p),
                        )
                    })
                    .when(is_changed, |d| {
                        d.child(tag_pill("changed", theme().warning))
                    })
                    .when(is_recent && !is_changed, |d| {
                        d.child(tag_pill("recent", theme().text_ghost))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _window, cx| {
                            if let Some(p) = this.file_palette.as_mut() {
                                p.selected = row;
                            }
                            this.confirm_file_palette(cx);
                        }),
                    ),
            );
        }

        let panel = div()
            .w(px(560.0))
            .max_h(px(440.0))
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
                    .child(self.file_palette_input.clone()),
            )
            .child(
                div()
                    .id("palette-results")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(list),
            )
            .child({
                let (text, color) = match &self.file_index.status {
                    IndexStatus::Building => ("Indexing…".to_string(), theme().text_ghost),
                    IndexStatus::Failed(e) => (e.clone(), theme().warning),
                    _ => (
                        format!(
                            "{} files · {} shown",
                            palette.files.len(),
                            palette.results.len()
                        ),
                        theme().text_ghost,
                    ),
                };
                div()
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(5.0))
                    .border_t_1()
                    .border_color(theme().border_subtle)
                    .text_size(px(10.0))
                    .text_color(color)
                    .child(text)
            });

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
                .pt(px(90.0))
                .bg(hsla(0.0, 0.0, 0.0, 0.35))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this: &mut Self, _e, _window, cx| {
                        this.close_file_palette(cx);
                    }),
                )
                .on_key_down(
                    cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => this.close_file_palette(cx),
                            "enter" => this.confirm_file_palette(cx),
                            "down" => {
                                if let Some(p) = this.file_palette.as_mut() {
                                    p.move_selection(1);
                                }
                                cx.notify();
                            }
                            "up" => {
                                if let Some(p) = this.file_palette.as_mut() {
                                    p.move_selection(-1);
                                }
                                cx.notify();
                            }
                            _ => {}
                        }
                    }),
                )
                .child(
                    // Swallow clicks on the panel so they don't dismiss.
                    div()
                        .on_mouse_down(MouseButton::Left, |_e, _window, cx| cx.stop_propagation())
                        .child(panel),
                ),
        ));
        root
    }
}

/// Small rounded label pill used for the recent/changed markers.
fn tag_pill(label: &'static str, color: Hsla) -> Div {
    div()
        .flex_shrink_0()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(px(3.0))
        .text_size(px(9.0))
        .text_color(color)
        .border_1()
        .border_color(color)
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matches_subsequence_and_ranks_boundaries() {
        // "amrs" should match "src/app_state.rs" and "readme.rs"-style paths.
        assert!(fuzzy_score("appst", "src/app_state.rs").is_some());
        assert!(fuzzy_score("xyz", "src/app_state.rs").is_none());
        // Basename match should outscore a scattered parent-dir match.
        let base = fuzzy_score("state", "src/app_state.rs").unwrap();
        let scattered = fuzzy_score("state", "s_t_a_t_e/zzzzzz.rs").unwrap();
        assert!(base > 0 && scattered > 0);
    }

    #[test]
    fn empty_query_scores_zero_not_none() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }
}
