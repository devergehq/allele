//! A handle to a live `Term` that a screen mirror can capture from, from any
//! thread, without keeping the terminal alive.

use std::sync::{Arc, Weak};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;

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
}

/// A [`MirrorTerm`] over a shared alacritty `Term`. Holds it weakly: a mirror
/// must not keep a closed session's terminal alive.
pub struct SharedTerm<T> {
    term: Weak<FairMutex<Term<T>>>,
    history_limit: Option<usize>,
}

impl<T> SharedTerm<T> {
    /// `history_limit` must be the `scrolling_history` the `Term` was built
    /// with; pass `None` if that is not known here.
    pub fn new(term: &Arc<FairMutex<Term<T>>>, history_limit: Option<usize>) -> Self {
        Self {
            term: Arc::downgrade(term),
            history_limit,
        }
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
}
