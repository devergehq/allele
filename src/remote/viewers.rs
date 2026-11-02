//! Which paired machines are attached to which of this host's sessions
//! (DEV-1166), so the session's tab can say so. Remote control is never
//! silent: while any screen stream is open, the host shows
//! "<Machine> is viewing".
//!
//! A screen stream registers when it starts and holds a [`Viewing`] guard;
//! the entry goes when the guard drops, which is when the stream ends for any
//! reason (detach, closed stream or connection, view revoked, unpaired). The
//! UI polls [`Viewers::version`] to know when to redraw.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::transport::PeerId;

#[derive(Default)]
struct State {
    /// Session id → the streams attached to it.
    sessions: HashMap<String, Vec<Entry>>,
    next_key: u64,
}

struct Entry {
    key: u64,
    peer: PeerId,
    machine_name: String,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    /// Bumped on every change.
    version: AtomicU64,
}

/// The attached machines, by session. Cheap to clone; safe from any thread.
#[derive(Clone, Default)]
pub struct Viewers {
    shared: Arc<Shared>,
}

impl Viewers {
    /// Record that `peer` (shown as `machine_name`) is attached to
    /// `session_id` until the returned guard drops.
    pub(crate) fn register(&self, session_id: &str, peer: PeerId, machine_name: &str) -> Viewing {
        let key = {
            let mut state = self.lock();
            let key = state.next_key;
            state.next_key += 1;
            state
                .sessions
                .entry(session_id.to_string())
                .or_default()
                .push(Entry {
                    key,
                    peer,
                    machine_name: machine_name.to_string(),
                });
            key
        };
        self.bump();
        Viewing {
            viewers: self.clone(),
            session_id: session_id.to_string(),
            key,
        }
    }

    /// The machines attached to `session_id`, one name per machine, sorted.
    pub fn machines(&self, session_id: &str) -> Vec<String> {
        let state = self.lock();
        let Some(entries) = state.sessions.get(session_id) else {
            return Vec::new();
        };
        let mut seen: Vec<PeerId> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for entry in entries {
            if !seen.contains(&entry.peer) {
                seen.push(entry.peer);
                names.push(entry.machine_name.clone());
            }
        }
        names.sort();
        names
    }

    /// Changes whenever a machine attaches or detaches anywhere.
    pub fn version(&self) -> u64 {
        self.shared.version.load(Ordering::Acquire)
    }

    fn remove(&self, session_id: &str, key: u64) {
        {
            let mut state = self.lock();
            if let Some(entries) = state.sessions.get_mut(session_id) {
                entries.retain(|e| e.key != key);
                if entries.is_empty() {
                    state.sessions.remove(session_id);
                }
            }
        }
        self.bump();
    }

    fn bump(&self) {
        self.shared.version.fetch_add(1, Ordering::AcqRel);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// One attached screen stream. Dropping it removes the entry.
pub(crate) struct Viewing {
    viewers: Viewers,
    session_id: String,
    key: u64,
}

impl Drop for Viewing {
    fn drop(&mut self) {
        self.viewers.remove(&self.session_id, self.key);
    }
}

/// The chip text for a session's tab, or `None` when nobody is attached.
pub fn chip_text(machines: &[String]) -> Option<String> {
    match machines {
        [] => None,
        [one] => Some(format!("{one} is viewing")),
        [a, b] => Some(format!("{a} and {b} are viewing")),
        [first, rest @ ..] => Some(format!("{first} and {} others are viewing", rest.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> PeerId {
        PeerId::from_validated([n; 32])
    }

    #[test]
    fn an_attached_machine_is_listed_until_its_guard_drops() {
        let viewers = Viewers::default();
        let before = viewers.version();
        let guard = viewers.register("s-1", peer(1), "Laptop");
        assert_eq!(viewers.machines("s-1"), vec!["Laptop".to_string()]);
        assert!(viewers.machines("s-2").is_empty());
        let attached = viewers.version();
        assert_ne!(attached, before);

        drop(guard);
        assert!(viewers.machines("s-1").is_empty());
        assert_ne!(viewers.version(), attached);
    }

    #[test]
    fn a_machine_attached_twice_is_named_once_until_both_end() {
        let viewers = Viewers::default();
        let first = viewers.register("s-1", peer(1), "Laptop");
        let second = viewers.register("s-1", peer(1), "Laptop");
        let other = viewers.register("s-1", peer(2), "Air");
        assert_eq!(viewers.machines("s-1"), vec!["Air", "Laptop"]);
        drop(first);
        assert_eq!(viewers.machines("s-1"), vec!["Air", "Laptop"]);
        drop(second);
        assert_eq!(viewers.machines("s-1"), vec!["Air"]);
        drop(other);
        assert!(viewers.machines("s-1").is_empty());
    }

    #[test]
    fn the_chip_names_who_is_attached() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(chip_text(&[]), None);
        assert_eq!(
            chip_text(&names(&["Laptop"])).as_deref(),
            Some("Laptop is viewing")
        );
        assert_eq!(
            chip_text(&names(&["Air", "Laptop"])).as_deref(),
            Some("Air and Laptop are viewing")
        );
        assert_eq!(
            chip_text(&names(&["Air", "Laptop", "Mini"])).as_deref(),
            Some("Air and 2 others are viewing")
        );
    }
}
