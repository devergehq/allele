//! What the grid element reads, so it can draw a local `Term` and a mirrored
//! host screen with the same code.
//!
//! The element only ever needed the grid's size, scroll state, cursor, and
//! each cell's character, colours and flags. [`GridView`] is exactly that
//! surface. A local `Term` implements it by reading its grid; a
//! [`MirroredGrid`](super::mirror::MirroredGrid) by reading the rows the host
//! sent. Because the element is a function of a `GridView` alone, the two draw
//! identically whenever their views agree, which is what the mirror tests
//! check.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;

use super::mirror::model::{Cell, Cursor, Style};

/// A cell as the renderer sees it: no zero-width or underline-colour extras,
/// because the renderer draws neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellView {
    pub c: char,
    pub style: Style,
}

impl From<&Cell> for CellView {
    fn from(cell: &Cell) -> Self {
        Self {
            c: cell.c,
            style: cell.style,
        }
    }
}

/// A read-only view of a terminal grid, in alacritty's coordinates: line 0 is
/// the top of the screen, negative lines are scrollback.
pub trait GridView {
    fn columns(&self) -> usize;
    fn screen_lines(&self) -> usize;
    /// Screen plus scrollback.
    fn total_lines(&self) -> usize;
    /// How far the view is scrolled up into scrollback.
    fn display_offset(&self) -> usize;
    fn cursor(&self) -> Cursor;
    fn cell(&self, line: i32, column: usize) -> CellView;
}

impl<T> GridView for Term<T> {
    fn columns(&self) -> usize {
        self.grid().columns()
    }

    fn screen_lines(&self) -> usize {
        self.grid().screen_lines()
    }

    fn total_lines(&self) -> usize {
        self.grid().total_lines()
    }

    fn display_offset(&self) -> usize {
        self.grid().display_offset()
    }

    fn cursor(&self) -> Cursor {
        let point = self.grid().cursor.point;
        Cursor {
            line: point.line.0.max(0) as u16,
            column: point.column.0 as u16,
            shape: self.cursor_style().shape.into(),
        }
    }

    fn cell(&self, line: i32, column: usize) -> CellView {
        let cell = &self.grid()[Line(line)][Column(column)];
        CellView {
            c: cell.c,
            style: Style {
                fg: cell.fg,
                bg: cell.bg,
                flags: cell.flags,
            },
        }
    }
}

/// Something that can lend out a [`GridView`] for one frame of drawing.
///
/// Object-safe so the element can hold either source as `Arc<dyn GridSource>`.
/// The view is only valid inside `f`: for a live `Term` that is exactly the
/// span the lock is held.
pub trait GridSource: Send + Sync {
    fn with_view(&self, f: &mut dyn FnMut(&dyn GridView));
}

impl<G: GridView + Send> GridSource for FairMutex<G> {
    fn with_view(&self, f: &mut dyn FnMut(&dyn GridView)) {
        let guard = self.lock();
        f(&*guard);
    }
}
