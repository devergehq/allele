//! `Endpoint`: this machine's remote endpoint as one stable handle that
//! survives rebinds (DEV-1186).
//!
//! [`RemoteService`] binds and releases the real transport as needs come and
//! go, and rebinds it when the relay setting changes. Anything that keeps a
//! transport for a long time — a [`PeerLink`] above all — holds this handle
//! instead, so after a rebind its next dial goes out through the new endpoint
//! rather than the one that was shut down.
//!
//! ```text
//!   Down ──bind──▶ Up(t1) ──relay changed──▶ Rebinding ──bind──▶ Up(t2)
//!     ▲                                          │
//!     └─────────────── bind failed / not needed ─┘
//! ```
//!
//! A dial made while `Rebinding` waits for the new transport instead of
//! failing, so a link whose connection the rebind just dropped reconnects as
//! soon as the new endpoint is up, not after a backoff.
//!
//! Accepting is not forwarded: the service's own listener accepts every
//! incoming connection on the real transport.
//!
//! [`RemoteService`]: super::service::RemoteService
//! [`PeerLink`]: super::link::PeerLink

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use super::transport::{PeerConnection, PeerId, PeerTransport};
use crate::errors::{AlleleError, Result};

/// How long a dial waits for a rebind to finish before giving up, so a
/// rebind that never completes (the service went away mid-bind) cannot hang
/// a link forever.
pub(super) const REBIND_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
enum Slot {
    /// Nothing is bound: dials fail at once.
    Down,
    /// The old transport is gone and the new one is being bound: dials wait.
    Rebinding,
    Up(Arc<dyn PeerTransport>),
}

/// The stable handle. Cheap to share as `Arc<dyn PeerTransport>`.
pub struct Endpoint {
    /// The identity every transport behind this handle binds with.
    id: PeerId,
    slot: watch::Sender<Slot>,
    /// Connections dialed through this handle, so the service can tell when
    /// the peers it was connected to before a rebind are back.
    dialed: Mutex<Vec<(PeerId, Weak<dyn PeerConnection>)>>,
}

impl Endpoint {
    /// A handle for `transport`, which is up.
    pub(super) fn new(transport: Arc<dyn PeerTransport>) -> Self {
        Self {
            id: transport.local_id(),
            slot: watch::channel(Slot::Up(transport)).0,
            dialed: Mutex::new(Vec::new()),
        }
    }

    /// Put `transport` behind the handle. Refused if it binds a different
    /// identity: peers trust this machine by id, so a handle that changed id
    /// would quietly point every link at a stranger.
    pub(super) fn install(&self, transport: Arc<dyn PeerTransport>) -> Result<()> {
        let id = transport.local_id();
        if id != self.id {
            return Err(AlleleError::Remote(format!(
                "the endpoint came back as {} instead of {}",
                id.short(),
                self.id.short()
            )));
        }
        self.slot.send_replace(Slot::Up(transport));
        Ok(())
    }

    /// The old transport is going away and a new one will follow: hold dials.
    pub(super) fn begin_rebind(&self) {
        self.slot.send_replace(Slot::Rebinding);
    }

    /// Nothing is bound and nothing is coming: fail dials, and release any
    /// that were waiting on a rebind.
    pub(super) fn set_down(&self) {
        self.slot.send_replace(Slot::Down);
    }

    /// Close every open connection dialed through this handle. The rebind
    /// does this before shutting the old transport, so links notice at once
    /// whatever the transport does with its connections on shutdown.
    pub(super) fn close_dialed(&self, reason: &str) {
        let dialed = std::mem::take(&mut *lock(&self.dialed));
        for conn in dialed.iter().filter_map(|(_, c)| c.upgrade()) {
            conn.close(reason);
        }
    }

    pub(super) fn is_rebinding(&self) -> bool {
        matches!(*self.slot.borrow(), Slot::Rebinding)
    }

    /// Peers with an open connection dialed through this handle.
    pub(super) fn live_peers(&self) -> Vec<PeerId> {
        let mut dialed = lock(&self.dialed);
        dialed.retain(|(_, c)| c.upgrade().is_some_and(|c| !c.is_closed()));
        let mut peers: Vec<PeerId> = dialed.iter().map(|(p, _)| *p).collect();
        peers.sort();
        peers.dedup();
        peers
    }

    fn is_current(&self, transport: &Arc<dyn PeerTransport>) -> bool {
        match &*self.slot.borrow() {
            Slot::Up(t) => std::ptr::addr_eq(Arc::as_ptr(t), Arc::as_ptr(transport)),
            _ => false,
        }
    }

    fn current(&self) -> Option<Arc<dyn PeerTransport>> {
        match &*self.slot.borrow() {
            Slot::Up(t) => Some(Arc::clone(t)),
            _ => None,
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

const RELAY_SUPERSEDED: &str = "the endpoint was rebound";

fn not_running() -> AlleleError {
    AlleleError::Remote("the remote endpoint is not running".into())
}

#[async_trait]
impl PeerTransport for Endpoint {
    fn local_id(&self) -> PeerId {
        self.id
    }

    async fn dial(&self, peer: PeerId) -> Result<Arc<dyn PeerConnection>> {
        let transport = {
            let mut rx = self.slot.subscribe();
            let slot =
                tokio::time::timeout(REBIND_WAIT, rx.wait_for(|s| !matches!(s, Slot::Rebinding)))
                    .await
                    .map_err(|_| not_running())?
                    .map_err(|_| not_running())?;
            match &*slot {
                Slot::Up(t) => Arc::clone(t),
                _ => return Err(not_running()),
            }
        };
        let conn = transport.dial(peer).await?;
        // Record it under the lock, and only if `transport` is still the one
        // behind the handle: a rebind switches the slot before it sweeps
        // `dialed`, so a dial that finished on the old transport after the
        // sweep is closed here instead of outliving it.
        let mut dialed = lock(&self.dialed);
        if !self.is_current(&transport) {
            drop(dialed);
            conn.close(RELAY_SUPERSEDED);
            return Err(not_running());
        }
        dialed.retain(|(_, c)| c.strong_count() > 0);
        dialed.push((peer, Arc::downgrade(&conn)));
        Ok(conn)
    }

    async fn accept(&self) -> Option<Arc<dyn PeerConnection>> {
        None // see the module docs: the service's listener accepts
    }

    async fn network_changed(&self) {
        if let Some(t) = self.current() {
            t.network_changed().await;
        }
    }

    /// Shuts down the transport behind the handle. The service owns the
    /// endpoint's lifecycle; this exists for the trait.
    async fn shutdown(&self) {
        let current = self.current();
        self.set_down();
        if let Some(t) = current {
            t.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::testing::MemNet;
    use crate::remote::LocalIdentity;
    use std::time::Duration;

    #[tokio::test]
    async fn dials_go_through_the_transport_installed_last() {
        let net = MemNet::new();
        let me = LocalIdentity::ephemeral().id();
        let peer = net.endpoint();
        let first = net.endpoint_as(me);
        let ep = Endpoint::new(first.clone());
        ep.dial(peer.local_id()).await.unwrap();
        first.shutdown().await;
        assert!(
            ep.dial(peer.local_id()).await.is_err(),
            "old transport is shut"
        );

        ep.install(net.endpoint_as(me)).unwrap();
        ep.dial(peer.local_id()).await.unwrap();
        assert_eq!(ep.local_id(), me);
    }

    #[tokio::test]
    async fn a_dial_during_a_rebind_waits_for_the_new_transport() {
        let net = MemNet::new();
        let me = LocalIdentity::ephemeral().id();
        let peer = net.endpoint();
        let ep = Arc::new(Endpoint::new(net.endpoint_as(me)));
        ep.begin_rebind();

        let dial = tokio::spawn({
            let ep = Arc::clone(&ep);
            let id = peer.local_id();
            async move { ep.dial(id).await.map(|c| c.remote_id()) }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!dial.is_finished(), "the dial waits while rebinding");

        ep.install(net.endpoint_as(me)).unwrap();
        assert_eq!(dial.await.unwrap().unwrap(), peer.local_id());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dial_gives_up_on_a_rebind_that_never_finishes() {
        let net = MemNet::new();
        let peer = net.endpoint();
        let ep = Endpoint::new(net.endpoint());
        ep.begin_rebind();
        let started = tokio::time::Instant::now();
        assert!(ep.dial(peer.local_id()).await.is_err());
        assert!(started.elapsed() >= REBIND_WAIT);
    }

    #[tokio::test]
    async fn dropped_connections_are_not_kept_in_the_dialed_list() {
        let net = MemNet::new();
        let peer = net.endpoint();
        let ep = Endpoint::new(net.endpoint());
        for _ in 0..5 {
            drop(ep.dial(peer.local_id()).await.unwrap());
        }
        let _live = ep.dial(peer.local_id()).await.unwrap();
        assert_eq!(lock(&ep.dialed).len(), 1);
    }

    #[tokio::test]
    async fn a_failed_rebind_releases_waiting_dials_with_an_error() {
        let net = MemNet::new();
        let peer = net.endpoint();
        let ep = Arc::new(Endpoint::new(net.endpoint()));
        ep.begin_rebind();
        let dial = tokio::spawn({
            let ep = Arc::clone(&ep);
            let id = peer.local_id();
            async move { ep.dial(id).await.is_err() }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        ep.set_down();
        assert!(dial.await.unwrap());
    }

    #[tokio::test]
    async fn a_transport_with_another_identity_is_refused() {
        let net = MemNet::new();
        let ep = Endpoint::new(net.endpoint());
        let before = ep.local_id();
        assert!(ep.install(net.endpoint()).is_err());
        assert_eq!(ep.local_id(), before);
    }

    #[tokio::test]
    async fn live_peers_lists_open_dialed_connections_only() {
        let net = MemNet::new();
        let a = net.endpoint();
        let b = net.endpoint();
        let ep = Endpoint::new(net.endpoint());
        let to_a = ep.dial(a.local_id()).await.unwrap();
        let _to_b = ep.dial(b.local_id()).await.unwrap();
        let mut live = ep.live_peers();
        live.sort();
        let mut want = vec![a.local_id(), b.local_id()];
        want.sort();
        assert_eq!(live, want);

        to_a.close("bye");
        assert_eq!(ep.live_peers(), vec![b.local_id()]);
    }
}
