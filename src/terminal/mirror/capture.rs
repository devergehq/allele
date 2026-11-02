//! Copying screen state out of a live `Term`.
//!
//! This is the only part of mirroring that runs while the `Term` lock is held,
//! so it does nothing but copy cells. Diffing, encoding and hashing all happen
//! on the copy, after the lock is released.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;

use super::model::{Cell, Cursor, Row};

/// A copy of everything the producer needs from one `Term` at one instant.
#[derive(Clone, Debug)]
pub struct Capture {
    pub columns: usize,
    pub lines: usize,
    pub history_size: usize,
    pub cursor: Cursor,
    /// The visible screen, top first. Independent of the host's own scroll
    /// position: the client scrolls its mirror on its own.
    pub screen: Vec<Row>,
    /// The most recent scrollback lines, nearest the screen first (host lines
    /// -1, -2, ...), up to `2 * lines + 1` of them. The producer uses them to
    /// recognise a scroll: up to `lines` rows that scrolled off, plus a
    /// `lines + 1` window of older scrollback to confirm the count.
    pub history_head: Vec<Row>,
}

/// Copy the screen and the head of scrollback. Call with the lock held.
pub fn capture<T>(term: &Term<T>) -> Capture {
    let grid = term.grid();
    let columns = grid.columns();
    let lines = grid.screen_lines();
    let history_size = grid.history_size();

    let screen = (0..lines)
        .map(|l| copy_row(term, Line(l as i32), columns))
        .collect();
    let head_len = history_size.min(2 * lines + 1);
    let history_head = (1..=head_len)
        .map(|n| copy_row(term, Line(-(n as i32)), columns))
        .collect();

    let point = grid.cursor.point;
    Capture {
        columns,
        lines,
        history_size,
        cursor: Cursor {
            line: point.line.0.max(0) as u16,
            column: point.column.0 as u16,
            shape: term.cursor_style().shape.into(),
        },
        screen,
        history_head,
    }
}

/// Copy scrollback rows `start..start + count` (0 = the line just above the
/// screen), clipped to what the host holds. Call with the lock held.
pub fn capture_history<T>(term: &Term<T>, start: usize, count: usize) -> Vec<Row> {
    let grid = term.grid();
    let columns = grid.columns();
    let end = start.saturating_add(count).min(grid.history_size());
    (start..end)
        .map(|i| copy_row(term, Line(-(i as i32) - 1), columns))
        .collect()
}

fn copy_row<T>(term: &Term<T>, line: Line, columns: usize) -> Row {
    let row = &term.grid()[line];
    (0..columns)
        .map(|c| Cell::from_term(&row[Column(c)]))
        .collect()
}
