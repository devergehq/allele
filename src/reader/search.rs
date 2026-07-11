//! Cmd+Shift+F project content & symbol search (DEV-36).
//!
//! Searches the background-built [`FileIndex`](crate::reader::index) corpus —
//! so `.gitignore` and the configured exclusions are already respected — in
//! three modes: file contents, filenames (fuzzy), and symbol definitions.
//! Runs off the UI thread with a generation guard; results deep-link to the
//! exact file and line.

use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::*;

use crate::app_state::{AppState, MainTab};
use crate::theme::theme;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SearchMode {
    Content,
    Filename,
    Symbol,
}

impl SearchMode {
    fn label(self) -> &'static str {
        match self {
            SearchMode::Content => "Content",
            SearchMode::Filename => "Filename",
            SearchMode::Symbol => "Symbol",
        }
    }
}

/// One search result: a file, a 1-based line (0 = whole-file/filename hit), and
/// a preview line to render.
#[derive(Clone)]
pub(crate) struct SearchHit {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
    pub(crate) preview: String,
}

pub(crate) struct SearchState {
    pub(crate) mode: SearchMode,
    pub(crate) query: String,
    pub(crate) results: Vec<SearchHit>,
    pub(crate) selected: usize,
    pub(crate) root: PathBuf,
    /// Bumped per run; async results with an older generation are dropped.
    pub(crate) generation: u64,
    /// True while a background scan is in flight.
    pub(crate) running: bool,
}

/// Corpus / result caps — keep search interactive on big repos.
const MAX_FILES_SCANNED: usize = 8_000;
const MAX_HITS: usize = 300;
const MAX_HITS_PER_FILE: usize = 20;
const MAX_FILE_BYTES: u64 = 512 * 1024;

impl AppState {
    pub(crate) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.reader_workspace_root() else {
            return;
        };
        self.ensure_file_index(cx);
        self.search = Some(SearchState {
            mode: SearchMode::Content,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            root,
            generation: 0,
            running: false,
        });
        self.search_input
            .update(cx, |i, cx| i.set_text_silent("", cx));
        self.search_input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(crate) fn close_search(&mut self, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn set_search_mode(&mut self, mode: SearchMode, cx: &mut Context<Self>) {
        if let Some(s) = self.search.as_mut() {
            if s.mode != mode {
                s.mode = mode;
                let q = s.query.clone();
                self.run_search(&q, cx);
            }
        }
    }

    pub(crate) fn move_search_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        if let Some(s) = self.search.as_mut() {
            if s.results.is_empty() {
                return;
            }
            let len = s.results.len() as i32;
            let mut i = s.selected as i32 + delta;
            if i < 0 {
                i = len - 1;
            } else if i >= len {
                i = 0;
            }
            s.selected = i as usize;
            cx.notify();
        }
    }

    /// Open the highlighted hit in the Reader at its file, highlighting the
    /// query. Precise scroll-to-line lands with the DEV-44 deep-link protocol.
    pub(crate) fn confirm_search(&mut self, cx: &mut Context<Self>) {
        let Some((path, query)) = self.search.as_ref().and_then(|s| {
            s.results
                .get(s.selected)
                .map(|h| (h.path.clone(), s.query.clone()))
        }) else {
            return;
        };
        self.reader.selected_path = Some(path.clone());
        self.main_tab = MainTab::Reader;
        self.load_preview(path);
        if !query.is_empty() {
            self.reader.find_query = query;
            self.reader.find_active = true;
        }
        self.search = None;
        cx.notify();
    }

    /// Kick a background scan for `query` in the current mode.
    pub(crate) fn run_search(&mut self, query: &str, cx: &mut Context<Self>) {
        let Some(state) = self.search.as_mut() else {
            return;
        };
        state.query = query.to_string();
        let q = query.trim().to_string();
        if q.is_empty() {
            state.results.clear();
            state.selected = 0;
            state.running = false;
            cx.notify();
            return;
        }
        state.generation += 1;
        let generation = state.generation;
        state.running = true;
        let mode = state.mode;
        let root = state.root.clone();
        let files = self.file_index.files.clone();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let hits = cx
                .background_executor()
                .spawn(async move { scan(&files, &root, &q, mode) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if let Some(s) = this.search.as_mut() {
                    if s.generation != generation {
                        return;
                    }
                    s.results = hits;
                    s.selected = 0;
                    s.running = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div();
        let Some(state) = self.search.as_ref() else {
            return root;
        };

        // Mode toggle.
        let mode_btn = |mode: SearchMode, active: bool| {
            div()
                .id(match mode {
                    SearchMode::Content => "search-mode-content",
                    SearchMode::Filename => "search-mode-filename",
                    SearchMode::Symbol => "search-mode-symbol",
                })
                .px(px(10.0))
                .py(px(3.0))
                .rounded(px(4.0))
                .text_size(px(11.0))
                .cursor_pointer()
                .when(active, |d| d.bg(theme().bg_active).text_color(theme().text_primary))
                .when(!active, |d| d.text_color(theme().text_faint).hover(|s| s.bg(theme().bg_hover)))
                .child(mode.label())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut Self, _e, _w, cx| this.set_search_mode(mode, cx)),
                )
        };

        let mut list = div().flex().flex_col().py(px(4.0));
        if state.results.is_empty() {
            let msg = if state.running {
                "Searching…"
            } else if state.query.trim().is_empty() {
                "Type to search file contents, names, or symbols"
            } else {
                "No matches"
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
        for (row, hit) in state.results.iter().enumerate() {
            let rel = hit.path.strip_prefix(&state.root).unwrap_or(&hit.path);
            let loc = if hit.line > 0 {
                format!("{}:{}", rel.to_string_lossy(), hit.line)
            } else {
                rel.to_string_lossy().into_owned()
            };
            let selected = row == state.selected;
            list = list.child(
                div()
                    .id(("search-row", row))
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .px(px(12.0))
                    .py(px(4.0))
                    .when(selected, |d| d.bg(theme().bg_active))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme().bg_hover))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme().text_ghost)
                            .child(loc),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme().text_primary)
                            .font_family(crate::theme::FONT_MONO)
                            .child(truncate(&hit.preview, 160)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this: &mut Self, _e, _w, cx| {
                            if let Some(s) = this.search.as_mut() {
                                s.selected = row;
                            }
                            this.confirm_search(cx);
                        }),
                    ),
            );
        }

        let panel = div()
            .w(px(620.0))
            .max_h(px(480.0))
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
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .p(px(8.0))
                    .border_b_1()
                    .border_color(theme().border_subtle)
                    .child(div().flex_1().min_w(px(0.0)).child(self.search_input.clone()))
                    .child(mode_btn(SearchMode::Content, state.mode == SearchMode::Content))
                    .child(mode_btn(SearchMode::Filename, state.mode == SearchMode::Filename))
                    .child(mode_btn(SearchMode::Symbol, state.mode == SearchMode::Symbol)),
            )
            .child(
                div()
                    .id("search-results")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(list),
            );

        root = root.child(
            deferred(
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
                        cx.listener(|this: &mut Self, _e, _w, cx| this.close_search(cx)),
                    )
                    .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _w, cx| {
                        match event.keystroke.key.as_str() {
                            "escape" => this.close_search(cx),
                            "enter" => this.confirm_search(cx),
                            "down" => this.move_search_selection(1, cx),
                            "up" => this.move_search_selection(-1, cx),
                            _ => {}
                        }
                    }))
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                            .child(panel),
                    ),
            ),
        );
        root
    }
}

/// Run the actual scan for `query` in `mode` across `files`. Pure/blocking —
/// called on the background executor.
fn scan(files: &[PathBuf], root: &std::path::Path, query: &str, mode: SearchMode) -> Vec<SearchHit> {
    let ql = query.to_lowercase();
    let mut hits = Vec::new();

    match mode {
        SearchMode::Filename => {
            let mut scored: Vec<(i32, SearchHit)> = Vec::new();
            for path in files.iter() {
                let rel = path.strip_prefix(root).unwrap_or(path);
                let hay = rel.to_string_lossy().to_lowercase();
                if let Some(score) = super::palette::fuzzy_score(&ql, &hay) {
                    scored.push((
                        score,
                        SearchHit {
                            path: path.clone(),
                            line: 0,
                            preview: rel.to_string_lossy().into_owned(),
                        },
                    ));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            scored.truncate(MAX_HITS);
            hits = scored.into_iter().map(|(_, h)| h).collect();
        }
        SearchMode::Content | SearchMode::Symbol => {
            for path in files.iter().take(MAX_FILES_SCANNED) {
                if hits.len() >= MAX_HITS {
                    break;
                }
                let Ok(meta) = std::fs::metadata(path) else {
                    continue;
                };
                if meta.len() > MAX_FILE_BYTES {
                    continue;
                }
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                if bytes.contains(&0) {
                    continue; // binary
                }
                let text = String::from_utf8_lossy(&bytes);
                let mut per_file = 0;
                for (i, line) in text.split('\n').enumerate() {
                    if per_file >= MAX_HITS_PER_FILE || hits.len() >= MAX_HITS {
                        break;
                    }
                    let matched = match mode {
                        SearchMode::Content => line.to_lowercase().contains(&ql),
                        SearchMode::Symbol => symbol_name(line)
                            .map(|n| n.to_lowercase().contains(&ql))
                            .unwrap_or(false),
                        SearchMode::Filename => false,
                    };
                    if matched {
                        hits.push(SearchHit {
                            path: path.clone(),
                            line: i + 1,
                            preview: line.trim_end().to_string(),
                        });
                        per_file += 1;
                    }
                }
            }
        }
    }
    hits
}

/// If `line` declares a named symbol, return the name. Handles a small set of
/// language keywords after stripping common modifier prefixes. No regex.
fn symbol_name(line: &str) -> Option<&str> {
    const MODIFIERS: &[&str] = &[
        "pub(crate) ", "pub ", "export ", "default ", "async ", "static ", "public ", "private ",
        "protected ", "final ", "abstract ", "const ", "unsafe ", "extern ",
    ];
    const KEYWORDS: &[&str] = &[
        "fn ", "def ", "class ", "struct ", "enum ", "trait ", "type ", "func ", "interface ",
        "impl ", "module ", "package ", "namespace ",
    ];
    let mut t = line.trim_start();
    // Strip a bounded number of leading modifiers.
    for _ in 0..6 {
        let mut stripped = false;
        for m in MODIFIERS {
            if let Some(rest) = t.strip_prefix(m) {
                t = rest;
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    for kw in KEYWORDS {
        if let Some(rest) = t.strip_prefix(kw) {
            let name = rest.trim_start();
            let end = name
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(name.len());
            if end > 0 {
                return Some(&name[..end]);
            }
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    let t = s.trim_start();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let cut: String = t.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_name_handles_modifiers_and_keywords() {
        assert_eq!(symbol_name("pub fn parse_config() {"), Some("parse_config"));
        assert_eq!(symbol_name("    pub(crate) struct FileIndex {"), Some("FileIndex"));
        assert_eq!(symbol_name("export default class App extends X {"), Some("App"));
        assert_eq!(symbol_name("def load_preview(self):"), Some("load_preview"));
        assert_eq!(symbol_name("let x = 3;"), None);
    }
}
