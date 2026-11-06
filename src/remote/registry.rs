//! The live trust decision: who is paired, and which of their connections are
//! open right now (DEV-1160).
//!
//! The trusted list and the open-connection table share **one lock**. That is
//! the point of this type: admitting a connection (check trust, then register
//! it) and revoking a peer (drop trust, then close what is registered) are each
//! atomic, so a connection accepted a moment before *Remove* cannot slip in
//! after the sweep and survive it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use super::peers::{PeerList, PeerPermissions, TrustedPeer};
use super::transport::{PeerConnection, PeerId};
use crate::errors::{AlleleError, Result};
use crate::repositories::PeersRepository;

/// Reason given to a peer whose connection is closed by *Remove*.
pub const REVOKED_REASON: &str = "this machine was removed";

struct Inner {
    peers: Vec<TrustedPeer>,
    live: HashMap<PeerId, Vec<Arc<dyn PeerConnection>>>,
    /// Set when `peers.json` could not be read. The registry then trusts
    /// nobody and refuses to save, so a damaged file is neither honoured nor
    /// silently overwritten — the user has to look at it.
    load_error: Option<String>,
}

/// The trusted peers and their open connections. Cheap to clone.
#[derive(Clone)]
pub struct PeerRegistry {
    inner: Arc<Mutex<Inner>>,
    repo: Arc<dyn PeersRepository>,
    /// Bumped on every change, so the UI can redraw without polling.
    changed: Arc<watch::Sender<u64>>,
}

impl PeerRegistry {
    /// Load the list from `repo`. A load failure leaves the registry empty and
    /// read-only (see [`PeerRegistry::load_error`]).
    pub(crate) fn load(repo: Arc<dyn PeersRepository>) -> Self {
        let (peers, load_error) = match repo.load() {
            Ok(list) => (list.peers, None),
            Err(e) => {
                tracing::error!("trusting no machines: {e}");
                (Vec::new(), Some(e.to_string()))
            }
        };
        Self {
            inner: Arc::new(Mutex::new(Inner {
                peers,
                live: HashMap::new(),
                load_error,
            })),
            repo,
            changed: Arc::new(watch::channel(0).0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// A receiver that wakes on every change to the list or the open
    /// connections.
    #[allow(dead_code)] // read through `RemoteService::changed`
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Why `peers.json` could not be read, if it could not.
    pub fn load_error(&self) -> Option<String> {
        self.lock().load_error.clone()
    }

    /// Every trusted peer, in pairing order.
    pub fn list(&self) -> Vec<TrustedPeer> {
        self.lock().peers.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().peers.is_empty()
    }

    pub fn get(&self, id: PeerId) -> Option<TrustedPeer> {
        self.lock().peers.iter().find(|p| p.id == id).cloned()
    }

    /// What `id` may do, read live — so a permission change applies to a
    /// connection that is already open. `None` for an unknown peer.
    pub fn permissions(&self, id: PeerId) -> Option<PeerPermissions> {
        self.get(id).map(|p| p.permissions)
    }

    /// How many connections from `id` are registered as open.
    pub fn live_count(&self, id: PeerId) -> usize {
        self.lock().live.get(&id).map_or(0, Vec::len)
    }

    /// If the connection's peer is trusted and `allow` (the *Allow remote
    /// connections* switch) is on, register the connection as open and return
    /// the peer. Atomic with [`PeerRegistry::remove`] and
    /// [`PeerRegistry::close_all`]: the switch is read under the same lock the
    /// switch-off sweep takes, so a connection is either swept or refused.
    pub fn admit(
        &self,
        conn: &Arc<dyn PeerConnection>,
        allow: &std::sync::atomic::AtomicBool,
    ) -> Option<TrustedPeer> {
        let id = conn.remote_id();
        let mut inner = self.lock();
        if !allow.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let peer = inner.peers.iter().find(|p| p.id == id).cloned()?;
        inner.live.entry(id).or_default().push(Arc::clone(conn));
        drop(inner);
        self.bump();
        Some(peer)
    }

    /// Forget a connection that has closed.
    pub fn release(&self, conn: &Arc<dyn PeerConnection>) {
        let id = conn.remote_id();
        let mut inner = self.lock();
        if let Some(conns) = inner.live.get_mut(&id) {
            conns.retain(|c| !Arc::ptr_eq(c, conn));
            if conns.is_empty() {
                inner.live.remove(&id);
            }
        }
        drop(inner);
        self.bump();
    }

    /// Write `peers` to disk, then make it the live list. Refused while the
    /// file on disk is unreadable.
    fn commit(&self, inner: &mut Inner, peers: Vec<TrustedPeer>) -> Result<()> {
        if let Some(e) = &inner.load_error {
            return Err(AlleleError::Remote(format!(
                "peers.json could not be read, so it will not be overwritten: {e}"
            )));
        }
        self.repo.save(&PeerList::new(peers.clone()))?;
        inner.peers = peers;
        Ok(())
    }

    /// Record a peer that has just paired. A peer that was already trusted
    /// keeps the permissions it had — re-pairing renames it but never grants
    /// anything new.
    pub fn add_paired(&self, peer: TrustedPeer) -> Result<()> {
        let mut inner = self.lock();
        let mut peers = inner.peers.clone();
        match peers.iter_mut().find(|p| p.id == peer.id) {
            Some(existing) => existing.machine_name = peer.machine_name,
            None => peers.push(peer),
        }
        self.commit(&mut inner, peers)?;
        drop(inner);
        self.bump();
        Ok(())
    }

    /// Grant or withdraw `control`. Takes effect on open connections at once,
    /// because permissions are read live.
    pub fn set_control(&self, id: PeerId, allowed: bool) -> Result<()> {
        let mut inner = self.lock();
        let mut peers = inner.peers.clone();
        let peer = peers
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| AlleleError::Remote(format!("{} is not paired", id.short())))?;
        peer.permissions.control = allowed;
        self.commit(&mut inner, peers)?;
        drop(inner);
        self.bump();
        Ok(())
    }

    /// Revoke `id`: drop it from the trusted list and close every open
    /// connection it has, under the same lock [`PeerRegistry::admit`] takes.
    ///
    /// Revocation happens in memory and on the wire even if writing
    /// `peers.json` fails; the write error is returned so the UI can say the
    /// removal will not survive a restart. Returns whether the peer was known.
    pub fn remove(&self, id: PeerId) -> Result<bool> {
        let mut inner = self.lock();
        let known = inner.peers.iter().any(|p| p.id == id);
        let remaining: Vec<_> = inner.peers.iter().filter(|p| p.id != id).cloned().collect();
        let saved = if known {
            self.commit(&mut inner, remaining.clone())
        } else {
            Ok(())
        };
        inner.peers = remaining;
        let conns = inner.live.remove(&id).unwrap_or_default();
        for conn in &conns {
            conn.close(REVOKED_REASON);
        }
        drop(inner);
        if known {
            tracing::info!("removed paired machine {}", id.short());
        }
        self.bump();
        saved.map(|()| known)
    }

    /// Close every registered connection — the *Allow remote connections*
    /// switch going off.
    pub fn close_all(&self, reason: &str) {
        let mut inner = self.lock();
        let all: Vec<_> = inner.live.drain().flat_map(|(_, c)| c).collect();
        drop(inner);
        for conn in all {
            conn.close(reason);
        }
        self.bump();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::testing::MemNet;
    use crate::remote::transport::PeerTransport;
    use crate::repositories::fakes::InMemoryPeersRepository;

    static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    fn registry() -> (PeerRegistry, InMemoryPeersRepository) {
        let repo = InMemoryPeersRepository::default();
        (PeerRegistry::load(Arc::new(repo.clone())), repo)
    }

    struct FailingRepo;
    impl PeersRepository for FailingRepo {
        fn load(&self) -> Result<PeerList> {
            Err(AlleleError::Remote("corrupt".into()))
        }
        fn save(&self, _: &PeerList) -> Result<()> {
            panic!("a registry that could not load must never save");
        }
    }

    #[tokio::test]
    async fn an_unknown_peer_is_not_admitted() {
        let (reg, _) = registry();
        let net = MemNet::new();
        let (a, b) = (net.endpoint(), net.endpoint());
        let _accept = b.clone();
        let conn = a.dial_mem(b.local_id()).await;
        assert!(reg.admit(&conn, &ON).is_none());
        assert_eq!(reg.live_count(conn.remote_id()), 0);
    }

    #[tokio::test]
    async fn removing_a_peer_closes_its_open_connection_at_once() {
        let (reg, repo) = registry();
        let net = MemNet::new();
        let (a, b) = (net.endpoint(), net.endpoint());
        let conn = a.dial_mem(b.local_id()).await;
        reg.add_paired(TrustedPeer::newly_paired(conn.remote_id(), "Desk"))
            .unwrap();
        assert!(reg.admit(&conn, &ON).is_some());
        assert_eq!(reg.live_count(conn.remote_id()), 1);

        assert!(reg.remove(conn.remote_id()).unwrap());
        assert!(
            conn.is_closed(),
            "revocation must close the live connection"
        );
        assert_eq!(reg.live_count(conn.remote_id()), 0);
        assert!(repo.snapshot().peers.is_empty());
        // And it cannot come back on the same or a new connection.
        assert!(reg.admit(&conn, &ON).is_none());
    }

    #[tokio::test]
    async fn re_pairing_keeps_permissions_and_never_grants_control() {
        let (reg, repo) = registry();
        let id = crate::remote::LocalIdentity::ephemeral().id();
        reg.add_paired(TrustedPeer::newly_paired(id, "Old"))
            .unwrap();
        reg.set_control(id, true).unwrap();
        reg.add_paired(TrustedPeer::newly_paired(id, "New"))
            .unwrap();
        let peer = reg.get(id).unwrap();
        assert_eq!(peer.machine_name, "New");
        assert!(peer.permissions.control, "re-pairing kept the grant");
        assert_eq!(repo.snapshot().peers.len(), 1);

        reg.set_control(id, false).unwrap();
        reg.add_paired(TrustedPeer::newly_paired(id, "Again"))
            .unwrap();
        assert!(!reg.permissions(id).unwrap().control);
    }

    #[tokio::test]
    async fn a_trusted_peer_is_not_admitted_while_the_switch_is_off() {
        let (reg, _) = registry();
        let net = MemNet::new();
        let (a, b) = (net.endpoint(), net.endpoint());
        let conn = a.dial_mem(b.local_id()).await;
        reg.add_paired(TrustedPeer::newly_paired(conn.remote_id(), "Desk"))
            .unwrap();
        let off = std::sync::atomic::AtomicBool::new(false);
        assert!(reg.admit(&conn, &off).is_none());
        assert_eq!(reg.live_count(conn.remote_id()), 0);
    }

    #[test]
    fn the_control_toggle_persists_to_peers_json() {
        let (reg, repo) = registry();
        let id = crate::remote::LocalIdentity::ephemeral().id();
        reg.add_paired(TrustedPeer::newly_paired(id, "Laptop"))
            .unwrap();
        reg.set_control(id, true).unwrap();
        assert!(repo.snapshot().peers[0].permissions.control);
        reg.set_control(id, false).unwrap();
        assert!(!repo.snapshot().peers[0].permissions.control);
    }

    #[test]
    fn an_unreadable_peers_json_trusts_nobody_and_is_never_overwritten() {
        let reg = PeerRegistry::load(Arc::new(FailingRepo));
        assert!(reg.list().is_empty());
        assert!(reg.load_error().is_some());
        let id = crate::remote::LocalIdentity::ephemeral().id();
        // FailingRepo::save panics, so reaching it fails the test.
        assert!(reg
            .add_paired(TrustedPeer::newly_paired(id, "Laptop"))
            .is_err());
        assert!(reg.get(id).is_none());
    }
}
