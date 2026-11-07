//! Remote session tabs (DEV-1167): a session on a paired Mac, opened from the
//! Machines sidebar or the attention bar, shown in the main area like a local
//! session and marked with its host's name.
//!
//! **The host's size is authoritative.** The tab draws the host's grid at the
//! host's size: a smaller window scrolls it, a larger one letterboxes it.
//!
//! **Which tab is on screen.** Every place that selects a local session sets
//! `AppState.active`; there are many. Rather than teach each to hide a remote
//! tab, a remote tab remembers the local selection it was opened over, and
//! [`RemoteTabs::settle`] hides it as soon as that selection changes. Clicking
//! the already-active local row changes nothing, so that one path hides the
//! tab explicitly ([`RemoteTabs::hide`]).
//!
//! **Lifetime.** A tab stays attached while it is open, shown or not, so going
//! back to it is instant. Closing it drops its [`RemoteScreen`], which closes
//! the screen stream (a detach). The host session is never touched.

mod actions;
mod view;

use gpui::Entity;

use crate::actions::SessionCursor;
use crate::remote::PeerId;

pub use actions::RemoteTabAction;
pub use view::{RemoteSessionView, RemoteTabEvent};

/// Which remote session a tab shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TabKey {
    pub peer: PeerId,
    pub session_id: String,
}

/// An open remote tab.
pub struct RemoteTab {
    pub key: TabKey,
    pub view: Entity<RemoteSessionView>,
}

/// Which tab, if any, is on screen, and over which local selection.
#[derive(Debug, Default, PartialEq, Eq)]
struct Focus {
    shown: Option<(TabKey, Option<SessionCursor>)>,
}

impl Focus {
    fn show(&mut self, key: TabKey, over: Option<SessionCursor>) {
        self.shown = Some((key, over));
    }

    /// Hide the tab if the local selection moved since it was shown.
    fn settle(&mut self, active: Option<SessionCursor>) {
        if matches!(&self.shown, Some((_, over)) if *over != active) {
            self.shown = None;
        }
    }

    fn key(&self) -> Option<&TabKey> {
        self.shown.as_ref().map(|(key, _)| key)
    }

    fn forget(&mut self, key: &TabKey) {
        if self.key() == Some(key) {
            self.shown = None;
        }
    }
}

/// Every open remote tab, and which one is on screen.
#[derive(Default)]
pub struct RemoteTabs {
    tabs: Vec<RemoteTab>,
    focus: Focus,
}

impl RemoteTabs {
    /// Call once per frame, before reading [`RemoteTabs::shown`]: a local
    /// selection made since the tab was shown hides it.
    pub fn settle(&mut self, active: Option<SessionCursor>) {
        self.focus.settle(active);
    }

    /// The tab on screen.
    pub fn shown(&self) -> Option<&RemoteTab> {
        let key = self.focus.key()?;
        self.tabs.iter().find(|t| &t.key == key)
    }

    pub fn shown_key(&self) -> Option<&TabKey> {
        self.shown().map(|t| &t.key)
    }

    /// Put the local session back on screen; the tab stays open.
    pub fn hide(&mut self) {
        self.focus.shown = None;
    }

    fn find(&self, key: &TabKey) -> Option<&RemoteTab> {
        self.tabs.iter().find(|t| &t.key == key)
    }

    fn show(&mut self, key: TabKey, over: Option<SessionCursor>) {
        self.focus.show(key, over);
    }

    fn insert(&mut self, tab: RemoteTab) {
        self.tabs.push(tab);
    }

    /// Close a tab. Dropping its view detaches.
    pub(super) fn close(&mut self, key: &TabKey) -> Option<RemoteTab> {
        self.focus.forget(key);
        let at = self.tabs.iter().position(|t| &t.key == key)?;
        Some(self.tabs.remove(at))
    }

    /// Close the tab on one session, if open (it was discarded).
    pub fn close_session(&mut self, peer: PeerId, session_id: &str) {
        let key = TabKey {
            peer,
            session_id: session_id.to_string(),
        };
        self.close(&key);
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &RemoteTab> {
        self.tabs.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::{Focus, SessionCursor, TabKey};

    fn key(session: &str) -> TabKey {
        thread_local! {
            static PEER: crate::remote::PeerId = crate::remote::LocalIdentity::ephemeral().id();
        }
        TabKey {
            peer: PEER.with(|p| *p),
            session_id: session.into(),
        }
    }

    fn at(p: usize, s: usize) -> Option<SessionCursor> {
        Some(SessionCursor {
            project_idx: p,
            session_idx: s,
        })
    }

    #[test]
    fn a_tab_stays_shown_while_the_local_selection_holds() {
        let mut f = Focus::default();
        f.show(key("a"), at(0, 1));
        f.settle(at(0, 1));
        assert_eq!(f.key(), Some(&key("a")));
    }

    #[test]
    fn selecting_another_local_session_hides_the_tab() {
        let mut f = Focus::default();
        f.show(key("a"), at(0, 1));
        f.settle(at(0, 2));
        assert_eq!(f.key(), None);
    }

    #[test]
    fn going_back_to_the_old_local_session_does_not_bring_the_tab_back() {
        let mut f = Focus::default();
        f.show(key("a"), at(0, 1));
        f.settle(at(0, 2));
        f.settle(at(0, 1));
        assert_eq!(f.key(), None, "a hidden tab is reopened by clicking it");
    }

    #[test]
    fn a_tab_opened_with_nothing_selected_hides_on_the_first_selection() {
        let mut f = Focus::default();
        f.show(key("a"), None);
        f.settle(None);
        assert_eq!(f.key(), Some(&key("a")));
        f.settle(at(0, 0));
        assert_eq!(f.key(), None);
    }

    #[test]
    fn closing_the_shown_tab_clears_it_and_closing_another_does_not() {
        let mut f = Focus::default();
        f.show(key("a"), None);
        f.forget(&key("b"));
        assert_eq!(f.key(), Some(&key("a")));
        f.forget(&key("a"));
        assert_eq!(f.key(), None);
    }
}
