//! An in-memory network of `PeerTransport`s, for tests above the transport
//! seam: pairing, the listener gate and revocation run against it without a
//! socket. Streams are `tokio::io::duplex` pipes.
//!
//! Each connection counts its `accept_stream` calls, which is how a test proves
//! the listener refused a peer *before reading anything from it*.

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use super::identity::LocalIdentity;
use super::transport::{PeerConnection, PeerId, PeerStream, PeerTransport};
use crate::errors::{AlleleError, Result};

type Incoming = mpsc::UnboundedSender<Arc<MemConn>>;

/// The shared "network": every endpoint, by id.
#[derive(Default)]
pub(crate) struct MemNet {
    endpoints: Mutex<HashMap<PeerId, Incoming>>,
}

impl MemNet {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A new endpoint with a fresh identity, reachable at once.
    pub(crate) fn endpoint(self: &Arc<Self>) -> Arc<MemTransport> {
        let id = LocalIdentity::ephemeral().id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.endpoints.lock().unwrap().insert(id, tx);
        Arc::new(MemTransport {
            id,
            net: Arc::clone(self),
            incoming: tokio::sync::Mutex::new(rx),
            accepted: Mutex::new(Vec::new()),
            shut: watch::channel(false).0,
        })
    }
}

pub(crate) struct MemTransport {
    id: PeerId,
    net: Arc<MemNet>,
    incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<Arc<MemConn>>>,
    /// Every connection `accept` has handed out, for inspection.
    accepted: Mutex<Vec<Arc<MemConn>>>,
    shut: watch::Sender<bool>,
}

impl MemTransport {
    /// `dial`, unwrapped.
    pub(crate) async fn dial_mem(&self, peer: PeerId) -> Arc<dyn PeerConnection> {
        self.dial(peer).await.expect("dial")
    }

    /// The connections this endpoint has accepted so far.
    pub(crate) fn accepted(&self) -> Vec<Arc<MemConn>> {
        self.accepted.lock().unwrap().clone()
    }
}

#[async_trait]
impl PeerTransport for MemTransport {
    fn local_id(&self) -> PeerId {
        self.id
    }

    async fn dial(&self, peer: PeerId) -> Result<Arc<dyn PeerConnection>> {
        let tx = self
            .net
            .endpoints
            .lock()
            .unwrap()
            .get(&peer)
            .cloned()
            .ok_or_else(|| AlleleError::Remote(format!("{} unreachable", peer.short())))?;
        let (ours, theirs) = MemConn::pair(self.id, peer);
        tx.send(theirs)
            .map_err(|_| AlleleError::Remote("peer shut down".into()))?;
        Ok(ours)
    }

    async fn accept(&self) -> Option<Arc<dyn PeerConnection>> {
        let mut shut = self.shut.subscribe();
        let mut rx = self.incoming.lock().await;
        tokio::select! {
            conn = rx.recv() => {
                let conn = conn?;
                self.accepted.lock().unwrap().push(Arc::clone(&conn));
                Some(conn as Arc<dyn PeerConnection>)
            }
            _ = shut.wait_for(|s| *s) => None,
        }
    }

    async fn network_changed(&self) {}

    async fn shutdown(&self) {
        self.net.endpoints.lock().unwrap().remove(&self.id);
        self.shut.send_replace(true);
    }
}

/// One side of an in-memory connection.
pub(crate) struct MemConn {
    remote: PeerId,
    to_peer: mpsc::UnboundedSender<PeerStream>,
    from_peer: tokio::sync::Mutex<mpsc::UnboundedReceiver<PeerStream>>,
    /// Shared by both sides: `Some(reason)` once either closes.
    closed: Arc<watch::Sender<Option<String>>>,
    accept_stream_calls: AtomicUsize,
}

impl MemConn {
    fn pair(a: PeerId, b: PeerId) -> (Arc<Self>, Arc<Self>) {
        let (a_tx, b_rx) = mpsc::unbounded_channel();
        let (b_tx, a_rx) = mpsc::unbounded_channel();
        let closed = Arc::new(watch::channel(None).0);
        let side = |remote, to_peer, from_peer| {
            Arc::new(Self {
                remote,
                to_peer,
                from_peer: tokio::sync::Mutex::new(from_peer),
                closed: Arc::clone(&closed),
                accept_stream_calls: AtomicUsize::new(0),
            })
        };
        (side(b, a_tx, a_rx), side(a, b_tx, b_rx))
    }

    /// How many times this side called `accept_stream` — zero means nothing
    /// the peer sent was ever read.
    pub(crate) fn accept_stream_calls(&self) -> usize {
        self.accept_stream_calls.load(Ordering::SeqCst)
    }
}

fn stream_of(end: tokio::io::DuplexStream) -> PeerStream {
    let (recv, send) = tokio::io::split(end);
    PeerStream {
        send: Box::new(send),
        recv: Box::new(recv),
    }
}

#[async_trait]
impl PeerConnection for MemConn {
    fn remote_id(&self) -> PeerId {
        self.remote
    }

    async fn open_stream(&self) -> Result<PeerStream> {
        if self.is_closed() {
            return Err(AlleleError::Remote("connection closed".into()));
        }
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        self.to_peer
            .send(stream_of(theirs))
            .map_err(|_| AlleleError::Remote("connection closed".into()))?;
        Ok(stream_of(ours))
    }

    async fn accept_stream(&self) -> Result<PeerStream> {
        self.accept_stream_calls.fetch_add(1, Ordering::SeqCst);
        let mut closed = self.closed.subscribe();
        let mut rx = self.from_peer.lock().await;
        tokio::select! {
            s = rx.recv() => s.ok_or_else(|| AlleleError::Remote("connection closed".into())),
            _ = closed.wait_for(Option::is_some) => Err(AlleleError::Remote("connection closed".into())),
        }
    }

    async fn closed(&self) -> String {
        let mut rx = self.closed.subscribe();
        let reason = rx
            .wait_for(Option::is_some)
            .await
            .map(|r| r.clone().unwrap_or_default())
            .unwrap_or_default();
        reason
    }

    fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    fn close(&self, reason: &str) {
        self.closed.send_if_modified(|r| {
            if r.is_none() {
                *r = Some(reason.to_string());
                true
            } else {
                false
            }
        });
    }
}
