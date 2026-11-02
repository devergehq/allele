//! Turning successive captures of a `Term` into a frame stream.
//!
//! The producer remembers the last state it handed out and diffs the next
//! capture against that, the way mosh diffs against the last state the peer
//! acknowledged. That is what makes a slow consumer cheap: while it is behind
//! nobody calls [`DiffProducer::poll`], nothing queues, and the next frame it
//! takes goes straight from what it last saw to the present. Any number of
//! host updates in between merge into that one frame.
//!
//! The producer does no I/O and takes no locks. The caller decides when to
//! poll (on a timer, or when the `Term` reports a wakeup) and supplies the
//! capture closure, so the lock is only taken when a frame may be sent.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use super::capture::Capture;
use super::model::{
    Cell, Cursor, Row, RowDiff, ScreenFrame, ScreenSnapshot, ScrollbackPage, WireRow,
};

/// The frame-rate cap: at most ~30 frames a second.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// What [`DiffProducer::poll`] decided.
#[derive(Debug)]
pub enum Poll {
    /// Send this.
    Frame(ScreenFrame),
    /// Nothing changed since the last frame.
    Unchanged,
    /// Too soon after the last frame; poll again after this long.
    Wait(Duration),
}

/// The state the consumer has, as of the last frame handed out.
struct Sent {
    columns: usize,
    lines: usize,
    history_size: usize,
    cursor: Cursor,
    screen: Vec<Row>,
    /// Host lines -1, -2, ... when the frame was taken (at most `lines + 1`),
    /// to confirm a scroll count against.
    history_head: Vec<Row>,
}

pub struct DiffProducer {
    min_interval: Duration,
    /// The host's scrollback limit, if known. Below it, scrollback growth is
    /// exactly the scroll count, which removes all guesswork.
    history_limit: Option<usize>,
    last_emit: Option<Instant>,
    sent: Option<Sent>,
    seq: u64,
}

impl Default for DiffProducer {
    fn default() -> Self {
        Self::new(MIN_FRAME_INTERVAL)
    }
}

impl DiffProducer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            history_limit: None,
            last_emit: None,
            sent: None,
            seq: 0,
        }
    }

    /// Tell the producer the host `Term`'s `scrolling_history`, so scroll
    /// counts are exact until scrollback fills up.
    pub fn with_history_limit(mut self, limit: usize) -> Self {
        self.history_limit = Some(limit);
        self
    }

    /// A full snapshot, regardless of rate or what was sent before. Use on
    /// attach, and when the consumer asks to resynchronise.
    pub fn snapshot(&mut self, cap: Capture, now: Instant) -> ScreenFrame {
        self.last_emit = Some(now);
        self.emit_snapshot(cap)
    }

    /// The next frame, if one is due and anything changed. `capture` is only
    /// called when a frame is due, so the `Term` lock is not taken while the
    /// rate cap holds.
    pub fn poll(&mut self, now: Instant, capture: impl FnOnce() -> Capture) -> Poll {
        if let Some(last) = self.last_emit {
            let due = last + self.min_interval;
            if now < due {
                return Poll::Wait(due - now);
            }
        }
        match self.frame_for(capture()) {
            Some(frame) => {
                self.last_emit = Some(now);
                Poll::Frame(frame)
            }
            None => Poll::Unchanged,
        }
    }

    /// Answer a scrollback request. `cap` and `page` must come from the same
    /// lock hold. Any pending change is sent first, ignoring the rate cap, so
    /// the page's indices match the state the consumer has when it arrives.
    pub fn scrollback(
        &mut self,
        cap: Capture,
        page: Vec<Row>,
        start: usize,
        now: Instant,
    ) -> Vec<ScreenFrame> {
        let history_size = cap.history_size as u32;
        let mut frames = Vec::with_capacity(2);
        if let Some(frame) = self.frame_for(cap) {
            self.last_emit = Some(now);
            frames.push(frame);
        }
        frames.push(ScreenFrame::Scrollback(ScrollbackPage {
            start: start as u32,
            history_size,
            rows: page.iter().map(|r| WireRow::encode(r)).collect(),
        }));
        frames
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn frame_for(&mut self, cap: Capture) -> Option<ScreenFrame> {
        match &self.sent {
            Some(sent) if sent.columns == cap.columns && sent.lines == cap.lines => {
                self.emit_diff(cap)
            }
            _ => Some(self.emit_snapshot(cap)),
        }
    }

    fn emit_snapshot(&mut self, cap: Capture) -> ScreenFrame {
        let frame = ScreenSnapshot {
            seq: self.next_seq(),
            columns: cap.columns as u16,
            lines: cap.lines as u16,
            history_size: cap.history_size as u32,
            cursor: cap.cursor,
            rows: cap.screen.iter().map(|r| WireRow::encode(r)).collect(),
        };
        self.remember(cap);
        ScreenFrame::Snapshot(frame)
    }

    fn emit_diff(&mut self, cap: Capture) -> Option<ScreenFrame> {
        let sent = self.sent.as_ref()?;
        let scroll = detect_scroll(sent, &cap, self.history_limit);
        let (scrolled, history_reset) = match scroll {
            Some(k) => (k, false),
            None => (0, true),
        };

        // Rows that scrolled off but differ from what the consumer holds
        // (edited, then scrolled, within one frame).
        let scrolled_rows: Vec<(u16, WireRow)> = (0..scrolled)
            .filter_map(|i| {
                let gone = &cap.history_head[scrolled - 1 - i];
                (*gone != sent.screen[i]).then(|| (i as u16, WireRow::encode(gone)))
            })
            .collect();

        let blank: Row = vec![Cell::default(); cap.columns];
        let rows: Vec<(u16, WireRow)> = cap
            .screen
            .iter()
            .enumerate()
            .filter(|(i, row)| {
                let before = sent.screen.get(i + scrolled).unwrap_or(&blank);
                *row != before
            })
            .map(|(i, row)| (i as u16, WireRow::encode(row)))
            .collect();

        let unchanged = scrolled == 0
            && !history_reset
            && rows.is_empty()
            && cap.cursor == sent.cursor
            && cap.history_size == sent.history_size;
        if unchanged {
            return None;
        }

        let frame = RowDiff {
            seq: self.next_seq(),
            scrolled: scrolled as u16,
            scrolled_rows,
            history_reset,
            history_size: cap.history_size as u32,
            cursor: cap.cursor,
            rows,
        };
        self.remember(cap);
        Some(ScreenFrame::Diff(frame))
    }

    fn remember(&mut self, cap: Capture) {
        let mut history_head = cap.history_head;
        history_head.truncate(cap.lines + 1);
        self.sent = Some(Sent {
            columns: cap.columns,
            lines: cap.lines,
            history_size: cap.history_size,
            cursor: cap.cursor,
            screen: cap.screen,
            history_head,
        });
    }
}

/// How many lines the host scrolled since `sent`, or `None` if its scrollback
/// changed in a way a scroll count cannot describe, or the count is ambiguous.
///
/// alacritty never edits a line once it is in scrollback; it only adds lines
/// at the top or clears the lot. So the count is found by aligning
/// scrollback alone: the lines the consumer knew about (`sent.history_head`)
/// must now start at line `-k-1`. The rows that scrolled off in between need
/// not match what the consumer had, since they may have been edited first;
/// the diff sends those that differ.
///
/// While scrollback is below its limit (when the producer knows the limit),
/// its growth *is* the count. Once it is full, blank or repetitive history can
/// align at several `k`. Then the
/// candidate whose scrolled-off rows the consumer already holds wins, and if
/// that still leaves a tie the answer is `None`: the consumer drops loaded
/// scrollback and pages it in again. A refetch beats showing wrong history.
fn detect_scroll(sent: &Sent, cap: &Capture, history_limit: Option<usize>) -> Option<usize> {
    if cap.history_size < sent.history_size {
        return None; // cleared
    }
    let growth = cap.history_size - sent.history_size;
    let max_k = sent.screen.len().min(cap.history_head.len());
    if sent.history_size == 0 {
        // Nothing to align against, but nothing else can explain the growth.
        return (growth <= max_k).then_some(growth);
    }

    let sent_head: Vec<u64> = sent.history_head.iter().map(row_hash).collect();
    let head: Vec<u64> = cap.history_head.iter().map(row_hash).collect();
    let aligned = |k: usize| -> bool {
        let window = sent_head.len().min(head.len().saturating_sub(k));
        window > 0
            && (0..window).all(|j| {
                sent_head[j] == head[k + j] && sent.history_head[j] == cap.history_head[k + j]
            })
    };
    let candidates: Vec<usize> = (0..=max_k).filter(|&k| aligned(k)).collect();

    let below_limit = history_limit.is_some_and(|limit| cap.history_size < limit);
    if below_limit {
        // Nothing has been dropped off the far end, so growth is the count.
        // If the old lines do not sit where growth says, scrollback was
        // cleared and refilled in between.
        return candidates.contains(&growth).then_some(growth);
    }
    if growth > 0 && candidates.contains(&growth) {
        // Exact unless scrollback filled up during this frame, in which case
        // alignment has to decide.
        return Some(growth);
    }
    match candidates.as_slice() {
        [k] => Some(*k),
        [] => None,
        many => {
            let held =
                |k: &&usize| (0..**k).all(|i| sent.screen[i] == cap.history_head[**k - 1 - i]);
            let mut held = many.iter().filter(held);
            match (held.next(), held.next()) {
                (Some(k), None) => Some(*k),
                _ => None,
            }
        }
    }
}

fn row_hash(row: &Row) -> u64 {
    let mut h = DefaultHasher::new();
    row.hash(&mut h);
    h.finish()
}
