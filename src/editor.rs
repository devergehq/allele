//! Editor tab — file tree + file preview rendering.

use gpui::*;
use std::path::PathBuf;

use crate::app_state::AppState;
use crate::settings;
impl AppState {

/// Two-column Editor view: file tree on the left, file preview on the right.
pub(crate) fn render_editor_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

        match (&self.editor.selected_path, &self.editor.preview) {
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

    // Context menu is rendered unconditionally; it returns an empty
    // element when no menu is open.
    root = root.child(self.render_editor_context_menu(cx));

    root
}


/// Floating right-click menu for the file tree. Rendered via
/// `deferred` so it paints on top of sibling content, and positioned
/// in window coordinates at the click site. Returns an empty element
/// when no menu is open so callers don't have to gate externally.
pub(crate) fn render_editor_context_menu(&self, cx: &mut Context<Self>) -> AnyElement {
    let Some((path, position)) = self.editor.context_menu.clone() else {
        return div().into_any_element();
    };

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
                        this.platform.shell.reveal_in_files(&path);
                    } else {
                        this.open_in_external_editor(&path);
                    }
                    this.editor.context_menu = None;
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

    deferred(anchored().position(position).snap_to_window().child(menu)).into_any_element()
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
        let is_expanded = self.editor.expanded_dirs.contains(&path);
        let is_selected = self.editor.selected_path.as_ref() == Some(&path);

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
                        if this.editor.expanded_dirs.contains(&p) {
                            this.editor.expanded_dirs.remove(&p);
                        } else {
                            this.editor.expanded_dirs.insert(p);
                        }
                    } else {
                        this.editor.selected_path = Some(p.clone());
                        this.load_preview(p);
                    }
                    this.editor.context_menu = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this: &mut Self, event: &MouseDownEvent, _window, cx| {
                    this.editor.context_menu =
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

/// Spawn the user-configured external editor with `path` as an argument.
/// Defaults to Sublime Text's `subl` CLI when no override is set.
fn open_in_external_editor(&self, path: &std::path::Path) {
    let cmd = self
        .user_settings
        .external_editor_command
        .as_deref()
        .unwrap_or(settings::DEFAULT_EXTERNAL_EDITOR);
    settings::spawn_external_editor(cmd, path, None);
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
    self.editor.preview = Some((path, contents));
}
}
