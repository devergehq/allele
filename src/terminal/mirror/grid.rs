//! The client's copy of a host screen, built from frames.

use std::collections::VecDeque;

use super::model::{Cell, Cursor, Row, RowDiff, ScreenFrame, ScreenSnapshot, ScrollbackPage};
use crate::errors::{AlleleError, Result};
use crate::terminal::grid_source::{CellView, GridView};

/// A host screen rebuilt from [`ScreenFrame`]s. Runs no terminal emulator:
/// it only ever holds what the host sent.
///
/// Scrollback is loaded lazily. `history_size` is how much the host holds;
/// `history` is the contiguous run of it loaded so far, nearest the screen
/// first. Lines not loaded yet render blank until their page arrives.
#[derive(Default)]
pub struct MirroredGrid {
    columns: usize,
    lines: usize,
    screen: Vec<Row>,
    history: VecDeque<Row>,
    history_size: usize,
    cursor: Cursor,
    /// The client's own scroll position, independent of the host's.
    display_offset: usize,
    /// Sequence number of the last snapshot or diff applied.
    seq: Option<u64>,
}

impl MirroredGrid {
    /// An empty mirror. It renders nothing until a snapshot arrives.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply the next frame from the stream.
    ///
    /// An error means the stream and the mirror disagree (a gap in sequence
    /// numbers, or a malformed frame). The mirror is unchanged by a failed
    /// diff; ask the host for a fresh snapshot.
    pub fn apply(&mut self, frame: &ScreenFrame) -> Result<()> {
        match frame {
            ScreenFrame::Snapshot(snap) => self.apply_snapshot(snap),
            ScreenFrame::Diff(diff) => self.apply_diff(diff),
            ScreenFrame::Scrollback(page) => self.apply_page(page),
        }
    }

    fn apply_snapshot(&mut self, snap: &ScreenSnapshot) -> Result<()> {
        let columns = snap.columns as usize;
        let lines = snap.lines as usize;
        if snap.rows.len() != lines {
            return Err(AlleleError::Mirror(format!(
                "snapshot has {} rows for {lines} lines",
                snap.rows.len()
            )));
        }
        let screen = snap
            .rows
            .iter()
            .map(|r| r.decode(columns))
            .collect::<Result<Vec<Row>>>()?;

        self.columns = columns;
        self.lines = lines;
        self.screen = screen;
        // Loaded scrollback may predate whatever made the host resend the
        // whole screen (a resize reflows it), so start again.
        self.history.clear();
        self.history_size = snap.history_size as usize;
        self.cursor = snap.cursor;
        self.display_offset = self.display_offset.min(self.history_size);
        self.seq = Some(snap.seq);
        Ok(())
    }

    fn apply_diff(&mut self, diff: &RowDiff) -> Result<()> {
        let expected = self.seq.map(|s| s + 1);
        if expected != Some(diff.seq) {
            return Err(AlleleError::Mirror(format!(
                "diff {} does not follow frame {:?}",
                diff.seq, self.seq
            )));
        }
        let scrolled = diff.scrolled as usize;
        if scrolled > self.lines {
            return Err(AlleleError::Mirror(format!(
                "diff scrolls {scrolled} of {} lines",
                self.lines
            )));
        }
        // Decode everything before touching state, so a bad diff changes nothing.
        let mut scrolled_rows = Vec::with_capacity(diff.scrolled_rows.len());
        for (line, wire) in &diff.scrolled_rows {
            let line = *line as usize;
            if line >= scrolled {
                return Err(AlleleError::Mirror(format!(
                    "scrolled row {line} is not among the {scrolled} that scrolled"
                )));
            }
            scrolled_rows.push((line, wire.decode(self.columns)?));
        }
        let mut rows = Vec::with_capacity(diff.rows.len());
        for (line, wire) in &diff.rows {
            let line = *line as usize;
            if line >= self.lines {
                return Err(AlleleError::Mirror(format!(
                    "diff row {line} is off a {}-line screen",
                    self.lines
                )));
            }
            rows.push((line, wire.decode(self.columns)?));
        }

        for (line, row) in scrolled_rows {
            self.screen[line] = row;
        }
        for row in self.screen.drain(..scrolled) {
            self.history.push_front(row);
        }
        self.screen
            .resize(self.lines, vec![Cell::default(); self.columns]);
        if diff.history_reset {
            self.history.clear();
        } else if self.display_offset > 0 {
            // Keep the client's view on the same content as lines arrive,
            // the way alacritty does for a local terminal.
            self.display_offset += scrolled;
        }
        self.history_size = diff.history_size as usize;
        self.history.truncate(self.history_size);
        self.display_offset = self.display_offset.min(self.history_size);

        for (line, row) in rows {
            self.screen[line] = row;
        }
        self.cursor = diff.cursor;
        self.seq = Some(diff.seq);
        Ok(())
    }

    fn apply_page(&mut self, page: &ScrollbackPage) -> Result<()> {
        let start = page.start as usize;
        if start > self.history.len() {
            // Not contiguous with what is loaded (scrollback was reset since
            // the request). Drop it; `wanted_page` will ask again.
            return Ok(());
        }
        // Decode first, so a bad row changes nothing.
        let rows = page
            .rows
            .iter()
            .map(|r| r.decode(self.columns))
            .collect::<Result<Vec<Row>>>()?;
        // The diffs own `history_size`: the producer flushes any pending diff
        // before a page, so the two agree, and a page never moves the view.
        let room = self.history_size.saturating_sub(start);
        for (i, row) in rows.into_iter().take(room).enumerate() {
            match self.history.get_mut(start + i) {
                Some(slot) => *slot = row,
                None => self.history.push_back(row),
            }
        }
        Ok(())
    }

    /// Scroll the client's view. Positive moves up into scrollback.
    pub fn scroll_display(&mut self, delta: i32) {
        let offset = self.display_offset as i64 + i64::from(delta);
        self.display_offset = offset.clamp(0, self.history_size as i64) as usize;
    }

    /// The next scrollback page to request, as `(start, count)`. It keeps a
    /// screenful loaded above whatever is in view, so scrolling up rarely
    /// shows a blank line waiting for its page; that also means one page is
    /// wanted straight after attach.
    pub fn wanted_page(&self, page_len: usize) -> Option<(usize, usize)> {
        let loaded = self.history.len();
        let needed = (self.display_offset + self.lines).min(self.history_size);
        (needed > loaded).then(|| (loaded, page_len.max(needed - loaded)))
    }

    /// Lines of scrollback loaded so far.
    pub fn loaded_history(&self) -> usize {
        self.history.len()
    }

    /// Lines of scrollback the host holds.
    pub fn history_size(&self) -> usize {
        self.history_size
    }

    /// A row by alacritty line number: 0.. is the screen, -1 the line above.
    /// `None` for scrollback not loaded yet, or out of range.
    pub fn row(&self, line: i32) -> Option<&Row> {
        if line >= 0 {
            self.screen.get(line as usize)
        } else {
            self.history.get((-line - 1) as usize)
        }
    }
}

impl GridView for MirroredGrid {
    fn columns(&self) -> usize {
        self.columns
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn total_lines(&self) -> usize {
        self.lines + self.history_size
    }

    fn display_offset(&self) -> usize {
        self.display_offset
    }

    fn cursor(&self) -> Cursor {
        self.cursor
    }

    fn cell(&self, line: i32, column: usize) -> CellView {
        self.row(line)
            .and_then(|r| r.get(column))
            .map_or_else(CellView::default, CellView::from)
    }
}
