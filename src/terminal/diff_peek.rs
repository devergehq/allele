//! The ⌥-hover diff peek card (DEV-1262): a small floating card over the
//! terminal grid showing one hunk window of what this session changed in a
//! path, or an unchanged file's first lines. Drawn by Allele; nothing here
//! touches the PTY.

use super::path_decor::{PeekBody, PeekLineKind};
use super::terminal_view::DiffPeek;
use crate::theme::theme;
use gpui::*;

const CARD_WIDTH: f32 = 560.0;

pub(crate) fn render_peek(peek: &DiffPeek) -> AnyElement {
    let t = theme();
    let title = match &peek.changed {
        Some(changed) => format!("{}  {}", changed.rel_path, changed.chip_label()),
        None => peek
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let footer = match &peek.body {
        PeekBody::Diff { hunk, hunks, .. } if *hunks > 1 => {
            format!("hunk {hunk} of {hunks} · Esc to close")
        }
        PeekBody::FileHead(_) => "unchanged in this session · Esc to close".to_string(),
        _ => "Esc to close".to_string(),
    };

    let mut body = div()
        .flex()
        .flex_col()
        .px(px(8.0))
        .py(px(6.0))
        .text_size(px(11.0))
        .font_family(crate::theme::FONT_MONO)
        .overflow_hidden();

    let note = |text: &str| {
        div()
            .py(px(4.0))
            .text_color(t.text_faint)
            .child(text.to_string())
    };

    body = match &peek.body {
        PeekBody::Loading => body.child(note("Loading…")),
        PeekBody::Binary => body.child(note("Binary file — no diff")),
        PeekBody::Empty => body.child(note("Nothing to show")),
        PeekBody::Error(e) => body.child(note(&format!("Could not load: {e}"))),
        PeekBody::FileHead(lines) => body.children(lines.iter().map(|line| {
            div()
                .whitespace_nowrap()
                .text_color(t.text_body)
                .child(line.clone())
        })),
        PeekBody::Diff { lines, .. } => body.children(lines.iter().map(|line| {
            let (prefix, color, bg) = match line.kind {
                PeekLineKind::Hunk => ("", t.accent, None),
                PeekLineKind::Added => ("+", t.success, Some(t.diff_add_bg)),
                PeekLineKind::Removed => ("-", t.danger, Some(t.diff_del_bg)),
                PeekLineKind::Context => (" ", t.text_body, None),
            };
            let row = div()
                .px(px(4.0))
                .whitespace_nowrap()
                .text_color(color)
                .child(format!("{prefix}{}", line.text));
            match bg {
                Some(bg) => row.bg(bg),
                None => row,
            }
        })),
    };

    let card = div()
        .w(px(CARD_WIDTH))
        .flex()
        .flex_col()
        .bg(t.bg_surface)
        .border_1()
        .border_color(t.border_default)
        .rounded(px(6.0))
        .shadow_md()
        .overflow_hidden()
        .child(
            div()
                .px(px(10.0))
                .py(px(5.0))
                .bg(t.bg_raised)
                .text_size(px(11.0))
                .font_weight(FontWeight::BOLD)
                .text_color(t.text_primary)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(title),
        )
        .child(body)
        .child(
            div()
                .px(px(10.0))
                .py(px(3.0))
                .border_t_1()
                .border_color(t.border_subtle)
                .text_size(px(10.0))
                .text_color(t.text_faint)
                .child(footer),
        );

    deferred(
        anchored()
            .position(peek.position + point(px(12.0), px(16.0)))
            .snap_to_window()
            .child(card),
    )
    .with_priority(1)
    .into_any_element()
}
