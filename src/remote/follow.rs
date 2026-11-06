//! Following paired hosts: the client half of the status feed (DEV-1162).
//!
//! For every trusted peer, a [`Follower`] runs a [`PeerLink`] and, each time
//! it connects, opens a control stream, says hello and sends
//! `subscribe_status`. What comes back is folded into that peer's
//! [`RemoteHostState`] in [`HostFeeds`], which the sidebar (DEV-1163) and
//! remote attention (DEV-1164) read.
//!
//! ```text
//!   link Connecting ─► Connected(conn) ─► hello ─► subscribe_status
//!                         │                 snapshot / deltas ─► HostFeeds
//!                         │                 gap ─► subscribe_status again
//!                         ▼
//!                       dropped ─► connection_lost(now) ─► link retries
//! ```
//!
//! Reconnecting is the link's job (backoff, sleep and wake). The follower
//! only notices each new connection and resubscribes on it, so a host that
//! sleeps and wakes comes back without anyone clicking anything.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::control::HostName;
use super::link::{LinkState, PeerLink, ReconnectPolicy};
use super::pairing::host::HELLO_TIMEOUT;
use super::transport::{PeerConnection, PeerId, PeerStream, PeerTransport};
use super::wire::{read_frame_within, write_frame, Frame, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::status_feed::client::Reachability;
use crate::status_feed::{DeltaOutcome, RemoteHostState, StatusDelta, StatusSnapshot, Transition};

/// How long to leave a host alone after it refused the feed (no `view`)
/// while the connection stays up. Long enough not to hammer it; short enough
/// that a grant given on the host shows up without a reconnect.
const REFUSED_RETRY: Duration = Duration::from_secs(30);

/// Live transitions buffered for a slow reader. A lagging reader misses
/// notifications, never state: the state is in [`HostFeeds`].
const TRANSITIONS: usize = 64;

/// Pause before reopening a stream that broke while its connection stayed up.
const STREAM_RETRY: Duration = Duration::from_secs(1);

/// A live status change on a paired host — what DEV-1164 notifies on. Only
/// deltas produce these; a reconnect snapshot never does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTransition {
    pub peer: PeerId,
    pub transition: Transition,
}

/// Every followed host's last-known state. Cheap to clone.
#[derive(Clone)]
pub struct HostFeeds {
    hosts: Arc<Mutex<HashMap<PeerId, RemoteHostState>>>,
    changed: Arc<watch::Sender<u64>>,
    transitions: broadcast::Sender<HostTransition>,
}

impl Default for HostFeeds {
    fn default() -> Self {
        Self {
            hosts: Arc::default(),
            changed: Arc::new(watch::channel(0).0),
            transitions: broadcast::channel(TRANSITIONS).0,
        }
    }
}

impl HostFeeds {
    fn lock(&self) -> MutexGuard<'_, HashMap<PeerId, RemoteHostState>> {
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One host's state, if it is followed.
    pub fn get(&self, peer: PeerId) -> Option<RemoteHostState> {
        self.lock().get(&peer).cloned()
    }

    /// Every followed host's state.
    #[allow(dead_code)] // the sidebar reads per paired peer through `get`
    pub fn all(&self) -> Vec<(PeerId, RemoteHostState)> {
        self.lock().iter().map(|(p, h)| (*p, h.clone())).collect()
    }

    /// Ticks whenever any host's state changes. For a UI that redraws.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// True when any followed host has dropped. Its "last seen" text is
    /// ageing, so the sidebar redraws it periodically.
    pub fn any_unreachable(&self) -> bool {
        self.lock()
            .values()
            .any(|h| matches!(h.reachability(), Reachability::Unreachable { .. }))
    }

    /// Live status transitions, for notifications.
    #[allow(dead_code)] // first caller: remote attention + notifications (DEV-1164)
    pub fn subscribe_transitions(&self) -> broadcast::Receiver<HostTransition> {
        self.transitions.subscribe()
    }

    fn update<R>(&self, peer: PeerId, f: impl FnOnce(&mut RemoteHostState) -> R) -> R {
        let r = f(self.lock().entry(peer).or_default());
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
        r
    }

    fn snapshot(&self, peer: PeerId, snapshot: StatusSnapshot) {
        let out = self.update(peer, |h| h.apply_snapshot(snapshot, SystemTime::now()));
        if !out.missed.is_empty() {
            // Not notified (a reconnect is not news); logged for the record.
            tracing::debug!(
                "remote: {} changed {} session(s) while unreachable",
                peer.short(),
                out.missed.len()
            );
        }
    }

    /// `true` when the store needs a fresh snapshot.
    fn delta(&self, peer: PeerId, delta: StatusDelta) -> bool {
        match self.update(peer, |h| h.apply_delta(delta, SystemTime::now())) {
            DeltaOutcome::Applied {
                transition: Some(transition),
            } => {
                let _ = self.transitions.send(HostTransition { peer, transition });
                false
            }
            DeltaOutcome::Applied { transition: None } | DeltaOutcome::Stale => false,
            // Left over from before a resubscribe; the snapshot is coming.
            DeltaOutcome::NotSubscribed => false,
            DeltaOutcome::Gap { expected, got } => {
                tracing::debug!(
                    "remote: {} status gap (wanted {expected}, got {got}); resyncing",
                    peer.short()
                );
                true
            }
            DeltaOutcome::Diverged { id } => {
                tracing::debug!(
                    "remote: {} status names unknown session {id}; resyncing",
                    peer.short()
                );
                true
            }
        }
    }

    pub(crate) fn lost(&self, peer: PeerId) {
        self.update(peer, |h| h.connection_lost(SystemTime::now()));
    }

    /// Stop following `peer` and drop what we knew (it was unpaired).
    pub(crate) fn forget(&self, peer: PeerId) {
        self.lock().remove(&peer);
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// One followed host: its link and the task reading its feed.
pub(crate) struct Follower {
    link: PeerLink,
    task: JoinHandle<()>,
}

impl Follower {
    pub(crate) fn spawn(
        rt: &tokio::runtime::Handle,
        transport: Arc<dyn PeerTransport>,
        peer: PeerId,
        policy: ReconnectPolicy,
        feeds: HostFeeds,
        name: Arc<HostName>,
    ) -> Self {
        // Listed from the start, as Connecting, so a host that never answers
        // still shows up.
        feeds.update(peer, |_| ());
        let link = PeerLink::spawn(rt, transport, peer, policy);
        let task = rt.spawn(follow(link.subscribe(), peer, feeds, name));
        Self { link, task }
    }

    pub(crate) async fn stop(self) {
        self.task.abort();
        self.link.shutdown().await;
    }
}

/// How one subscription on one connection ended.
#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    /// The stream or connection broke.
    Dropped,
    /// The host said no (no `view`, wrong protocol, not paired).
    Refused,
}

async fn follow(
    mut link: watch::Receiver<LinkState>,
    peer: PeerId,
    feeds: HostFeeds,
    name: Arc<HostName>,
) {
    loop {
        let state = link.borrow_and_update().clone();
        match state {
            LinkState::Connected(conn) => {
                let end = session(&conn, peer, &feeds, &name).await;
                feeds.lost(peer);
                if !conn.is_closed() {
                    // The stream ended but the connection is up. Pause — never
                    // resubscribe in a tight loop, least of all on a host that
                    // said no — then try again on the same connection, unless
                    // it drops meanwhile and the link takes over.
                    let pause = match end {
                        SessionEnd::Refused => REFUSED_RETRY,
                        SessionEnd::Dropped => STREAM_RETRY,
                    };
                    tokio::select! {
                        _ = conn.closed() => {}
                        _ = tokio::time::sleep(pause) => continue,
                    }
                }
            }
            // The link gave up on a dial or lost the connection: the host is
            // unreachable from now, or from earlier if it already was.
            LinkState::Waiting { .. } => feeds.lost(peer),
            LinkState::Connecting { .. } => {}
            LinkState::Stopped => return,
        }
        if link.changed().await.is_err() {
            return;
        }
    }
}

/// Hello, subscribe, then fold frames into `feeds` until something ends it.
async fn session(
    conn: &Arc<dyn PeerConnection>,
    peer: PeerId,
    feeds: &HostFeeds,
    name: &Arc<HostName>,
) -> SessionEnd {
    let Ok(mut stream) = conn.open_stream().await else {
        return SessionEnd::Dropped;
    };
    let hello = Frame::Hello {
        protocol: PROTOCOL_VERSION,
        machine_name: name.get().await,
        permissions: None,
    };
    if write_frame(&mut stream.send, &hello).await.is_err() {
        return SessionEnd::Dropped;
    }
    match timeout(HELLO_TIMEOUT, read(conn, &mut stream)).await {
        Ok(Some(Frame::Hello { .. })) => {}
        Ok(Some(Frame::Refused { message, .. })) => {
            tracing::info!(
                "remote: {} refused the control stream: {message}",
                peer.short()
            );
            return SessionEnd::Refused;
        }
        _ => return SessionEnd::Dropped,
    }
    if write_frame(&mut stream.send, &Frame::SubscribeStatus)
        .await
        .is_err()
    {
        return SessionEnd::Dropped;
    }
    loop {
        let Some(frame) = read(conn, &mut stream).await else {
            return SessionEnd::Dropped;
        };
        match frame {
            Frame::StatusSnapshot { seq, sessions } => {
                feeds.snapshot(peer, StatusSnapshot { seq, sessions })
            }
            Frame::StatusDelta { seq, change } => {
                if feeds.delta(peer, StatusDelta { seq, change })
                    && write_frame(&mut stream.send, &Frame::SubscribeStatus)
                        .await
                        .is_err()
                {
                    return SessionEnd::Dropped;
                }
            }
            Frame::StatusRefused { message } => {
                tracing::info!(
                    "remote: {} refused the status feed: {message}",
                    peer.short()
                );
                return SessionEnd::Refused;
            }
            other => tracing::debug!(
                "remote: {} sent an unexpected frame on the status stream: {other:?}",
                peer.short()
            ),
        }
    }
}

/// The next frame, or `None` once the stream or the connection is gone.
/// `select!`ing on `closed()` matters: a closed in-memory connection does not
/// fail a pending read. The read is never resumed after a close.
async fn read(conn: &Arc<dyn PeerConnection>, stream: &mut PeerStream) -> Option<Frame> {
    tokio::select! {
        frame = read_frame_within(&mut stream.recv, MAX_PEER_FRAME) => frame.ok(),
        _ = conn.closed() => None,
    }
}
