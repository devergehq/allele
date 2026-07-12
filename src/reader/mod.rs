//! Reader tab rendering — file-tree rows, context menu, preview pane.
//!
//! The Reader is a read-only project comprehension surface, not an in-app
//! editor: it retrieves and displays source, Markdown, and referenced
//! artifacts while editing stays in the user's external editor (DEV-43).
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 7.
//! See ARCHITECTURE.md §2 for module role.

pub(crate) mod command;
pub(crate) mod deeplink;
pub(crate) mod highlight;
pub(crate) mod ts_highlight;
pub(crate) mod index;
pub(crate) mod palette;
pub(crate) mod search;

use gpui::*;
use gpui::prelude::FluentBuilder as _;
use std::path::PathBuf;
use crate::theme::theme;

use crate::app_state::{AppState, FindMatch, Preview, PreviewKind, ReaderSelection};

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
                    self.render_markdown_body(&preview.path, contents, cx).into_any_element()
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
                                    // Always start (and leave) Find empty: clear the
                                    // query, matches, AND the input entity's text so a
                                    // stale value never lingers un-actioned.
                                    this.reader.find_query.clear();
                                    this.reader.find_matches.clear();
                                    this.reader.find_current = 0;
                                    this.reader_find_input
                                        .update(cx, |i, cx| i.set_text_silent("", cx));
                                    if this.reader.find_active {
                                        this.reader_find_input.focus_handle(cx).focus(window, cx);
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
                let total = self.reader.find_matches.len();
                let position = if self.reader.find_query.is_empty() {
                    String::new()
                } else if total == 0 {
                    "No results".to_string()
                } else {
                    format!("{} of {}", self.reader.find_current + 1, total)
                };
                let nav = |id: &'static str, glyph: &'static str| {
                    div()
                        .id(id)
                        .w(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.0))
                        .text_size(px(12.0))
                        .text_color(theme().text_secondary)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme().bg_hover))
                        .child(glyph)
                };
                strip.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(10.0))
                        .pb(px(6.0))
                        // Shift+Enter = previous, Escape = close. Plain Enter is
                        // handled via the input's Submitted event (next match).
                        .on_key_down(cx.listener(|this: &mut Self, e: &KeyDownEvent, _w, cx| {
                            match e.keystroke.key.as_str() {
                                "escape" => {
                                    this.reader.find_active = false;
                                    this.reader.find_query.clear();
                                    this.reader.find_matches.clear();
                                    cx.notify();
                                }
                                "enter" if e.keystroke.modifiers.shift => {
                                    this.find_step(-1);
                                    cx.notify();
                                }
                                _ => {}
                            }
                        }))
                        .child(div().w(px(220.0)).child(self.reader_find_input.clone()))
                        .child(
                            div()
                                .min_w(px(60.0))
                                .text_size(px(10.0))
                                .text_color(theme().text_faint)
                                .child(position),
                        )
                        .child(nav("reader-find-prev", "‹").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this: &mut Self, _e, _w, cx| {
                                this.find_step(-1);
                                cx.notify();
                            }),
                        ))
                        .child(nav("reader-find-next", "›").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this: &mut Self, _e, _w, cx| {
                                this.find_step(1);
                                cx.notify();
                            }),
                        )),
                )
            })
    }

    /// Recompute every match of the find query in the current preview, in
    /// document order. Only records matches that fall on char boundaries so the
    /// substring-highlight run split can never bisect a UTF-8 character.
    pub(crate) fn recompute_find_matches(&mut self) {
        let lower_q = self.reader.find_query.to_lowercase();
        let raw_q = self.reader.find_query.clone();
        let mut matches = Vec::new();
        const MAX_MATCHES: usize = 5000;

        if !lower_q.is_empty() {
            if let Some(Preview { kind: PreviewKind::Text(contents), .. }) =
                self.reader.preview.as_ref()
            {
                for (i, line) in contents.split('\n').enumerate() {
                    // Case-insensitive search is offset-safe only when
                    // lowercasing preserves byte length (always true for ASCII
                    // code); otherwise fall back to a case-sensitive scan on the
                    // original so byte offsets stay valid.
                    let lower_line = line.to_lowercase();
                    let (hay, needle) = if lower_line.len() == line.len() {
                        (lower_line.as_str(), lower_q.as_str())
                    } else {
                        (line, raw_q.as_str())
                    };
                    let nlen = needle.len();
                    if nlen == 0 {
                        continue;
                    }
                    let mut start = 0;
                    while let Some(pos) = hay[start..].find(needle) {
                        let abs = start + pos;
                        if line.is_char_boundary(abs) && line.is_char_boundary(abs + nlen) {
                            matches.push(FindMatch { line: i + 1, start: abs, len: nlen });
                        }
                        start = abs + nlen;
                        if matches.len() >= MAX_MATCHES || start > hay.len() {
                            break;
                        }
                    }
                    if matches.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
        }
        self.reader.find_matches = matches;
        self.reader.find_current = 0;
    }

    /// Move the current-match cursor by `delta` (wrapping) and reveal it.
    pub(crate) fn find_step(&mut self, delta: i32) {
        let n = self.reader.find_matches.len();
        if n == 0 {
            return;
        }
        let next = (self.reader.find_current as i32 + delta).rem_euclid(n as i32) as usize;
        self.reader.find_current = next;
        self.focus_current_find_match();
    }

    /// Scroll the current match into view and mark its line for emphasis.
    pub(crate) fn focus_current_find_match(&mut self) {
        if let Some(m) = self.reader.find_matches.get(self.reader.find_current) {
            let line = m.line;
            self.reader.reveal_line = Some(line);
            self.reader.source_scroll.scroll_to_item(line.saturating_sub(1));
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
            function: theme().info,
            type_: theme().accent,
            constant: theme().warning,
            property: theme().text_body,
            operator: theme().text_secondary,
            punctuation: theme().text_muted,
            variable: theme().text_primary,
        };
        let lines = highlight::highlight(contents, &ext, colors);
        let total = lines.len();
        let shown = total.min(Self::MAX_RENDER_LINES);
        let gutter_w = px((format!("{total}").len().max(2) as f32) * 8.0 + 16.0);
        let hl_bg = theme().bg_bell;
        let hl_bg_current = theme().warning;
        let reveal = self.reader.reveal_line.filter(|&l| l >= 1 && l <= shown);

        // Group find matches by line so each rendered line can highlight its
        // matched substrings (current match gets a stronger background).
        let mut line_ranges: std::collections::HashMap<usize, Vec<(usize, usize, bool)>> =
            std::collections::HashMap::new();
        let current_match = self.reader.find_current;
        for (mi, m) in self.reader.find_matches.iter().enumerate() {
            line_ranges
                .entry(m.line)
                .or_default()
                .push((m.start, m.len, mi == current_match));
        }

        let mut body = div()
            .id("reader-source-body")
            .track_scroll(&self.reader.source_scroll)
            .flex_1()
            .min_h(px(0.0))
            .overflow_scroll()
            .py(px(6.0))
            .text_size(px(12.0));

        for (idx, line) in lines.into_iter().take(shown).enumerate() {
            let line_no = idx + 1;
            let is_reveal = reveal == Some(line_no);
            // Apply find-match backgrounds to this line's runs (splitting runs
            // at match boundaries; boundaries are char-safe by construction).
            let runs = match line_ranges.get(&line_no) {
                Some(ranges) => apply_find_bg(line.runs, ranges, hl_bg, hl_bg_current),
                None => line.runs,
            };
            // Defensive: GPUI panics (aborting the whole app) if run lengths
            // don't sum to the text length. Highlighting is cosmetic, so if a
            // lexer edge case ever drifts, fall back to plain text rather than
            // crash the render.
            let runs_len: usize = runs.iter().map(|r| r.len).sum();
            let styled = if runs_len == line.text.len() {
                StyledText::new(line.text).with_runs(runs)
            } else {
                StyledText::new(line.text)
            };
            let content = div()
                .flex_1()
                .min_w(px(0.0))
                .whitespace_normal()
                .font_family(crate::theme::FONT_MONO)
                .child(styled);
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .px(px(4.0))
                    .when(is_reveal, |d| d.bg(theme().bg_raised))
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

        // Deep-link line reveal: scroll the target row into view. The handle is
        // interior-mutable, so requesting from `&self` render is fine; it takes
        // effect on the next layout pass.
        if let Some(l) = reveal {
            self.reader.source_scroll.scroll_to_item(l - 1);
        }

        body
    }

    /// Rendered-Markdown body: the pulldown-cmark render (reused from the
    /// transcript renderer) beside a clickable heading outline rail.
    fn render_markdown_body(
        &self,
        path: &std::path::Path,
        contents: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let _ = path;
        let font_size = self.user_settings.font_size.max(12.0);
        let outline = markdown_outline(contents);

        // Split the document into sections at heading boundaries so each heading
        // starts a fresh, directly-scrollable child. The outline scrolls to the
        // matching section by index (see scroll_md_to_heading / DEV-65).
        let sections = markdown_sections(contents, &outline);

        let mut rendered = div()
            .id("reader-md-scroll")
            .track_scroll(&self.reader.md_scroll)
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_scroll()
            .px(px(20.0))
            .py(px(14.0));
        for chunk in &sections {
            rendered = rendered.child(crate::rich::markdown::render(chunk, false, font_size));
        }

        let mut row = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_row()
            .child(rendered);
        if !outline.is_empty() {
            row = row.child(self.render_md_outline(outline, cx));
        }
        row
    }

    /// Scroll the rendered-Markdown body so heading `outline_idx` is at the top.
    /// Recomputes the section layout from the current preview so it matches what
    /// `render_markdown_body` built.
    fn scroll_md_to_heading(&mut self, outline_idx: usize, cx: &mut Context<Self>) {
        let Some(Preview { kind: PreviewKind::Text(contents), .. }) = self.reader.preview.as_ref()
        else {
            return;
        };
        let outline = markdown_outline(contents);
        // A leading preamble section (content before the first heading) shifts
        // every heading's child index by one.
        let has_preamble = outline
            .first()
            .map(|o| o.offset > 0 && !contents[..o.offset].trim().is_empty())
            .unwrap_or(false);
        let child = if has_preamble { outline_idx + 1 } else { outline_idx };
        self.reader.md_scroll.scroll_to_item(child);
        cx.notify();
    }

    /// Right-hand heading outline. Clicking a heading scrolls the rendered body
    /// to that section (DEV-65).
    fn render_md_outline(
        &self,
        outline: Vec<OutlineItem>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        for (i, item) in outline.into_iter().enumerate() {
            let indent = 12.0 + (item.level.saturating_sub(1) as f32) * 10.0;
            rail = rail.child(
                div()
                    .id(("md-outline-item", i))
                    .pl(px(indent))
                    .pr(px(10.0))
                    .py(px(2.0))
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme().bg_hover))
                    .text_color(if item.level <= 1 { theme().text_primary } else { theme().text_faint })
                    .child(item.text)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _window, cx| {
                            this.scroll_md_to_heading(i, cx);
                        }),
                    ),
            );
        }
        rail
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

        root = root.child(self.dismissable_popover(
            position,
            menu,
            |this: &mut Self, cx| {
                this.reader.context_menu = None;
                cx.notify();
            },
            cx,
        ));
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
        // Selecting a new file resets the in-file find, Markdown view mode, and
        // any pending deep-link line reveal (set again by reveal_file after).
        self.reader.find_query.clear();
        self.reader.find_active = false;
        self.reader.find_matches.clear();
        self.reader.find_current = 0;
        self.reader.md_view_source = false;
        self.reader.reveal_line = None;
        // Record in the recents list (most-recent first, deduped, capped).
        self.reader.recent.retain(|p| p != &path);
        self.reader.recent.insert(0, path.clone());
        self.reader.recent.truncate(50);
        self.reader.preview = Some(Preview { path, kind });
    }

    /// Keep Reader state scoped to the active session (DEV-66). Called once per
    /// frame; only does work when the active session's workspace root changes.
    /// Stashes the outgoing session's sticky selection and restores the
    /// incoming one, so each session remembers its own open file.
    pub(crate) fn sync_reader_session(&mut self) {
        let root = self.reader_workspace_root();
        if root == self.reader.active_root {
            return;
        }

        // Stash the outgoing session's sticky selection under its root.
        if let Some(old) = self.reader.active_root.take() {
            self.reader.sessions.insert(
                old,
                ReaderSelection {
                    selected_path: self.reader.selected_path.clone(),
                    expanded_dirs: self.reader.expanded_dirs.clone(),
                    md_view_source: self.reader.md_view_source,
                },
            );
        }

        // Reset transient state that must never bleed across sessions.
        self.reader.find_query.clear();
        self.reader.find_active = false;
        self.reader.find_matches.clear();
        self.reader.find_current = 0;
        self.reader.reveal_line = None;
        self.reader.context_menu = None;

        self.reader.active_root = root.clone();

        // Restore the incoming session's selection (or an empty view).
        let restored = root.and_then(|r| self.reader.sessions.get(&r).cloned());
        match restored {
            Some(sel) => {
                self.reader.expanded_dirs = sel.expanded_dirs;
                match sel.selected_path {
                    Some(p) if p.exists() => {
                        // Set selected_path so the file-tree row shows as selected
                        // (load_preview only fills the preview, not the selection).
                        self.reader.selected_path = Some(p.clone());
                        self.load_preview(p);
                        // load_preview resets md_view_source; reapply the saved mode.
                        self.reader.md_view_source = sel.md_view_source;
                    }
                    _ => {
                        self.reader.selected_path = None;
                        self.reader.preview = None;
                        self.reader.md_view_source = false;
                    }
                }
            }
            None => {
                self.reader.selected_path = None;
                self.reader.preview = None;
                self.reader.expanded_dirs.clear();
                self.reader.md_view_source = false;
            }
        }
    }
}

/// Split `runs` at find-match boundaries and paint the matched byte ranges with
/// a background colour (`bg`, or `bg_current` for the focused match). Preserves
/// each run's foreground styling and total length; all boundaries are char-safe
/// because both the incoming runs and the match ranges are char-aligned.
fn apply_find_bg(
    runs: Vec<TextRun>,
    ranges: &[(usize, usize, bool)],
    bg: Hsla,
    bg_current: Hsla,
) -> Vec<TextRun> {
    if ranges.is_empty() {
        return runs;
    }
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    let mut pos = 0usize;
    for run in runs {
        let run_end = pos + run.len;
        let mut cur = pos;
        while cur < run_end {
            // Is `cur` inside a match? If so, colour until the match (or run) end.
            if let Some(&(s, l, is_cur)) = ranges.iter().find(|&&(s, l, _)| cur >= s && cur < s + l)
            {
                let seg_end = (s + l).min(run_end);
                let mut r = run.clone();
                r.len = seg_end - cur;
                r.background_color = Some(if is_cur { bg_current } else { bg });
                out.push(r);
                cur = seg_end;
            } else {
                // Plain until the next match start (or run end).
                let seg_end = ranges
                    .iter()
                    .filter_map(|&(s, _, _)| (s > cur).then_some(s))
                    .min()
                    .unwrap_or(run_end)
                    .min(run_end);
                let mut r = run.clone();
                r.len = seg_end - cur;
                r.background_color = None;
                out.push(r);
                cur = seg_end;
            }
        }
        pos = run_end;
    }
    out
}

/// True for files the Reader renders as Markdown by default.
fn is_markdown(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(),
        Some("md") | Some("markdown") | Some("mdx") | Some("mdown") | Some("mkd")
    )
}

/// A heading in the outline: nesting level, text, 1-based source line, and the
/// byte offset where the heading starts (for section splitting).
struct OutlineItem {
    level: u8,
    text: String,
    #[allow(dead_code)]
    line: usize,
    offset: usize,
}

/// Extract every heading (with its source line and byte offset) for the outline.
fn markdown_outline(contents: &str) -> Vec<OutlineItem> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let level_num = |l: HeadingLevel| match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    };
    let line_of = |offset: usize| contents.as_bytes()[..offset].iter().filter(|b| **b == b'\n').count() + 1;
    let parser = Parser::new_ext(contents, Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS)
        .into_offset_iter();
    let mut out = Vec::new();
    let mut cur: Option<(u8, String, usize, usize)> = None;
    for (ev, range) in parser {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                cur = Some((level_num(level), String::new(), line_of(range.start), range.start))
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, s, _, _)) = cur.as_mut() {
                    s.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text, line, offset)) = cur.take() {
                    if !text.trim().is_empty() {
                        out.push(OutlineItem { level, text, line, offset });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Split `contents` into rendered-Markdown sections: a leading preamble (only if
/// non-empty) followed by one chunk per heading, starting at each heading's byte
/// offset. Boundaries are pulldown token offsets, so always char-aligned.
fn markdown_sections(contents: &str, outline: &[OutlineItem]) -> Vec<String> {
    if outline.is_empty() {
        return vec![contents.to_string()];
    }
    let mut sections = Vec::new();
    let first = outline[0].offset;
    if first > 0 && !contents[..first].trim().is_empty() {
        sections.push(contents[..first].to_string());
    }
    for (i, item) in outline.iter().enumerate() {
        let end = outline.get(i + 1).map(|n| n.offset).unwrap_or(contents.len());
        sections.push(contents[item.offset..end].to_string());
    }
    sections
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
