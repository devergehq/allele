//! Screen streams: a paired machine mirroring a session's terminal
//! (DEV-1165, proposal §3.6).
//!
//! ```text
//!   client                                       host
//!   ──────                                       ────
//!   control: attach {session_id}        ───►     view? (read now) session?
//!                                       ◄───     control: attach_failed {…}
//!                                                | opens a screen stream:
//!                                       ◄───       screen_open {session_id}
//!                                       ◄───       screen {snapshot}
//!                                       ◄───       screen {diff} …  ≤ 30/s
//!   screen: scrollback_request {start,count} ─►
//!                                       ◄───       screen {diff?, scrollback}
//!   screen: resync                      ───►
//!                                       ◄───       screen {snapshot}
//!   control: detach, or close the stream ───►    stops
//! ```
//!
//! The host sends screen *state*, not PTY bytes (see `terminal::mirror`): a
//! snapshot, then row diffs. While the link is slow the write blocks, nothing
//! is produced, and the next diff jumps from what the client last got to the
//! present, so a stalled link catches up rather than replaying a backlog.
//!
//! **Screen streams are host-opened**, so they never pass through the
//! listener's first-frame routing; `attach` arrives on the control stream and
//! [`Attachments`] (one per control stream) owns the streams it started.
//! `view` is read live: at attach, and on every tick while streaming.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::sync::FairMutex;
use async_trait::async_trait;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep_until, timeout, Instant, MissedTickBehavior};

use super::control::{allows, Need};
use super::registry::PeerRegistry;
use super::transport::{PeerConnection, PeerId, PeerStream};
use super::wire::{self, read_frame, read_frame_within, write_frame, Frame, MAX_PEER_FRAME};
use crate::dispatch::protocol::ErrorCode;
use crate::dispatch::server::ScreenLookup;
use crate::errors::{AlleleError, Result};
use crate::terminal::mirror::{
    DiffProducer, MirrorTerm, MirroredGrid, Poll, ScreenFrame, MIN_FRAME_INTERVAL,
};

/// Sessions one control stream may mirror at once.
pub const MAX_ATTACHED: usize = 8;

/// Most scrollback rows in one page. A dense row (a style change on every
/// cell) encodes to a few KiB; 100 of them stay well inside `MAX_PEER_FRAME`.
pub const MAX_PAGE_ROWS: u32 = 100;

/// How long a client waits for the host to open the screen stream.
#[allow(dead_code)]
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the host waits for the foreground thread to find a session.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

// ── host: finding a session's terminal ───────────────────────────────────

/// Finds a session's terminal for a screen stream. Production asks the
/// foreground thread; tests serve terminals from a map.
#[async_trait]
pub trait ScreenSource: Send + Sync {
    /// `Ok(None)` when there is no such session or it has no terminal (still
    /// starting, suspended); `Err` when the host could not tell, which is
    /// worth retrying.
    async fn find(&self, session_id: &str) -> Result<Option<Arc<dyn MirrorTerm>>>;
}

/// The production [`ScreenSource`]: the same foreground drain `control.sock`
/// uses, which looks the session up in `AppState`.
pub struct ScreenQueue {
    tx: std_mpsc::Sender<ScreenLookup>,
}

impl ScreenQueue {
    pub fn new(tx: std_mpsc::Sender<ScreenLookup>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl ScreenSource for ScreenQueue {
    async fn find(&self, session_id: &str) -> Result<Option<Arc<dyn MirrorTerm>>> {
        let tx = self.tx.clone();
        let session_id = session_id.to_string();
        let busy = || AlleleError::Remote("the other Mac is busy; try again".into());
        tokio::task::spawn_blocking(move || {
            let (reply, answer) = std_mpsc::channel();
            tx.send(ScreenLookup { session_id, reply })
                .map_err(|_| busy())?;
            answer.recv_timeout(LOOKUP_TIMEOUT).map_err(|_| busy())
        })
        .await
        .map_err(|_| busy())?
    }
}

/// No terminals at all (fixtures).
#[cfg(test)]
pub struct NoScreens;

#[cfg(test)]
#[async_trait]
impl ScreenSource for NoScreens {
    async fn find(&self, _: &str) -> Result<Option<Arc<dyn MirrorTerm>>> {
        Ok(None)
    }
}

// ── host: the attachments of one control stream ──────────────────────────

/// Aborts a task when dropped, so nothing a stream started outlives it.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What the control stream should do after an `attach` or `detach`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Handled {
    Continue,
    /// The peer is no longer trusted: end the connection, as for a request.
    Gone,
}

/// The screen streams one control stream has started. Dropping it stops them
/// all, which is how a closed control stream (or connection) detaches.
pub(super) struct Attachments {
    conn: Arc<dyn PeerConnection>,
    registry: PeerRegistry,
    source: Arc<dyn ScreenSource>,
    /// The control stream's outbox, for `attach_failed`.
    out: mpsc::Sender<Frame>,
    live: HashMap<String, AbortOnDrop>,
}

impl Attachments {
    pub(super) fn new(
        conn: Arc<dyn PeerConnection>,
        registry: PeerRegistry,
        source: Arc<dyn ScreenSource>,
        out: mpsc::Sender<Frame>,
    ) -> Self {
        Self {
            conn,
            registry,
            source,
            out,
            live: HashMap::new(),
        }
    }

    /// Handle an `attach` or `detach` read from the control stream.
    ///
    /// Inline on the control stream's read loop: the session lookup is
    /// bounded by `LOOKUP_TIMEOUT`, and requests already admitted keep
    /// running meanwhile.
    pub(super) async fn handle(&mut self, frame: Frame) -> Handled {
        match frame {
            Frame::Attach {
                attach_id,
                session_id,
            } => return self.attach(attach_id, session_id).await,
            Frame::Detach { session_id } => {
                self.live.remove(&session_id);
            }
            _ => {}
        }
        Handled::Continue
    }

    async fn attach(&mut self, attach_id: u64, session_id: String) -> Handled {
        let peer = self.conn.remote_id();
        let Some(trusted) = self.registry.get(peer) else {
            tracing::info!("remote: {} attach refused: no longer paired", peer.short());
            return Handled::Gone;
        };
        let granted = allows(trusted.permissions, Need::View);
        tracing::info!(
            "remote: \u{201c}{}\u{201d} ({}) attach {} {}",
            trusted.machine_name,
            peer.short(),
            session_id,
            if granted {
                "allowed"
            } else {
                "refused: needs view"
            }
        );
        if !granted {
            let message = "attach needs view permission; grant it to this Mac in the other \
                           Mac's Settings → Machines";
            return self
                .fail(attach_id, session_id, ErrorCode::PermissionDenied, message)
                .await;
        }

        // Attaching again restarts the stream with a fresh snapshot. The old
        // stream is replaced only once the new one can start, so a re-attach
        // that fails leaves a working mirror alone.
        self.live.retain(|_, task| !task.0.is_finished());
        let others = self.live.keys().filter(|id| **id != session_id).count();
        if others >= MAX_ATTACHED {
            let message = format!("at most {MAX_ATTACHED} sessions can be mirrored at once");
            return self
                .fail(attach_id, session_id, ErrorCode::CapacityExceeded, &message)
                .await;
        }
        let term = match self.source.find(&session_id).await {
            Ok(Some(term)) => term,
            Ok(None) => {
                let message = format!("no session with id {session_id} has a terminal to show");
                return self
                    .fail(attach_id, session_id, ErrorCode::SessionNotFound, &message)
                    .await;
            }
            Err(e) => {
                let message = e.to_string();
                return self
                    .fail(attach_id, session_id, ErrorCode::Internal, &message)
                    .await;
            }
        };

        let stream = Streamer {
            attach_id,
            conn: Arc::clone(&self.conn),
            registry: self.registry.clone(),
            peer,
            session_id: session_id.clone(),
            term,
        };
        let out = self.out.clone();
        let id = session_id.clone();
        let task = tokio::spawn(async move {
            if let Some((code, message)) = stream.run().await {
                let _ = out
                    .send(Frame::AttachFailed {
                        attach_id,
                        session_id: id,
                        code,
                        message,
                    })
                    .await;
            }
        });
        // Dropping the replaced entry aborts the old stream.
        self.live.insert(session_id, AbortOnDrop(task));
        Handled::Continue
    }

    async fn fail(
        &self,
        attach_id: u64,
        session_id: String,
        code: ErrorCode,
        message: &str,
    ) -> Handled {
        let _ = self
            .out
            .send(Frame::AttachFailed {
                attach_id,
                session_id,
                code,
                message: message.to_string(),
            })
            .await;
        Handled::Continue
    }
}

/// Why a screen stream stopped, when the client should be told.
type Stopped = Option<(ErrorCode, String)>;

fn gone() -> Stopped {
    Some((ErrorCode::SessionNotFound, "the session has ended".into()))
}

/// One host-opened screen stream.
struct Streamer {
    attach_id: u64,
    conn: Arc<dyn PeerConnection>,
    registry: PeerRegistry,
    peer: PeerId,
    session_id: String,
    term: Arc<dyn MirrorTerm>,
}

impl Streamer {
    fn viewable(&self) -> bool {
        self.registry
            .permissions(self.peer)
            .is_some_and(|p| allows(p, Need::View))
    }

    async fn capture(&self) -> Option<crate::terminal::mirror::Capture> {
        // The `Term` lock can be held by the PTY reader mid-chunk; wait for it
        // off the runtime's worker threads.
        let term = Arc::clone(&self.term);
        tokio::task::spawn_blocking(move || term.capture())
            .await
            .ok()
            .flatten()
    }

    async fn run(self) -> Stopped {
        let PeerStream { mut send, recv } = match self.conn.open_stream().await {
            Ok(stream) => stream,
            Err(e) => return Some((ErrorCode::Internal, e.to_string())),
        };

        // Requests from the client arrive on the same stream. Reading them in
        // a task of their own keeps the (not cancel-safe) frame read out of
        // the select below.
        // `hung_up` resolves when the reader stops, i.e. the client closed
        // the stream, which must stop a write that is waiting on that client.
        let (requests_tx, mut requests) = mpsc::channel::<Frame>(4);
        let (reader_alive, mut hung_up) = oneshot::channel::<()>();
        let _reader = AbortOnDrop(tokio::spawn(async move {
            let _alive = reader_alive;
            let mut recv = recv;
            while let Ok(frame) = read_frame(&mut recv).await {
                if requests_tx.send(frame).await.is_err() {
                    break;
                }
            }
        }));

        let open = Frame::ScreenOpen {
            attach_id: self.attach_id,
            session_id: self.session_id.clone(),
        };
        if write_frame(&mut send, &open).await.is_err() {
            return None;
        }

        // The interval below is the rate cap, so the producer's own is off.
        let mut producer = DiffProducer::new(Duration::ZERO);
        if let Some(limit) = self.term.history_limit() {
            producer = producer.with_history_limit(limit);
        }
        let Some(cap) = self.capture().await else {
            return gone();
        };
        let snapshot = producer.snapshot(cap, std::time::Instant::now());
        if let Err(stop) = send_frame(&mut send, snapshot).await {
            return stop;
        }

        // Requests share the frame-rate cap, so a peer cannot pull captures
        // faster than the screen would stream them.
        let mut next_request = Instant::now();
        let mut tick = interval(MIN_FRAME_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let frames: Vec<ScreenFrame> = tokio::select! {
                _ = self.conn.closed() => return None,
                request = requests.recv() => match request {
                    // The client closed the stream: that is a detach.
                    None => return None,
                    Some(request) if !self.viewable() => {
                        tracing::debug!("remote: {} {request:?} refused: view withdrawn", self.peer.short());
                        return revoked();
                    }
                    Some(Frame::ScrollbackRequest { start, count }) => {
                        sleep_until(next_request).await;
                        next_request = Instant::now() + MIN_FRAME_INTERVAL;
                        let (start, count) = (start as usize, count.min(MAX_PAGE_ROWS) as usize);
                        let term = Arc::clone(&self.term);
                        let captured = tokio::task::spawn_blocking(move || {
                            term.capture_with_history(start, count)
                        })
                        .await
                        .ok()
                        .flatten();
                        let Some((cap, page)) = captured else { return gone() };
                        producer.scrollback(cap, page, start, std::time::Instant::now())
                    }
                    Some(Frame::Resync) => {
                        sleep_until(next_request).await;
                        next_request = Instant::now() + MIN_FRAME_INTERVAL;
                        let Some(cap) = self.capture().await else { return gone() };
                        vec![producer.snapshot(cap, std::time::Instant::now())]
                    }
                    Some(other) => {
                        tracing::debug!(
                            "remote: {} sent an unexpected frame on a screen stream: {other:?}",
                            self.peer.short()
                        );
                        return Some((ErrorCode::BadRequest, "unexpected frame".into()));
                    }
                },
                _ = tick.tick() => {
                    if !self.viewable() {
                        return revoked();
                    }
                    let Some(cap) = self.capture().await else { return gone() };
                    match producer.poll(std::time::Instant::now(), || cap) {
                        Poll::Frame(frame) => vec![frame],
                        Poll::Unchanged | Poll::Wait(_) => continue,
                    }
                }
            };
            for frame in frames {
                // A slow link blocks here, and no frame is produced meanwhile:
                // that is what merges a backlog into the next diff.
                tokio::select! {
                    sent = send_frame(&mut send, frame) => {
                        if let Err(stop) = sent {
                            return stop;
                        }
                    }
                    _ = self.conn.closed() => return None,
                    // Closed by the client while we wait on it.
                    _ = &mut hung_up => return None,
                }
            }
        }
    }
}

/// Write one screen frame. The error is how the stream should stop: a frame
/// too large for the link is reported to the client, a broken link is not.
async fn send_frame<W: AsyncWrite + Unpin + ?Sized>(
    w: &mut W,
    frame: ScreenFrame,
) -> std::result::Result<(), Stopped> {
    let body = match wire::encode(&Frame::Screen { frame }) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("remote: screen frame would not encode: {e}");
            let message = format!("this screen is too large to send to the other Mac ({e})");
            return Err(Some((ErrorCode::Internal, message)));
        }
    };
    wire::write_body(w, &body).await.map_err(|e| {
        tracing::debug!("remote: screen stream ended: {e}");
        None
    })
}

fn revoked() -> Stopped {
    Some((
        ErrorCode::PermissionDenied,
        "view permission was withdrawn on the other Mac".into(),
    ))
}

// ── client ───────────────────────────────────────────────────────────────
//
// No caller in the app yet: the client UI that shows a mirrored session is
// its own ticket. The MemNet tests drive it end to end.

#[allow(dead_code)]
/// Pending attaches, by `attach_id`.
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<PeerStream>>>>>;

/// The client side of screen mirroring on one connection: sends `attach` on
/// the control stream and picks up the stream the host opens in reply.
///
/// The control stream belongs to whoever opened it, so this takes its writer
/// per call, and the control stream's reader hands `attach_failed` frames to
/// [`ScreenClient::on_control_frame`].
#[allow(dead_code)]
pub struct ScreenClient {
    waiting: Waiting,
    next_id: AtomicU64,
    _accept: AbortOnDrop,
}

#[allow(dead_code)]
impl ScreenClient {
    /// Start accepting the screen streams the host opens on `conn`.
    pub fn new(conn: Arc<dyn PeerConnection>) -> Self {
        let waiting: Waiting = Arc::default();
        let routes = Arc::clone(&waiting);
        let accept = tokio::spawn(async move {
            while let Ok(mut stream) = conn.accept_stream().await {
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let first = timeout(ATTACH_TIMEOUT, read_frame(&mut stream.recv)).await;
                    let Ok(Ok(Frame::ScreenOpen { attach_id, .. })) = first else {
                        return; // not a screen stream
                    };
                    // No waiter: an attach given up on. Dropping the stream
                    // closes it, which the host takes as a detach.
                    let waiter = routes
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&attach_id);
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(Ok(stream));
                    }
                });
            }
        });
        Self {
            waiting,
            next_id: AtomicU64::new(1),
            _accept: AbortOnDrop(accept),
        }
    }

    /// Mirror `session_id`. Writes `attach` to `control` and waits for the
    /// host's screen stream; the returned [`Attachment`] keeps its
    /// [`MirroredGrid`] up to date until it is detached or dropped.
    pub async fn attach<W: AsyncWrite + Unpin + ?Sized>(
        &self,
        control: &mut W,
        session_id: &str,
    ) -> Result<Attachment> {
        let attach_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(attach_id, tx);
        let attach = Frame::Attach {
            attach_id,
            session_id: session_id.to_string(),
        };
        let sent = write_frame(control, &attach).await;
        let answer = match sent {
            Ok(()) => timeout(ATTACH_TIMEOUT, rx).await,
            Err(e) => Ok(Ok(Err(e))),
        };
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&attach_id);
        match answer {
            Ok(Ok(Ok(stream))) => Ok(Attachment::start(session_id, stream)),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(AlleleError::Remote("the attach was abandoned".into())),
            Err(_) => Err(AlleleError::Remote(
                "the other Mac did not open the screen in time".into(),
            )),
        }
    }

    /// Offer a frame read from the control stream. Returns true if it was an
    /// `attach_failed`, which this consumes: the attach it names fails with
    /// its message. One for an attach no longer pending (a stream that has
    /// since ended) is dropped; that stream's own end is what reports.
    pub fn on_control_frame(&self, frame: &Frame) -> bool {
        let Frame::AttachFailed {
            attach_id, message, ..
        } = frame
        else {
            return false;
        };
        let waiter = self
            .waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(attach_id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(Err(AlleleError::Remote(message.clone())));
        }
        true
    }
}

/// A mirrored session. The grid is ready to hand to the grid element
/// (`Arc<FairMutex<MirroredGrid>>` is a `GridSource`).
#[allow(dead_code)]
pub struct Attachment {
    session_id: String,
    grid: Arc<FairMutex<MirroredGrid>>,
    changed: watch::Receiver<u64>,
    nudge: Arc<Notify>,
    _task: AbortOnDrop,
}

#[allow(dead_code)]
impl Attachment {
    fn start(session_id: &str, stream: PeerStream) -> Self {
        let grid = Arc::new(FairMutex::new(MirroredGrid::new()));
        let (changed_tx, changed) = watch::channel(0u64);
        let nudge = Arc::new(Notify::new());
        let task = tokio::spawn(mirror(
            stream,
            Arc::clone(&grid),
            changed_tx,
            Arc::clone(&nudge),
        ));
        Self {
            session_id: session_id.to_string(),
            grid,
            changed,
            nudge,
            _task: AbortOnDrop(task),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The mirrored screen.
    pub fn grid(&self) -> Arc<FairMutex<MirroredGrid>> {
        Arc::clone(&self.grid)
    }

    /// Bumped after every applied frame; closed when the stream ends.
    pub fn changed(&self) -> watch::Receiver<u64> {
        self.changed.clone()
    }

    /// Scroll the client's view; scrollback the host has not sent yet is
    /// requested in pages.
    pub fn scroll_display(&self, delta: i32) {
        self.grid.lock().scroll_display(delta);
        self.nudge.notify_one();
    }

    /// Stop mirroring: tell the host, then close the stream.
    pub async fn detach<W: AsyncWrite + Unpin + ?Sized>(self, control: &mut W) -> Result<()> {
        let detach = Frame::Detach {
            session_id: self.session_id.clone(),
        };
        write_frame(control, &detach).await
    }
}

/// Apply the host's frames to `grid`, and ask for scrollback as the view needs
/// it. Ends when the stream does.
#[allow(dead_code)]
async fn mirror(
    stream: PeerStream,
    grid: Arc<FairMutex<MirroredGrid>>,
    changed: watch::Sender<u64>,
    nudge: Arc<Notify>,
) {
    let PeerStream { mut send, recv } = stream;
    let (frames_tx, mut frames) = mpsc::channel::<Frame>(4);
    let _reader = AbortOnDrop(tokio::spawn(async move {
        let mut recv = recv;
        while let Ok(frame) = read_frame_within(&mut recv, MAX_PEER_FRAME).await {
            if frames_tx.send(frame).await.is_err() {
                break;
            }
        }
    }));

    // The page asked for and not yet received, so a request is sent once.
    let mut pending_page: Option<usize> = None;
    let mut applied = 0u64;
    loop {
        let reply = tokio::select! {
            frame = frames.recv() => {
                let Some(Frame::Screen { frame }) = frame else {
                    break; // the stream ended, or the host broke the protocol
                };
                if matches!(frame, ScreenFrame::Scrollback(_)) {
                    pending_page = None;
                }
                let result = grid.lock().apply(&frame);
                match result {
                    Ok(()) => {
                        applied += 1;
                        let _ = changed.send(applied);
                        None
                    }
                    Err(e) => {
                        tracing::debug!("remote: mirror out of step ({e}); asking for a snapshot");
                        pending_page = None;
                        Some(Frame::Resync)
                    }
                }
            }
            _ = nudge.notified() => None,
        };
        let page = grid.lock().wanted_page(MAX_PAGE_ROWS as usize);
        let request = reply.or_else(|| match page {
            Some((start, count)) if pending_page != Some(start) => {
                pending_page = Some(start);
                Some(Frame::ScrollbackRequest {
                    start: start as u32,
                    count: count.min(MAX_PAGE_ROWS as usize) as u32,
                })
            }
            _ => None,
        });
        if let Some(request) = request {
            if write_frame(&mut send, &request).await.is_err() {
                break;
            }
        }
    }
    let _ = send.shutdown().await;
}
