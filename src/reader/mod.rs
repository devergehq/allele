//! Reader tab rendering — file-tree rows, context menu, preview pane.
//!
//! The Reader is a read-only project comprehension surface, not an in-app
//! editor: it retrieves and displays source, Markdown, and referenced
//! artifacts while editing stays in the user's external editor (DEV-43).
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 7.
//! See ARCHITECTURE.md §2 for module role.

pub(crate) mod highlight;
pub(crate) mod palette;

use gpui::*;
use gpui::prelude::FluentBuilder as _;
use std::path::PathBuf;
use crate::theme::theme;

use crate::app_state::{AppState, Preview, PreviewKind};

impl AppState {
    /// Root directory for the Reader tab's file tree: the active session's
    /// clone path if present, otherwise the project's source path.
    pub(crate) fn reader_workspace_root(&self) -> Option<PathBuf> {
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

    /// Two-column Reader view: file tree on the left, file preview on the right.
    pub(crate) fn render_reader_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let root = self.reader_workspace_root();

        let tree_col = {
            let mut col = div()
                .id("reader-tree-scroll")
                .w(px(240.0))
                .flex_shrink_0()
                .h_full()
                .overflow_y_scroll()
                .bg(theme().bg_surface)
                .border_r_1()
                .border_color(theme().border_subtle)
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme().text_primary);

            if let Some(root_path) = root.clone() {
                let mut rows: Vec<AnyElement> = Vec::new();
                let mut counter: usize = 0;
                self.collect_tree_rows(&root_path, 0, &mut rows, &mut counter, cx);
                if rows.is_empty() {
                    col = col.child(
                        div()
                            .px(px(10.0))
                            .py(px(6.0))
                            .text_color(theme().text_faint)
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
                        .text_color(theme().text_faint)
                        .child("No active session"),
                );
            }

            col
        };

        let preview_col = self.render_preview_col(root.as_deref(), cx);

        let mut root = div()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme().bg_base)
            .child(tree_col)
            .child(preview_col);

        root = root.child(self.render_reader_context_menu(cx));

        root
    }

    /// Max source lines rendered at once. Beyond this the tail is dropped with
    /// a notice — keeps a 500 KB minified file from stalling the frame.
    const MAX_RENDER_LINES: usize = 6000;

    /// Right-hand preview column: header (breadcrumb + actions + find) over a
    /// scrollable body that dispatches on the loaded [`PreviewKind`].
    fn render_preview_col(&self, root: Option<&std::path::Path>, cx: &mut Context<Self>) -> impl IntoElement {
        let base = div()
            .id("reader-preview-scroll")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .bg(theme().bg_base);

        let Some(preview) = self.reader.preview.as_ref() else {
            return base
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .font_family(crate::theme::FONT_UI)
                .text_color(theme().text_faint)
                .child(div().text_size(px(13.0)).child("Select a file to read"))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme().text_ghost)
                        .child("Reader is read-only — editing opens in your external editor"),
                );
        };

        let header = self.render_preview_header(preview, root, cx);
        let body: AnyElement = match &preview.kind {
            PreviewKind::Text(contents) => {
                if is_markdown(&preview.path) && !self.reader.md_view_source {
                    self.render_markdown_body(contents).into_any_element()
                } else {
                    self.render_source_body(&preview.path, contents).into_any_element()
                }
            }
            PreviewKind::Binary => {
                degraded_body("Binary file", "This file isn't text and can't be previewed.")
                    .into_any_element()
            }
            PreviewKind::TooLarge(len) => degraded_body(
                "File too large to preview",
                &format!("{} exceeds the 512 KB preview limit.", human_size(*len)),
            )
            .into_any_element(),
            PreviewKind::Unreadable(msg) => {
                degraded_body("Could not read file", msg).into_any_element()
            }
        };

        base.flex().flex_col().child(header).child(body)
    }

    /// Header strip: breadcrumb of the file path, line/size stat, and the
    /// Find / Copy / Open-externally actions.
    fn render_preview_header(
        &self,
        preview: &Preview,
        root: Option<&std::path::Path>,
        cx: &mut Context<Self>,
    ) -> Div {
        let rel = root
            .and_then(|r| preview.path.strip_prefix(r).ok())
            .unwrap_or(&preview.path);
        let crumbs: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();

        let mut breadcrumb = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_wrap()
            .gap(px(3.0))
            .min_w(px(0.0))
            .flex_1()
            .text_size(px(11.0));
        for (idx, part) in crumbs.iter().enumerate() {
            let last = idx + 1 == crumbs.len();
            if idx > 0 {
                breadcrumb = breadcrumb.child(
                    div().text_color(theme().text_ghost).child("›"),
                );
            }
            breadcrumb = breadcrumb.child(
                div()
                    .text_color(if last { theme().text_primary } else { theme().text_faint })
                    .child(part.clone()),
            );
        }

        let stat = match &preview.kind {
            PreviewKind::Text(c) => format!("{} lines", c.split('\n').count()),
            PreviewKind::TooLarge(len) => human_size(*len),
            _ => String::new(),
        };

        let can_copy = matches!(preview.kind, PreviewKind::Text(_));
        let is_md = is_markdown(&preview.path);
        let md_source = self.reader.md_view_source;
        let path_for_copy = preview.path.clone();
        let path_for_open = preview.path.clone();
        let find_active = self.reader.find_active;

        let action = |id: &'static str, label: SharedString, enabled: bool| {
            let mut b = div()
                .id(id)
                .px(px(8.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .text_size(px(11.0))
                .child(label);
            if enabled {
                b = b
                    .text_color(theme().text_secondary)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme().bg_hover));
            } else {
                b = b.text_color(theme().text_ghost);
            }
            b
        };

        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(theme().bg_surface)
            .border_b_1()
            .border_color(theme().border_subtle)
            .font_family(crate::theme::FONT_UI)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .child(breadcrumb)
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.0))
                            .text_color(theme().text_ghost)
                            .child(stat),
                    )
                    .when(is_md, |row| {
                        let label: SharedString =
                            if md_source { "Rendered".into() } else { "Source".into() };
                        row.child(
                            action("reader-md-toggle", label, true)
                                .when(md_source, |b| b.bg(theme().bg_active))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this: &mut Self, _e, _window, cx| {
                                        this.reader.md_view_source = !this.reader.md_view_source;
                                        cx.notify();
                                    }),
                                ),
                        )
                    })
                    .child(
                        action("reader-find-toggle", "Find".into(), true)
                            .when(find_active, |b| b.bg(theme().bg_active))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this: &mut Self, _e, window, cx| {
                                    this.reader.find_active = !this.reader.find_active;
                                    if this.reader.find_active {
                                        this.reader_find_input.focus_handle(cx).focus(window, cx);
                                    } else {
                                        this.reader.find_query.clear();
                                    }
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        action("reader-copy", "Copy".into(), can_copy).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this: &mut Self, _e, _window, cx| {
                                if let Some(Preview { kind: PreviewKind::Text(c), .. }) =
                                    this.reader.preview.as_ref()
                                {
                                    let _ = &path_for_copy;
                                    cx.write_to_clipboard(ClipboardItem::new_string(c.clone()));
                                }
                            }),
                        ),
                    )
                    .child(
                        action("reader-open-external", "Open externally".into(), true)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this: &mut Self, _e, _window, _cx| {
                                    this.open_in_external_editor(&path_for_open);
                                }),
                            ),
                    ),
            )
            .when(find_active, |strip| {
                let count = self.find_match_count();
                strip.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(10.0))
                        .pb(px(6.0))
                        .child(div().w(px(220.0)).child(self.reader_find_input.clone()))
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme().text_faint)
                                .child(if self.reader.find_query.is_empty() {
                                    String::new()
                                } else {
                                    format!("{count} matches")
                                }),
                        ),
                )
            })
    }

    /// Count of in-file find matches (case-insensitive substring) across the
    /// current text preview. Zero when find is empty or the file isn't text.
    fn find_match_count(&self) -> usize {
        let q = self.reader.find_query.to_lowercase();
        if q.is_empty() {
            return 0;
        }
        match self.reader.preview.as_ref().map(|p| &p.kind) {
            Some(PreviewKind::Text(c)) => c.to_lowercase().matches(&q).count(),
            _ => 0,
        }
    }

    /// Line-numbered, syntax-highlighted source body. Matches for the active
    /// find query get a background highlight.
    fn render_source_body(&self, path: &std::path::Path, contents: &str) -> impl IntoElement {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let colors = highlight::TokenColors {
            text: theme().text_primary,
            comment: theme().text_faint,
            string: theme().success,
            keyword: theme().lavender,
            number: theme().warning,
        };
        let lines = highlight::highlight(contents, &ext, colors);
        let total = lines.len();
        let shown = total.min(Self::MAX_RENDER_LINES);
        let gutter_w = px((format!("{total}").len().max(2) as f32) * 8.0 + 16.0);
        let query = self.reader.find_query.to_lowercase();
        let hl_bg = theme().bg_bell;

        let mut body = div()
            .id("reader-source-body")
            .flex_1()
            .min_h(px(0.0))
            .overflow_scroll()
            .py(px(6.0))
            .text_size(px(12.0));

        for (idx, line) in lines.into_iter().take(shown).enumerate() {
            let line_no = idx + 1;
            let matched = !query.is_empty()
                && line.text.to_lowercase().contains(&query);
            // Defensive: GPUI panics (aborting the whole app) if run lengths
            // don't sum to the text length. Highlighting is cosmetic, so if a
            // lexer edge case ever drifts, fall back to plain text rather than
            // crash the render.
            let runs_len: usize = line.runs.iter().map(|r| r.len).sum();
            let styled = if runs_len == line.text.len() {
                StyledText::new(line.text).with_runs(line.runs)
            } else {
                StyledText::new(line.text)
            };
            let content = div()
                .flex_1()
                .min_w(px(0.0))
                .whitespace_normal()
                .font_family(crate::theme::FONT_MONO)
                .when(matched, |d| d.bg(hl_bg))
                .child(styled);
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .px(px(4.0))
                    .child(
                        div()
                            .w(gutter_w)
                            .flex_shrink_0()
                            .pr(px(10.0))
                            .text_color(theme().text_ghost)
                            .font_family(crate::theme::FONT_MONO)
                            .child(SharedString::from(line_no.to_string())),
                    )
                    .child(content),
            );
        }

        if total > shown {
            body = body.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(px(11.0))
                    .font_family(crate::theme::FONT_UI)
                    .text_color(theme().text_ghost)
                    .child(format!(
                        "… {} more lines not shown — open externally to view the full file",
                        total - shown
                    )),
            );
        }

        body
    }

    /// Rendered-Markdown body: the pulldown-cmark render (reused from the
    /// transcript renderer) beside a heading outline rail.
    fn render_markdown_body(&self, contents: &str) -> impl IntoElement {
        let font_size = self.user_settings.font_size.max(12.0);
        let outline = markdown_outline(contents);

        let rendered = div()
            .id("reader-md-scroll")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_scroll()
            .px(px(20.0))
            .py(px(14.0))
            .child(crate::rich::markdown::render(contents, false, font_size));

        let mut row = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(rendered);
        if !outline.is_empty() {
            row = row.child(render_md_outline(outline));
        }
        row
    }

    /// Floating right-click menu for the file tree. Returns an empty `Div`
    /// when no menu is open, so callers can attach unconditionally.
    /// Rendered via `deferred` so it paints on top of sibling content, and
    /// positioned in window coordinates at the click site.
    pub(crate) fn render_reader_context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div();
        let Some((path, position)) = self.reader.context_menu.clone() else {
            return root;
        };

        let item = |id: &'static str, label: &'static str, path: PathBuf, reveal: bool| {
            div()
                .id(id)
                .px(px(14.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme().text_primary)
                .cursor_pointer()
                .hover(|s| s.bg(theme().bg_hover))
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
                        this.reader.context_menu = None;
                        cx.notify();
                    }),
                )
        };

        let menu = div()
            .flex()
            .flex_col()
            .min_w(px(220.0))
            .py(px(4.0))
            .bg(theme().bg_surface)
            .border_1()
            .border_color(theme().border_default)
            .rounded(px(6.0))
            .shadow_lg()
            .child(item(
                "reader-ctx-reveal",
                "Reveal in Finder",
                path.clone(),
                true,
            ))
            .child(item(
                "reader-ctx-open-external",
                "Open in External Editor",
                path,
                false,
            ));

        root = root.child(deferred(anchored().position(position).snap_to_window().child(menu)));
        root
    }

    /// Recursively build file-tree rows starting at `dir`.
    /// Directories render with chevron icons; files as plain rows.
    pub(crate) fn collect_tree_rows(
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
            let is_expanded = self.reader.expanded_dirs.contains(&path);
            let is_selected = self.reader.selected_path.as_ref() == Some(&path);

            let tree_chevron = if is_dir {
                Some(if is_expanded {
                    crate::icon::name::CHEVRON_DOWN
                } else {
                    crate::icon::name::CHEVRON_RIGHT
                })
            } else {
                None
            };

            let row_bg = if is_selected { theme().bg_raised } else { theme().bg_surface };
            let path_for_click = path.clone();

            let row_id = *counter;
            *counter += 1;
            let path_for_right_click = path.clone();
            let row = div()
                .id(("reader-tree-row", row_id))
                .flex()
                .flex_row()
                .items_center()
                .pl(indent)
                .pr(px(8.0))
                .py(px(2.0))
                .bg(row_bg)
                .cursor_pointer()
                .hover(|s| s.bg(theme().bg_raised))
                .gap(px(4.0))
                .child(match tree_chevron {
                    Some(ch) => crate::icon::icon(ch, 11.0, theme().text_faint)
                        .into_any_element(),
                    None => div().w(px(11.0)).flex_shrink_0().into_any_element(),
                })
                .child(name)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _event, _window, cx| {
                        let p = path_for_click.clone();
                        if p.is_dir() {
                            if this.reader.expanded_dirs.contains(&p) {
                                this.reader.expanded_dirs.remove(&p);
                            } else {
                                this.reader.expanded_dirs.insert(p);
                            }
                        } else {
                            this.reader.selected_path = Some(p.clone());
                            this.load_preview(p);
                        }
                        this.reader.context_menu = None;
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this: &mut Self, event: &MouseDownEvent, _window, cx| {
                        this.reader.context_menu =
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

    /// Load a file into the preview cache. Classifies the result so the view
    /// can degrade explicitly for binary, oversized, and unreadable files.
    pub(crate) fn load_preview(&mut self, path: PathBuf) {
        const MAX: u64 = 512 * 1024;
        let kind = match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX => PreviewKind::TooLarge(meta.len()),
            Ok(_) => match std::fs::read(&path) {
                Ok(bytes) => {
                    if bytes.contains(&0) {
                        PreviewKind::Binary
                    } else {
                        PreviewKind::Text(String::from_utf8_lossy(&bytes).into_owned())
                    }
                }
                Err(e) => PreviewKind::Unreadable(format!("Could not read file: {e}")),
            },
            Err(e) => PreviewKind::Unreadable(format!("Could not stat file: {e}")),
        };
        // Selecting a new file resets the in-file find and Markdown view mode.
        self.reader.find_query.clear();
        self.reader.find_active = false;
        self.reader.md_view_source = false;
        // Record in the recents list (most-recent first, deduped, capped).
        self.reader.recent.retain(|p| p != &path);
        self.reader.recent.insert(0, path.clone());
        self.reader.recent.truncate(50);
        self.reader.preview = Some(Preview { path, kind });
    }
}

/// True for files the Reader renders as Markdown by default.
fn is_markdown(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(),
        Some("md") | Some("markdown") | Some("mdx") | Some("mdown") | Some("mkd")
    )
}

/// Extract (level, text) for every heading, for the outline rail.
fn markdown_outline(contents: &str) -> Vec<(u8, String)> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let level_num = |l: HeadingLevel| match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    };
    let parser = Parser::new_ext(contents, Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS);
    let mut out = Vec::new();
    let mut cur: Option<(u8, String)> = None;
    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => cur = Some((level_num(level), String::new())),
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, s)) = cur.as_mut() {
                    s.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(h) = cur.take() {
                    if !h.1.trim().is_empty() {
                        out.push(h);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Right-hand heading outline for the Markdown reader. Display-only for now —
/// scroll-to-heading arrives with the DEV-44 deep-link protocol.
fn render_md_outline(outline: Vec<(u8, String)>) -> impl IntoElement {
    let mut rail = div()
        .id("reader-md-outline")
        .w(px(200.0))
        .flex_shrink_0()
        .h_full()
        .overflow_y_scroll()
        .bg(theme().bg_surface)
        .border_l_1()
        .border_color(theme().border_subtle)
        .py(px(8.0))
        .font_family(crate::theme::FONT_UI)
        .child(
            div()
                .px(px(12.0))
                .pb(px(6.0))
                .text_size(px(10.0))
                .text_color(theme().text_ghost)
                .child("OUTLINE"),
        );
    for (level, text) in outline {
        let indent = 12.0 + (level.saturating_sub(1) as f32) * 10.0;
        rail = rail.child(
            div()
                .pl(px(indent))
                .pr(px(10.0))
                .py(px(2.0))
                .text_size(px(11.0))
                .text_color(if level <= 1 { theme().text_primary } else { theme().text_faint })
                .child(text),
        );
    }
    rail
}

/// Centered explanatory panel for a file that can't be rendered as source.
fn degraded_body(title: &str, detail: &str) -> Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .font_family(crate::theme::FONT_UI)
        .child(
            div()
                .text_size(px(13.0))
                .text_color(theme().text_secondary)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(theme().text_ghost)
                .child(detail.to_string()),
        )
}

/// Human-readable byte size (e.g. `1.4 MB`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}
