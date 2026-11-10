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
//!
//!   screen: input {text, paste?}        ───►     control? (read now) → PTY
//!                                       ◄───       input_refused {…} if not
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
//!
//! **Remote input** (DEV-1166) rides the same stream, client to host. It is a
//! human typing on the other Mac, so it is not gated like `away::decide`;
//! but `control` is read from the registry for every `input` frame, and a
//! frame without it is dropped and answered `input_refused`. Input is handled
//! by the stream's reader, not the frame loop, so a keystroke never waits
//! behind screen frames queued for a slow link. While a stream is open the
//! host lists its machine in [`Viewers`] for the tab chip.

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
use super::viewers::Viewers;
use super::wire::{self, read_frame, read_frame_within, write_frame, Frame, MAX_PEER_FRAME};
use crate::dispatch::protocol::ErrorCode;
use crate::dispatch::server::{ScreenLookup, ScreenResize};
use crate::errors::{AlleleError, Result};
use crate::terminal::keymap::paste_bytes;
use crate::terminal::mirror::{
    DiffProducer, MirrorTerm, MirroredGrid, Poll, ScreenFrame, MIN_FRAME_INTERVAL,
};

/// Sessions one control stream may mirror at once.
pub const MAX_ATTACHED: usize = 8;

/// Most scrollback rows in one page. A dense row (a style change on every
/// cell) encodes to a few KiB; 100 of them stay well inside `MAX_PEER_FRAME`.
pub const MAX_PAGE_ROWS: u32 = 100;

/// How long a client waits for the host to open the screen stream.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the host waits for the foreground thread to find a session.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Most text in one `input` frame. A keystroke is a few bytes; this bounds a
/// paste. JSON escapes a control byte to six, so even a worst-case frame
/// stays under `MAX_PEER_FRAME`. The client refuses a larger paste before
/// sending it.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// How fast the host writes remote input to a PTY, at most. Far above any
/// typist, and it keeps a peer from queueing unbounded input behind a
/// program that has stopped reading.
const INPUT_BYTES_PER_SEC: u64 = 1024 * 1024;

/// The least time between two input writes, however small.
const MIN_INPUT_INTERVAL: Duration = Duration::from_millis(1);

/// Input frames the client queues before `send_input` reports the link as
/// not keeping up.
const INPUT_QUEUE: usize = 64;

/// Input frames the host reads ahead of the PTY writes. Once full, the
/// reader stops and the link pushes back on the peer.
const INPUT_BACKLOG: usize = 16;

/// After remote input is written, the screen is polled this often instead of
/// every `MIN_FRAME_INTERVAL`, so the echo goes out as soon as the program
/// draws it rather than at the next 33 ms tick (DEV-1242).
const ECHO_POLL: Duration = Duration::from_millis(4);

/// How long after input the fast poll waits for the echo. A program slower
/// than this to redraw is shown by the regular tick, as before.
const ECHO_WINDOW: Duration = Duration::from_millis(100);

/// What the client shows when the host drops its input for want of `control`.
const NEEDS_CONTROL: &str = "typing needs control permission; grant it to this Mac in the \
                             other Mac's Settings → Machines";

// ── host: finding a session's terminal ───────────────────────────────────

/// Finds a session's terminal for a screen stream. Production asks the
/// foreground thread; tests serve terminals from a map.
#[async_trait]
pub trait ScreenSource: Send + Sync {
    /// `Ok(None)` when there is no such session or it has no terminal (still
    /// starting, suspended); `Err` when the host could not tell, which is
    /// worth retrying.
    async fn find(&self, session_id: &str) -> Result<Option<Arc<dyn MirrorTerm>>>;

    /// Resize a session's terminal to a paired machine's window (DEV-1167).
    /// `Ok(false)` when there is no such terminal. The host's next local
    /// interaction restores its own size; that is the host UI's business.
    async fn resize(&self, _session_id: &str, _columns: u16, _lines: u16) -> Result<bool> {
        Ok(false)
    }
}

/// The production [`ScreenSource`]: the same foreground drain `control.sock`
/// uses, which looks the session up in `AppState`.
pub struct ScreenQueue {
    tx: std_mpsc::Sender<ScreenLookup>,
    resizes: Option<std_mpsc::Sender<ScreenResize>>,
}

impl ScreenQueue {
    pub fn new(tx: std_mpsc::Sender<ScreenLookup>) -> Self {
        Self { tx, resizes: None }
    }

    /// Also serve "Fit to this window" (DEV-1167) through the same drain.
    pub fn with_resizes(mut self, resizes: std_mpsc::Sender<ScreenResize>) -> Self {
        self.resizes = Some(resizes);
        self
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

    async fn resize(&self, session_id: &str, columns: u16, lines: u16) -> Result<bool> {
        let Some(tx) = self.resizes.clone() else {
            return Ok(false);
        };
        let session_id = session_id.to_string();
        let busy = || AlleleError::Remote("the other Mac is busy; try again".into());
        tokio::task::spawn_blocking(move || {
            let (reply, answer) = std_mpsc::channel();
            tx.send(ScreenResize {
                session_id,
                columns,
                lines,
                reply,
            })
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
pub(super) struct AbortOnDrop(pub(super) JoinHandle<()>);

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
    viewers: Viewers,
    /// The control stream's outbox, for `attach_failed`.
    out: mpsc::Sender<Frame>,
    live: HashMap<String, AbortOnDrop>,
}

impl Attachments {
    pub(super) fn new(
        conn: Arc<dyn PeerConnection>,
        registry: PeerRegistry,
        source: Arc<dyn ScreenSource>,
        viewers: Viewers,
        out: mpsc::Sender<Frame>,
    ) -> Self {
        Self {
            conn,
            registry,
            source,
            viewers,
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
            Frame::Resize {
                session_id,
                columns,
                lines,
            } => self.resize(session_id, columns, lines),
            _ => {}
        }
        Handled::Continue
    }

    /// "Fit to this window" (DEV-1167): needs `control`, read now, and the
    /// session must be attached on this stream, so a peer can only resize
    /// what it is looking at. Run off the read loop; there is no reply.
    fn resize(&mut self, session_id: String, columns: u16, lines: u16) {
        let peer = self.conn.remote_id();
        let permitted = self
            .registry
            .permissions(peer)
            .is_some_and(|p| allows(p, Need::Control));
        let attached = self
            .live
            .get(&session_id)
            .is_some_and(|task| !task.0.is_finished());
        tracing::info!(
            "remote: {} resize {} to {}x{} {}",
            peer.short(),
            session_id,
            columns,
            lines,
            match (permitted, attached) {
                (true, true) => "allowed",
                (false, _) => "refused: needs control",
                (true, false) => "refused: not attached",
            }
        );
        if !(permitted && attached) {
            return;
        }
        let source = Arc::clone(&self.source);
        tokio::spawn(async move {
            if let Err(e) = source.resize(&session_id, columns, lines).await {
                tracing::debug!("remote: resize of {session_id} failed: {e}");
            }
        });
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
            machine_name: trusted.machine_name.clone(),
            viewers: self.viewers.clone(),
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
    /// The peer's name as paired, for the host's tab chip.
    machine_name: String,
    viewers: Viewers,
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

    /// Capture the screen and diff it against what the client has: the
    /// frame to send, if anything changed. Both the tick and the echo
    /// one-shot come through here, so neither sends a frame to a peer whose
    /// `view` has been withdrawn.
    async fn next_frame(
        &self,
        producer: &mut DiffProducer,
    ) -> std::result::Result<Option<ScreenFrame>, Stopped> {
        if !self.viewable() {
            return Err(revoked());
        }
        let Some(cap) = self.capture().await else {
            return Err(gone());
        };
        Ok(match producer.poll(std::time::Instant::now(), || cap) {
            Poll::Frame(frame) => Some(frame),
            Poll::Unchanged | Poll::Wait(_) => None,
        })
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
        //
        // `input` frames are handled by the reader itself (see [`Input`]), so
        // typing is never queued behind a scrollback page or a slow write.
        // Its refusals come back through `notices` for this loop to write.
        //
        // Listed on the host's tab before any input can be read, and until
        // this stream ends, however it ends.
        let _viewing = self
            .viewers
            .register(&self.session_id, self.peer, &self.machine_name);
        let (requests_tx, mut requests) = mpsc::channel::<Frame>(4);
        let (inputs_tx, mut inputs) = mpsc::channel::<(String, bool)>(INPUT_BACKLOG);
        let (notices_tx, mut notices) = mpsc::channel::<Frame>(1);
        let (reader_alive, mut hung_up) = oneshot::channel::<()>();
        let mut input = Input {
            registry: self.registry.clone(),
            peer: self.peer,
            term: Arc::clone(&self.term),
            notices: notices_tx,
            next: Instant::now(),
        };
        // Input on a task of its own: pacing and a paste's `Term` lock wait
        // hold up neither screen requests nor screen frames.
        // `typed` tells the frame loop that input reached the PTY, so it can
        // send the echo early. Refused input wrote nothing and has no echo.
        let typed = Arc::new(Notify::new());
        let typed_tx = Arc::clone(&typed);
        let _input = AbortOnDrop(tokio::spawn(async move {
            while let Some((text, paste)) = inputs.recv().await {
                if input.deliver(text, paste).await {
                    typed_tx.notify_one();
                }
            }
        }));
        let _reader = AbortOnDrop(tokio::spawn(async move {
            let _alive = reader_alive;
            let mut recv = recv;
            // An admitted peer's limit: a paste is up to MAX_INPUT_BYTES.
            while let Ok(frame) = read_frame_within(&mut recv, MAX_PEER_FRAME).await {
                let sent = match frame {
                    Frame::Input { text, paste } => inputs_tx.send((text, paste)).await.is_ok(),
                    other => requests_tx.send(other).await.is_ok(),
                };
                if !sent {
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
        // The echo one-shot (DEV-1242). Input arms it; the first frame after
        // that is sent at once instead of on the tick, and disarms it, so
        // input adds at most one frame to the 30/s cap.
        //
        // The shot is spent on whatever changed first. When something
        // unrelated (a spinner, a clock) draws between the input and its
        // echo, the echo waits for the tick, as it did before DEV-1242:
        // never later. Keeping the shot until the "real" echo would either
        // send more than one extra frame per input, or need to tell an echo
        // from a redraw, which a full-screen program like Claude Code (it
        // repaints its whole input area for either) doesn't let us do.
        let mut echo_until = Instant::now();
        let mut echo_poll = interval(ECHO_POLL);
        echo_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let frames: Vec<ScreenFrame> = tokio::select! {
                _ = self.conn.closed() => return None,
                Some(notice) = notices.recv() => {
                    tokio::select! {
                        sent = write_frame(&mut send, &notice) => {
                            if sent.is_err() {
                                return None;
                            }
                        }
                        _ = self.conn.closed() => return None,
                        _ = &mut hung_up => return None,
                    }
                    continue;
                }
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
                _ = typed.notified() => {
                    echo_until = Instant::now() + ECHO_WINDOW;
                    continue;
                }
                _ = echo_poll.tick(), if Instant::now() < echo_until => {
                    match self.next_frame(&mut producer).await {
                        Ok(Some(frame)) => {
                            echo_until = Instant::now();
                            vec![frame]
                        }
                        Ok(None) => continue,
                        Err(stop) => return stop,
                    }
                }
                _ = tick.tick() => match self.next_frame(&mut producer).await {
                    Ok(Some(frame)) => vec![frame],
                    Ok(None) => continue,
                    Err(stop) => return stop,
                },
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

/// The host side of remote input on one screen stream (DEV-1166).
struct Input {
    registry: PeerRegistry,
    peer: PeerId,
    term: Arc<dyn MirrorTerm>,
    /// To the stream's frame loop, which writes `input_refused`. Capacity
    /// one: while a refusal is waiting, later ones are dropped, so a peer
    /// typing into a read-only mirror cannot make the host queue replies.
    notices: mpsc::Sender<Frame>,
    /// When the next write may happen (see [`INPUT_BYTES_PER_SEC`]).
    next: Instant,
}

impl Input {
    /// Read live, from the registry, for every frame: `control` can be
    /// granted or withdrawn while the mirror stays open.
    fn controllable(&self) -> bool {
        self.registry
            .permissions(self.peer)
            .is_some_and(|p| allows(p, Need::Control))
    }

    /// Write one `input` frame to the PTY, or drop it and say why. Whether
    /// it was written.
    async fn deliver(&mut self, text: String, paste: bool) -> bool {
        // Every frame is paced, refused or not, by the bytes the peer sent;
        // and before the check, so the permission read is the last thing
        // before the write.
        sleep_until(self.next).await;
        let pace = Duration::from_micros(text.len() as u64 * 1_000_000 / INPUT_BYTES_PER_SEC);
        self.next = Instant::now() + pace.max(MIN_INPUT_INTERVAL);
        if text.len() > MAX_INPUT_BYTES {
            let message = format!("input over {MAX_INPUT_BYTES} bytes was not sent");
            self.refuse(ErrorCode::BadRequest, message);
            return false;
        }
        if !self.controllable() {
            tracing::debug!("remote: {} input dropped: needs control", self.peer.short());
            self.refuse(ErrorCode::PermissionDenied, NEEDS_CONTROL.into());
            return false;
        }
        let term = Arc::clone(&self.term);
        let written = if paste {
            // Reading the paste mode takes the `Term` lock, which the PTY
            // reader can hold mid-chunk: wait for it off the runtime's
            // worker threads. A keystroke needs no lock.
            tokio::task::spawn_blocking(move || {
                term.bracketed_paste()
                    .is_some_and(|bracketed| term.write_input(&paste_bytes(&text, bracketed)))
            })
            .await
            .unwrap_or(false)
        } else {
            term.write_input(text.as_bytes())
        };
        if !written {
            let message = "the session no longer takes input".to_string();
            self.refuse(ErrorCode::SessionNotFound, message);
        }
        written
    }

    fn refuse(&self, code: ErrorCode, message: String) {
        let _ = self.notices.try_send(Frame::InputRefused { code, message });
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

/// Pending attaches, by `attach_id`.
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<PeerStream>>>>>;

/// The client side of screen mirroring on one connection: sends `attach` on
/// the control stream and picks up the stream the host opens in reply.
///
/// The control stream belongs to whoever opened it, so this takes its writer
/// per call, and the control stream's reader hands `attach_failed` frames to
/// [`ScreenClient::on_control_frame`].
pub struct ScreenClient {
    waiting: Waiting,
    next_id: AtomicU64,
    _accept: AbortOnDrop,
}

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
    #[cfg(test)] // the app attaches through `request` (the follower's outbox)
    pub async fn attach<W: AsyncWrite + Unpin + ?Sized>(
        &self,
        control: &mut W,
        session_id: &str,
    ) -> Result<Attachment> {
        let (attach, pending) = self.request(session_id);
        write_frame(control, &attach).await?;
        pending.wait().await
    }

    /// Start an attach without writing it: returns the `attach` frame for the
    /// caller to send on the control stream, and the wait for its answer.
    ///
    /// For a control stream whose writer is shared (the follower's outbox), so
    /// a slow host never holds the writer while it decides.
    pub fn request(&self, session_id: &str) -> (Frame, PendingAttach) {
        let attach_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(attach_id, tx);
        let frame = Frame::Attach {
            attach_id,
            session_id: session_id.to_string(),
        };
        let pending = PendingAttach {
            attach_id,
            session_id: session_id.to_string(),
            rx: Some(rx),
            waiting: Arc::clone(&self.waiting),
        };
        (frame, pending)
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

/// An attach sent and not yet answered. Dropping it gives the attach up; a
/// stream the host opens for it afterwards is closed, which the host takes as
/// a detach.
pub struct PendingAttach {
    attach_id: u64,
    session_id: String,
    rx: Option<oneshot::Receiver<Result<PeerStream>>>,
    waiting: Waiting,
}

impl PendingAttach {
    /// Wait up to [`ATTACH_TIMEOUT`] for the host's screen stream.
    pub async fn wait(mut self) -> Result<Attachment> {
        let Some(rx) = self.rx.take() else {
            return Err(AlleleError::Remote("the attach was abandoned".into()));
        };
        match timeout(ATTACH_TIMEOUT, rx).await {
            Ok(Ok(Ok(stream))) => Ok(Attachment::start(&self.session_id, stream)),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(AlleleError::Remote("the attach was abandoned".into())),
            Err(_) => Err(AlleleError::Remote(
                "the other Mac did not open the screen in time".into(),
            )),
        }
    }
}

impl Drop for PendingAttach {
    fn drop(&mut self) {
        self.waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.attach_id);
    }
}

/// A mirrored session. The grid is ready to hand to the grid element
/// (`Arc<FairMutex<MirroredGrid>>` is a `GridSource`).
///
/// With `control` it also types into the session: [`Attachment::send_input`]
/// for keystrokes, [`Attachment::send_paste`] for a paste. Both are
/// non-blocking, so a key handler on the UI thread can call them.
pub struct Attachment {
    #[allow(dead_code)] // read by `detach`
    session_id: String,
    grid: Arc<FairMutex<MirroredGrid>>,
    changed: watch::Receiver<u64>,
    nudge: Arc<Notify>,
    /// Input frames for the stream task to write, in order.
    input: mpsc::Sender<Frame>,
    /// The host's last `input_refused` message, not yet taken.
    refusal: Arc<Mutex<Option<String>>>,
    _task: AbortOnDrop,
}

impl Attachment {
    fn start(session_id: &str, stream: PeerStream) -> Self {
        let grid = Arc::new(FairMutex::new(MirroredGrid::new()));
        let (changed_tx, changed) = watch::channel(0u64);
        let nudge = Arc::new(Notify::new());
        let (input, input_rx) = mpsc::channel(INPUT_QUEUE);
        let refusal = Arc::new(Mutex::new(None));
        let task = tokio::spawn(mirror(
            stream,
            Mirror {
                grid: Arc::clone(&grid),
                changed: changed_tx,
                nudge: Arc::clone(&nudge),
                input: input_rx,
                refusal: Arc::clone(&refusal),
            },
        ));
        Self {
            session_id: session_id.to_string(),
            grid,
            changed,
            nudge,
            input,
            refusal,
            _task: AbortOnDrop(task),
        }
    }

    #[allow(dead_code)] // remote tabs key on their own `TabKey`
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

    /// Type into the host session. `bytes` is what the local keymap produced
    /// for the keystroke (`keymap::app_action`'s `SendBytes`, or
    /// `KeymapConfig::resolve`) — the bytes local typing would write. Ctrl-C
    /// is `[0x03]`.
    ///
    /// Queued, not sent, when this returns: the host checks `control` and,
    /// without it, answers with a refusal ([`Attachment::take_refusal`]).
    /// `Err` only when the input never left this machine: the mirror has
    /// ended, the link is not keeping up, the bytes are not UTF-8 (keymap
    /// output always is) or there are more than [`MAX_INPUT_BYTES`].
    pub fn send_input(&self, bytes: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| AlleleError::Remote("that key cannot be sent to the other Mac".into()))?;
        self.queue(text.to_string(), false)
    }

    /// Paste `text` into the host session as one frame. The host wraps it in
    /// bracketed-paste markers when its terminal asked for them, so a
    /// multi-line paste arrives as one paste, never as typed Enter keys.
    /// Same errors as [`Attachment::send_input`]; a paste over
    /// [`MAX_INPUT_BYTES`] is refused whole, never split.
    pub fn send_paste(&self, text: &str) -> Result<()> {
        self.queue(text.to_string(), true)
    }

    /// The host's last refusal of this attachment's input, once: a second
    /// call returns `None` until the host refuses again. [`Attachment::changed`]
    /// is bumped when one arrives, so a view can show "read only" when the
    /// user types into a mirror without `control`.
    pub fn take_refusal(&self) -> Option<String> {
        self.refusal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    fn queue(&self, text: String, paste: bool) -> Result<()> {
        if text.len() > MAX_INPUT_BYTES {
            return Err(AlleleError::Remote(format!(
                "that is too much to paste to the other Mac at once (over {} KiB)",
                MAX_INPUT_BYTES / 1024
            )));
        }
        self.input
            .try_send(Frame::Input { text, paste })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    AlleleError::Remote("the other Mac is not keeping up; try again".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    AlleleError::Remote("this session is no longer attached".into())
                }
            })
    }

    /// Stop mirroring: tell the host, then close the stream.
    #[allow(dead_code)] // a remote tab detaches by closing the stream (drop)
    pub async fn detach<W: AsyncWrite + Unpin + ?Sized>(self, control: &mut W) -> Result<()> {
        let detach = Frame::Detach {
            session_id: self.session_id.clone(),
        };
        write_frame(control, &detach).await
    }
}

/// What the client's stream task updates.
struct Mirror {
    grid: Arc<FairMutex<MirroredGrid>>,
    changed: watch::Sender<u64>,
    nudge: Arc<Notify>,
    input: mpsc::Receiver<Frame>,
    refusal: Arc<Mutex<Option<String>>>,
}

/// Apply the host's frames to the grid, ask for scrollback as the view needs
/// it, and send the user's input. Ends when the stream does.
async fn mirror(stream: PeerStream, m: Mirror) {
    let Mirror {
        grid,
        changed,
        nudge,
        mut input,
        refusal,
    } = m;
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
            frame = frames.recv() => match frame {
                Some(Frame::Screen { frame }) => {
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
                Some(Frame::InputRefused { message, .. }) => {
                    *refusal.lock().unwrap_or_else(|p| p.into_inner()) = Some(message);
                    applied += 1;
                    let _ = changed.send(applied);
                    None
                }
                // The stream ended, or the host broke the protocol.
                _ => break,
            },
            Some(typed) = input.recv() => {
                if write_frame(&mut send, &typed).await.is_err() {
                    break;
                }
                None
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
