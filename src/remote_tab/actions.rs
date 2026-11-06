//! Opening, showing and closing remote tabs (DEV-1167).

use gpui::{AppContext as _, Context, Entity, Window};

use super::{RemoteSessionView, RemoteTab, RemoteTabEvent, TabKey};
use crate::actions::PendingAction;
use crate::app_state::AppState;
use crate::remote::PeerId;

/// What a remote-tab gesture asks for.
#[derive(Debug)]
pub enum RemoteTabAction {
    /// Show this remote session, opening (attaching) its tab if needed. From
    /// a Machines sidebar row or an attention-bar row.
    Open { peer: PeerId, session_id: String },
    /// Close the tab: detach. The session keeps running on its host.
    Close(TabKey),
    /// Resize the host's terminal to this tab's window.
    FitToWindow {
        key: TabKey,
        columns: u16,
        lines: u16,
    },
}

impl From<RemoteTabAction> for PendingAction {
    fn from(a: RemoteTabAction) -> Self {
        PendingAction::RemoteTab(a)
    }
}

impl AppState {
    pub(crate) fn handle_remote_tab_action(
        &mut self,
        action: RemoteTabAction,
        skip_refocus: &mut bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            RemoteTabAction::Open { peer, session_id } => {
                let key = TabKey { peer, session_id };
                let view = match self.remote_tabs.find(&key) {
                    Some(tab) => tab.view.clone(),
                    None => self.open_remote_tab(key.clone(), window, cx),
                };
                self.remote_tabs.show(key, self.active);
                view.read(cx).focus_handle.clone().focus(window, cx);
                *skip_refocus = true;
            }
            RemoteTabAction::Close(key) => {
                let was_shown = self.remote_tabs.shown_key() == Some(&key);
                // Dropping the view drops its screen: the stream closes and
                // the host stops mirroring. Nothing is sent to the session.
                drop(self.remote_tabs.close(&key));
                if was_shown {
                    // Back to the local session that was under it; the
                    // default refocus below puts the keyboard there.
                    *skip_refocus = false;
                }
            }
            RemoteTabAction::FitToWindow {
                key,
                columns,
                lines,
            } => {
                self.remote
                    .fit_screen(key.peer, &key.session_id, columns, lines);
                *skip_refocus = true;
            }
        }
        cx.notify();
    }

    fn open_remote_tab(
        &mut self,
        key: TabKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<RemoteSessionView> {
        let (host_name, label) = self.remote_titles(&key);
        let screen = self.remote.open_screen(key.peer, &key.session_id);
        let font_size = crate::terminal::clamp_font_size(self.user_settings.font_size);
        let view =
            cx.new(|cx| RemoteSessionView::new(screen, host_name, label, font_size, window, cx));
        let event_key = key.clone();
        cx.subscribe(
            &view,
            move |this: &mut Self, _view, event: &RemoteTabEvent, cx: &mut Context<Self>| {
                let action = match event {
                    RemoteTabEvent::Close => RemoteTabAction::Close(event_key.clone()),
                    RemoteTabEvent::FitToWindow { columns, lines } => {
                        RemoteTabAction::FitToWindow {
                            key: event_key.clone(),
                            columns: *columns,
                            lines: *lines,
                        }
                    }
                };
                this.pending_action = Some(action.into());
                cx.notify();
            },
        )
        .detach();
        self.remote_tabs.insert(RemoteTab {
            key,
            view: view.clone(),
        });
        view
    }

    /// The host's display name and the session's label, as the feed last
    /// gave them. Falls back to short forms while the feed has not arrived.
    pub(crate) fn remote_titles(&self, key: &TabKey) -> (String, String) {
        let host_name = self
            .remote
            .paired()
            .into_iter()
            .find(|p| p.id == key.peer)
            .map(|p| p.machine_name)
            .unwrap_or_else(|| key.peer.short());
        let label = self
            .remote
            .hosts()
            .get(key.peer)
            .and_then(|h| h.session(&key.session_id).map(|s| s.label.clone()))
            .unwrap_or_else(|| "Remote session".to_string());
        (host_name, label)
    }

    /// Whether `peer` said, in its hello, that this Mac holds `control`.
    /// Only greys out controls; the host checks again on every request.
    pub(crate) fn remote_can_control(&self, peer: PeerId) -> bool {
        self.remote.hosts().grant(peer).is_some_and(|g| g.control)
    }

    /// Once per frame: hide a tab whose local selection moved, refresh the
    /// shown tab's titles, and close tabs with nothing left to show — their
    /// host was unpaired, or a connected host no longer lists the session.
    /// An unreachable host's tabs stay: that is the last-frame overlay.
    pub(crate) fn settle_remote_tabs(&mut self, cx: &mut Context<Self>) {
        if self.remote_tabs.is_empty() {
            return;
        }
        self.remote_tabs.settle(self.active);
        let paired: Vec<PeerId> = self.remote.paired().into_iter().map(|p| p.id).collect();
        let hosts = self.remote.hosts();
        let gone: Vec<TabKey> = self
            .remote_tabs
            .iter()
            .map(|t| t.key.clone())
            .filter(|k| {
                !paired.contains(&k.peer)
                    || hosts
                        .get(k.peer)
                        .is_some_and(|h| session_gone(&h, &k.session_id))
            })
            .collect();
        for key in gone {
            drop(self.remote_tabs.close(&key));
        }
        if let Some(tab) = self.remote_tabs.shown() {
            let key = tab.key.clone();
            let view = tab.view.clone();
            let (host, label) = self.remote_titles(&key);
            let can_fit = self.remote_can_control(key.peer);
            view.update(cx, |v, _| v.set_titles(&host, &label, can_fit));
        }
    }
}

/// A connected host's feed no longer lists the session: it was discarded or
/// closed there. Before the first snapshot nothing is known, so nothing is
/// gone.
fn session_gone(host: &crate::status_feed::RemoteHostState, session_id: &str) -> bool {
    host.reachability() == crate::status_feed::client::Reachability::Connected
        && host.session(session_id).is_none()
}

#[cfg(test)]
mod tests {
    // Explicit imports, not `use super::*`: gpui's glob would shadow `#[test]`.
    use super::{RemoteTabAction, TabKey};
    use crate::app_state::fixture::Fixture;
    use gpui::TestAppContext;

    /// A machine paired with the fixture's app, as Settings → Machines would.
    fn paired(fx: &Fixture, cx: &mut TestAppContext) -> crate::remote::PeerId {
        let peer = crate::remote::LocalIdentity::ephemeral().id();
        fx.update(cx, |state, _w, _cx| {
            state
                .remote
                .registry()
                .add_paired(crate::remote::peers::TrustedPeer::newly_paired(
                    peer, "Desk",
                ))
                .expect("pair");
        });
        peer
    }

    fn open(peer: crate::remote::PeerId, session_id: &str) -> RemoteTabAction {
        RemoteTabAction::Open {
            peer,
            session_id: session_id.into(),
        }
    }

    #[gpui::test]
    fn opening_a_remote_session_shows_its_tab(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let peer = paired(&fx, cx);
        fx.dispatch(open(peer, "s-1"), cx);

        let (shown, host) = fx.update(cx, |state, _w, cx| {
            let tab = state.remote_tabs.shown().expect("shown");
            let host = tab.view.read(cx).host_name().to_string();
            (state.remote_tabs.shown_key().cloned(), host)
        });
        assert_eq!(host, "Desk", "the tab is marked with its host's name");
        assert_eq!(
            shown,
            Some(TabKey {
                peer,
                session_id: "s-1".into()
            })
        );
    }

    #[gpui::test]
    fn opening_the_same_session_twice_reuses_its_tab(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let peer = paired(&fx, cx);
        fx.dispatch(open(peer, "s-1"), cx);
        fx.dispatch(open(peer, "s-1"), cx);
        fx.dispatch(open(peer, "s-2"), cx);

        let (count, shown) = fx.update(cx, |state, _w, _cx| {
            (
                state.remote_tabs.iter().count(),
                state.remote_tabs.shown_key().map(|k| k.session_id.clone()),
            )
        });
        assert_eq!(count, 2, "one tab per session");
        assert_eq!(
            shown.as_deref(),
            Some("s-2"),
            "the last one opened is shown"
        );
    }

    #[gpui::test]
    fn closing_a_tab_removes_it_and_shows_the_local_session(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let peer = paired(&fx, cx);
        fx.dispatch(open(peer, "s-1"), cx);
        let key = TabKey {
            peer,
            session_id: "s-1".into(),
        };
        fx.dispatch(RemoteTabAction::Close(key), cx);

        let (count, shown) = fx.update(cx, |state, _w, _cx| {
            (
                state.remote_tabs.iter().count(),
                state.remote_tabs.shown_key().is_some(),
            )
        });
        assert_eq!(count, 0);
        assert!(!shown);
    }

    #[gpui::test]
    fn a_tab_on_an_unpaired_machine_is_closed(cx: &mut TestAppContext) {
        let fx = Fixture::new(cx);
        let peer = crate::remote::LocalIdentity::ephemeral().id();
        fx.dispatch(open(peer, "s-1"), cx);

        // Nothing in the fixture is paired, as after "Remove machine".
        let count = fx.update(cx, |state, _w, cx| {
            state.settle_remote_tabs(cx);
            state.remote_tabs.iter().count()
        });
        assert_eq!(count, 0);
    }
}
