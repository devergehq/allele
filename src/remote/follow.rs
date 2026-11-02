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
//!
//! **Remote ops ride the same stream (DEV-1168).** The host allows one
//! control stream per connection, and this is it, so a `projects_list` or
//! `sessions_create` from the New Session picker is queued to the follower
//! ([`Follower::ops`]) and sent as `request { id, op }`; the matching
//! `response { id }` is routed back. Frames are read by a separate task that
//! forwards them over a channel, so waiting for "a frame or a queued op"
//! never cancels a half-read frame. The grant the host states in its hello
//! is kept per host ([`HostFeeds::grant`]) so the UI can hide what this Mac
//! may not do.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::control::HostName;
use super::link::{LinkState, PeerLink, ReconnectPolicy};
use super::pairing::host::HELLO_TIMEOUT;
use super::peers::PeerPermissions;
use super::transport::{PeerConnection, PeerId, PeerStream, PeerTransport};
use super::wire::{read_frame_within, write_frame, Frame, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::dispatch::protocol::{Request, Response};
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

/// Ops queued to one host before more are refused as busy. A human clicks
/// far slower than this; it bounds a stuck host, not normal use.
const OPS_QUEUE: usize = 8;

/// Frames read ahead of the follower's loop.
const FRAMES_AHEAD: usize = 16;

/// Why a remote op got no answer from the host. The host's own refusals
/// (cap, unknown project, permission) are not here: they are a
/// [`Response::Error`] and come back as an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    /// No live control stream to the host.
    Unreachable,
    /// Too many ops already waiting for this host.
    Busy,
    /// The stream or connection ended before the answer came.
    Dropped,
    /// No answer within the caller's deadline.
    TimedOut,
}

impl fmt::Display for CallError {
    /// Worded to follow the host's name: "Studio isn't connected right now".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unreachable => "isn't connected right now",
            Self::Busy => "is still answering earlier requests; try again shortly",
            Self::Dropped => "dropped the connection before answering",
            Self::TimedOut => "didn't answer in time",
        })
    }
}

/// One op on its way to a host, with where to send the answer.
pub(crate) struct OpCall {
    pub(crate) op: Request,
    pub(crate) reply: oneshot::Sender<Result<Response, CallError>>,
}

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
    /// What each host last said this Mac may do, from its hello. Kept across
    /// drops (liveness is the feed's reachability); replaced on every hello.
    grants: Arc<Mutex<HashMap<PeerId, PeerPermissions>>>,
    changed: Arc<watch::Sender<u64>>,
    transitions: broadcast::Sender<HostTransition>,
}

impl Default for HostFeeds {
    fn default() -> Self {
        Self {
            hosts: Arc::default(),
            grants: Arc::default(),
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

    /// What `peer` last told this Mac it may do, or `None` if it never got
    /// as far as a hello. The host re-checks every request regardless; this
    /// only decides what the UI offers.
    pub fn grant(&self, peer: PeerId) -> Option<PeerPermissions> {
        self.grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&peer)
            .copied()
    }

    pub(crate) fn set_grant(&self, peer: PeerId, grant: PeerPermissions) {
        let old = self
            .grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer, grant);
        if old != Some(grant) {
            self.changed.send_modify(|n| *n = n.wrapping_add(1));
        }
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
        self.grants
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&peer);
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// One followed host: its link, the task reading its feed, and the queue its
/// remote ops go through.
pub(crate) struct Follower {
    link: PeerLink,
    task: JoinHandle<()>,
    ops: mpsc::Sender<OpCall>,
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
        let (ops, queue) = mpsc::channel(OPS_QUEUE);
        let task = rt.spawn(follow(link.subscribe(), peer, feeds, name, queue));
        Self { link, task, ops }
    }

    /// Where to queue an op for this host. Answered with
    /// [`CallError::Unreachable`] while there is no live control stream.
    pub(crate) fn ops(&self) -> mpsc::Sender<OpCall> {
        self.ops.clone()
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
    mut ops: mpsc::Receiver<OpCall>,
) {
    loop {
        let state = link.borrow_and_update().clone();
        match state {
            LinkState::Connected(conn) => {
                let end = session(&conn, peer, &feeds, &name, &mut ops).await;
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
                    let retry = refusing_ops(&mut ops, async {
                        tokio::select! {
                            _ = conn.closed() => false,
                            _ = tokio::time::sleep(pause) => true,
                        }
                    })
                    .await;
                    if retry {
                        continue;
                    }
                }
            }
            // The link gave up on a dial or lost the connection: the host is
            // unreachable from now, or from earlier if it already was.
            LinkState::Waiting { .. } => feeds.lost(peer),
            LinkState::Connecting { .. } => {}
            LinkState::Stopped => return,
        }
        if refusing_ops(&mut ops, link.changed()).await.is_err() {
            return;
        }
    }
}

/// Run `until`, answering every op queued meanwhile as unreachable: with no
/// control stream there is nobody to send it to, and a caller left waiting
/// for a reconnect would see a create land minutes after it gave up.
async fn refusing_ops<T>(ops: &mut mpsc::Receiver<OpCall>, until: impl Future<Output = T>) -> T {
    tokio::pin!(until);
    loop {
        tokio::select! {
            out = &mut until => return out,
            Some(call) = ops.recv() => {
                let _ = call.reply.send(Err(CallError::Unreachable));
            }
        }
    }
}

/// Stops the frame reader when the session it feeds ends, however it ends.
struct Reader(JoinHandle<()>);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Hello, subscribe, then fold frames into `feeds` and carry queued ops to
/// the host until something ends it.
async fn session(
    conn: &Arc<dyn PeerConnection>,
    peer: PeerId,
    feeds: &HostFeeds,
    name: &Arc<HostName>,
    ops: &mut mpsc::Receiver<OpCall>,
) -> SessionEnd {
    let Ok(stream) = conn.open_stream().await else {
        return SessionEnd::Dropped;
    };
    let PeerStream { mut send, recv } = stream;
    let hello = Frame::Hello {
        protocol: PROTOCOL_VERSION,
        machine_name: name.get().await,
        permissions: None,
    };
    if write_frame(&mut send, &hello).await.is_err() {
        return SessionEnd::Dropped;
    }
    let (frames_tx, mut frames) = mpsc::channel(FRAMES_AHEAD);
    let _reader = Reader(tokio::spawn(read_frames(Arc::clone(conn), recv, frames_tx)));

    match timeout(HELLO_TIMEOUT, frames.recv()).await {
        Ok(Some(Frame::Hello { permissions, .. })) => {
            if let Some(grant) = permissions {
                feeds.set_grant(peer, grant);
            }
        }
        Ok(Some(Frame::Refused { message, .. })) => {
            tracing::info!(
                "remote: {} refused the control stream: {message}",
                peer.short()
            );
            return SessionEnd::Refused;
        }
        _ => return SessionEnd::Dropped,
    }
    if write_frame(&mut send, &Frame::SubscribeStatus)
        .await
        .is_err()
    {
        return SessionEnd::Dropped;
    }

    let mut pending: HashMap<u64, oneshot::Sender<Result<Response, CallError>>> = HashMap::new();
    let mut next_id: u64 = 1;
    let end = loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break SessionEnd::Dropped };
                match frame {
                    Frame::StatusSnapshot { seq, sessions } => {
                        feeds.snapshot(peer, StatusSnapshot { seq, sessions })
                    }
                    Frame::StatusDelta { seq, change } => {
                        if feeds.delta(peer, StatusDelta { seq, change })
                            && write_frame(&mut send, &Frame::SubscribeStatus).await.is_err()
                        {
                            break SessionEnd::Dropped;
                        }
                    }
                    Frame::StatusRefused { message } => {
                        tracing::info!(
                            "remote: {} refused the status feed: {message}",
                            peer.short()
                        );
                        break SessionEnd::Refused;
                    }
                    Frame::Response { id, result } => match pending.remove(&id) {
                        Some(reply) => {
                            let _ = reply.send(Ok(result));
                        }
                        None => tracing::debug!(
                            "remote: {} answered request {id}, which nobody is waiting for",
                            peer.short()
                        ),
                    },
                    other => tracing::debug!(
                        "remote: {} sent an unexpected frame on the control stream: {other:?}",
                        peer.short()
                    ),
                }
            }
            Some(call) = ops.recv() => {
                // Callers that gave up (timed out) have dropped their end;
                // forget them rather than hold them for the stream's life.
                pending.retain(|_, reply| !reply.is_closed());
                let id = next_id;
                next_id += 1;
                let request = Frame::Request { id, op: call.op };
                if write_frame(&mut send, &request).await.is_err() {
                    let _ = call.reply.send(Err(CallError::Dropped));
                    break SessionEnd::Dropped;
                }
                pending.insert(id, call.reply);
            }
        }
    };
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(CallError::Dropped));
    }
    end
}

/// Read frames until the stream or the connection ends, handing each to the
/// session. Runs as its own task so the session can wait on "a frame or a
/// queued op" without ever cancelling a half-read frame.
async fn read_frames(
    conn: Arc<dyn PeerConnection>,
    mut recv: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    out: mpsc::Sender<Frame>,
) {
    loop {
        // `select!`ing on `closed()` matters: a closed in-memory connection
        // does not fail a pending read. The read is never resumed after a
        // close — this task ends.
        let frame = tokio::select! {
            frame = read_frame_within(&mut recv, MAX_PEER_FRAME) => frame.ok(),
            _ = conn.closed() => None,
        };
        let Some(frame) = frame else { return };
        if out.send(frame).await.is_err() {
            return;
        }
    }
}
