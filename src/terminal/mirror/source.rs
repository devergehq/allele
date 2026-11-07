//! A handle to a live `Term` that a screen mirror can capture from, and a
//! remote client with `control` can type into (DEV-1166), from any thread,
//! without keeping the terminal alive.

use std::sync::{Arc, Weak};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Term, TermMode};

use super::capture::{capture, capture_history, Capture};
use super::model::Row;

/// What a screen stream reads from the host terminal.
pub trait MirrorTerm: Send + Sync {
    /// Copy the screen. `None` once the terminal has gone (session closed).
    fn capture(&self) -> Option<Capture>;

    /// Copy the screen and scrollback rows `start..start + count` in one lock
    /// hold, as [`DiffProducer::scrollback`](super::DiffProducer::scrollback)
    /// needs.
    fn capture_with_history(&self, start: usize, count: usize) -> Option<(Capture, Vec<Row>)>;

    /// The `Term`'s `scrolling_history`, when known (see
    /// [`DiffProducer::with_history_limit`](super::DiffProducer::with_history_limit)).
    fn history_limit(&self) -> Option<usize>;

    /// Whether the program in the terminal asked for bracketed paste
    /// (`CSI ? 2004 h`). `None` once the terminal has gone.
    fn bracketed_paste(&self) -> Option<bool> {
        None
    }

    /// Write `bytes` to the PTY as one write, exactly as local typing does.
    /// False when nothing was written: the terminal has gone, or this handle
    /// was made without an input path (a read-only mirror). Callers check
    /// permission first; this does not.
    fn write_input(&self, _bytes: &[u8]) -> bool {
        false
    }
}

/// Where a [`SharedTerm`] sends input: the PTY's own writer. Returns false
/// when the PTY is no longer listening.
pub type InputSink = Box<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// A [`MirrorTerm`] over a shared alacritty `Term`. Holds it weakly: a mirror
/// must not keep a closed session's terminal alive.
pub struct SharedTerm<T> {
    term: Weak<FairMutex<Term<T>>>,
    history_limit: Option<usize>,
    input: Option<InputSink>,
}

impl<T> SharedTerm<T> {
    /// `history_limit` must be the `scrolling_history` the `Term` was built
    /// with; pass `None` if that is not known here.
    pub fn new(term: &Arc<FairMutex<Term<T>>>, history_limit: Option<usize>) -> Self {
        Self {
            term: Arc::downgrade(term),
            history_limit,
            input: None,
        }
    }

    /// Let remote input reach the PTY through `sink`. Without this the
    /// handle is read-only and [`MirrorTerm::write_input`] writes nothing.
    pub fn with_input(mut self, sink: InputSink) -> Self {
        self.input = Some(sink);
        self
    }
}

impl<T: Send> MirrorTerm for SharedTerm<T> {
    fn capture(&self) -> Option<Capture> {
        let term = self.term.upgrade()?;
        let guard = term.lock();
        Some(capture(&guard))
    }

    fn capture_with_history(&self, start: usize, count: usize) -> Option<(Capture, Vec<Row>)> {
        let term = self.term.upgrade()?;
        let guard = term.lock();
        Some((capture(&guard), capture_history(&guard, start, count)))
    }

    fn history_limit(&self) -> Option<usize> {
        self.history_limit
    }

    fn bracketed_paste(&self) -> Option<bool> {
        let term = self.term.upgrade()?;
        let bracketed = term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        Some(bracketed)
    }

    fn write_input(&self, bytes: &[u8]) -> bool {
        // A closed session's terminal is gone even if its PTY writer lingers.
        if self.term.strong_count() == 0 {
            return false;
        }
        self.input.as_ref().is_some_and(|sink| sink(bytes))
    }
}
