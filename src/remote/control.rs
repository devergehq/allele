//! The control stream: a paired machine calling this one's dispatch API
//! (DEV-1161, proposal §3.3–3.4).
//!
//! ```text
//!   client                                    host
//!   ──────                                    ────
//!   open stream, hello {protocol, name}  ───► version ok? peer still trusted?
//!                                       ◄───  hello {protocol, name, permissions}
//!                                             | refused {code, message}, then
//!                                               wait for the client to hang up
//!   request {id, op}                     ───► permissions(peer), read now
//!                                             ├ not granted → permission_denied
//!                                             └ granted     → the control.sock
//!                                                             drain, unchanged
//!                                       ◄───  response {id, result}
//! ```
//!
//! **`control.sock` is never exposed.** A request is decoded here into the
//! same `dispatch::protocol::Request` the socket decodes, and handed to the
//! same foreground drain through [`RemoteDispatch`] — so the two servers share
//! one handler, one non-interactive contract (§7.9) and one admission cap, and
//! cannot drift apart on what an operation does.
//!
//! What may be called is decided per request by [`need`] against the peer's
//! grant as it stands *at that moment*: a host user revoking `control` stops
//! the next create on a connection that is already open.
//!
//! The status feed (DEV-1162) rides the same stream: after
//! `subscribe_status`, a push task queues a snapshot and then every change on
//! the same outbox the responses use, so the two interleave. It needs `view`,
//! read at subscribe time and again before every change it sends.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::peers::PeerPermissions;
use super::registry::PeerRegistry;
use super::screen::{Attachments, Handled, ScreenSource};
use super::transport::{PeerConnection, PeerId, PeerStream};
use super::wire::{self, write_frame, Frame, StreamRefusal, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::dispatch::protocol::{ErrorCode, Request, Response};
use crate::dispatch::server::{forward, ControlRequest};
use crate::status_feed::StatusFeed;

/// Requests one connection may have in flight. Past this the host stops
/// reading that stream until one finishes — backpressure on a peer that
/// floods, rather than a queue of creates on the foreground thread.
pub const MAX_IN_FLIGHT: usize = 8;

const NOT_PAIRED: &str = "this machine is not paired";

/// Responses and pushes waiting to be written on one stream.
const OUTBOX: usize = 2 * MAX_IN_FLIGHT;

// ── permissions ───────────────────────────────────────────────────────────

/// Which grant an operation needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// Read-only: status and screen.
    View,
    /// Changes something on this machine.
    Control,
}

/// The grant `op` needs. Exhaustive on purpose: a new dispatch op does not
/// compile until someone decides what it needs remotely.
pub fn need(op: &Request) -> Need {
    match op {
        Request::ProjectsList | Request::SessionsList | Request::SessionsStatus { .. } => {
            Need::View
        }
        Request::SessionsCreate(_)
        | Request::SessionsInterrupt { .. }
        | Request::SessionsDiscard { .. } => Need::Control,
        // A paired machine has no session here to ask as; `localise` strips
        // the caller, so these are refused whatever the grant (DEV-1267).
        Request::DecisionsList { .. } => Need::View,
        Request::DecisionsPost(_) | Request::DecisionsWithdraw { .. } => Need::Control,
    }
}

pub fn allows(permissions: PeerPermissions, need: Need) -> bool {
    match need {
        Need::View => permissions.view,
        Need::Control => permissions.control,
    }
}

/// The op's wire name, for the log.
pub fn op_name(op: &Request) -> &'static str {
    match op {
        Request::ProjectsList => "projects_list",
        Request::SessionsList => "sessions_list",
        Request::SessionsStatus { .. } => "sessions_status",
        Request::SessionsCreate(_) => "sessions_create",
        Request::SessionsInterrupt { .. } => "sessions_interrupt",
        Request::SessionsDiscard { .. } => "sessions_discard",
        Request::DecisionsPost(_) => "decisions_post",
        Request::DecisionsList { .. } => "decisions_list",
        Request::DecisionsWithdraw { .. } => "decisions_withdraw",
    }
}

/// Strip what a remote caller cannot honestly claim. `caller_session_id` and
/// `caller_reply_to` name a session and a socket *on this machine*; a peer has
/// neither, so a value it sends is either wrong or borrowed from a local
/// session. Absent, the request is treated like any caller with no session —
/// which still counts against the cap and the depth limit.
fn localise(op: Request) -> Request {
    match op {
        Request::SessionsCreate(mut req) => {
            req.caller_session_id = None;
            req.caller_reply_to = None;
            Request::SessionsCreate(req)
        }
        Request::DecisionsPost(mut req) => {
            req.caller_session_id = None;
            Request::DecisionsPost(req)
        }
        Request::DecisionsList { .. } => Request::DecisionsList {
            caller_session_id: None,
        },
        Request::DecisionsWithdraw { id, .. } => Request::DecisionsWithdraw {
            id,
            caller_session_id: None,
        },
        other => other,
    }
}

// ── the seam to the dispatch handler ──────────────────────────────────────

/// Answers one dispatch request. Production sends it to the `control.sock`
/// foreground drain; tests answer from a script.
#[async_trait]
pub trait RemoteDispatch: Send + Sync {
    async fn call(&self, op: Request) -> Response;
}

/// The production [`RemoteDispatch`]: the same channel `control.sock` feeds.
pub struct ControlQueue {
    tx: mpsc::Sender<ControlRequest>,
}

impl ControlQueue {
    pub fn new(tx: mpsc::Sender<ControlRequest>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl RemoteDispatch for ControlQueue {
    /// `control.sock`'s own hand-off, so the two cannot differ on timeouts or
    /// errors. It blocks on a std channel, so it runs on the blocking pool —
    /// at most [`MAX_IN_FLIGHT`] threads per connection.
    async fn call(&self, op: Request) -> Response {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || forward(op, &tx))
            .await
            .unwrap_or_else(|_| internal("Allele did not answer"))
    }
}

/// A dispatcher for a service with no app behind it (fixtures).
#[cfg(test)]
pub struct Unavailable;

#[cfg(test)]
#[async_trait]
impl RemoteDispatch for Unavailable {
    async fn call(&self, _: Request) -> Response {
        internal("remote dispatch is not available in this Allele")
    }
}

fn internal(message: &str) -> Response {
    Response::Error {
        code: ErrorCode::Internal,
        message: message.to_string(),
    }
}

/// This machine's name as peers are told it. The setting is live; the
/// computer name is looked up once, off the runtime, when first needed.
pub struct HostName {
    setting: Mutex<Option<String>>,
    computer: tokio::sync::OnceCell<Option<String>>,
    lookup: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl HostName {
    pub fn new(
        setting: Option<String>,
        lookup: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            setting: Mutex::new(setting),
            computer: tokio::sync::OnceCell::new(),
            lookup: Box::new(lookup),
        })
    }

    pub fn set(&self, setting: Option<String>) {
        *self.setting.lock().unwrap_or_else(|p| p.into_inner()) = setting;
    }

    pub async fn get(self: &Arc<Self>) -> String {
        let this = Arc::clone(self);
        let computer = self
            .computer
            .get_or_init(|| async move {
                tokio::task::spawn_blocking(move || (this.lookup)())
                    .await
                    .ok()
                    .flatten()
            })
            .await
            .clone();
        let setting = self
            .setting
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        super::identity::resolve_machine_name(setting.as_deref(), computer)
    }
}

/// What a control stream needs from the host. Cheap to clone.
#[derive(Clone)]
pub struct ControlContext {
    pub dispatch: Arc<dyn RemoteDispatch>,
    pub host_name: Arc<HostName>,
    /// What `subscribe_status` streams (DEV-1162).
    pub status_feed: StatusFeed,
    /// Finds a session's terminal for `attach` (DEV-1165).
    pub screens: Arc<dyn ScreenSource>,
    /// Who is attached to which session, for the host's tab chip (DEV-1166).
    pub viewers: super::viewers::Viewers,
}

impl ControlContext {
    /// No app behind it: every request is answered `internal`.
    #[cfg(test)]
    pub fn unavailable() -> Self {
        Self {
            dispatch: Arc::new(Unavailable),
            host_name: HostName::new(None, || None),
            status_feed: StatusFeed::new(),
            screens: Arc::new(super::screen::NoScreens),
            viewers: Default::default(),
        }
    }
}

// ── the stream ────────────────────────────────────────────────────────────

/// How a control stream ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The stream is over; the connection carries on.
    Stream,
    /// The whole connection should close, for this reason — after the peer
    /// has had a chance to read the refusal (see `listener`).
    Connection(String),
}

/// Serve one control stream whose client hello (`protocol`, `machine_name`)
/// has been read. `busy` is set while this connection has a control stream.
pub async fn serve(
    conn: &Arc<dyn PeerConnection>,
    mut stream: PeerStream,
    protocol: u32,
    registry: &PeerRegistry,
    ctx: &ControlContext,
    busy: &AtomicBool,
) -> Ended {
    let peer = conn.remote_id();
    if busy.swap(true, Ordering::SeqCst) {
        let refused = Frame::Refused {
            code: StreamRefusal::AlreadyOpen,
            message: "this connection already has a control stream".into(),
        };
        let _ = write_frame(&mut stream.send, &refused).await;
        let _ = stream.send.shutdown().await;
        return Ended::Stream;
    }
    let ended = if protocol == PROTOCOL_VERSION {
        run(conn, stream, registry, ctx).await
    } else {
        refuse_protocol(peer, stream, protocol).await
    };
    busy.store(false, Ordering::SeqCst);
    ended
}

/// The client speaks another protocol: say so, readably, and end the
/// connection — nothing else on it can be understood either.
async fn refuse_protocol(peer: PeerId, mut stream: PeerStream, protocol: u32) -> Ended {
    {
        let message = format!(
            "The other Mac speaks remote protocol {PROTOCOL_VERSION} and this one speaks \
             {protocol}. Update Allele on both Macs to the same version."
        );
        tracing::info!("remote: {} sent protocol {protocol}; refused", peer.short());
        let refused = Frame::Refused {
            code: StreamRefusal::UnsupportedProtocol,
            message: message.clone(),
        };
        let _ = write_frame(&mut stream.send, &refused).await;
        Ended::Connection(message)
    }
}

async fn run(
    conn: &Arc<dyn PeerConnection>,
    stream: PeerStream,
    registry: &PeerRegistry,
    ctx: &ControlContext,
) -> Ended {
    let peer = conn.remote_id();
    let PeerStream { mut send, mut recv } = stream;
    // Read live: a peer removed since the gate admitted it gets nothing.
    let Some(permissions) = registry.permissions(peer) else {
        let refused = Frame::Refused {
            code: StreamRefusal::NotPaired,
            message: NOT_PAIRED.into(),
        };
        let _ = write_frame(&mut send, &refused).await;
        return Ended::Connection(NOT_PAIRED.into());
    };
    let welcome = Frame::Hello {
        protocol: PROTOCOL_VERSION,
        machine_name: ctx.host_name.get().await,
        permissions: Some(permissions),
    };
    if write_frame(&mut send, &welcome).await.is_err() {
        return Ended::Stream;
    }

    let (out, mut outbox) = tokio::sync::mpsc::channel::<Frame>(OUTBOX);
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbox.recv().await {
            let Some(body) = fit(&frame) else { continue };
            if wire::write_body(&mut send, &body).await.is_err() {
                return;
            }
        }
        let _ = send.shutdown().await;
    });

    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let mut status_push: Option<JoinHandle<()>> = None;
    // The screen streams this control stream started; dropped with it.
    let mut attachments = Attachments::new(
        Arc::clone(conn),
        registry.clone(),
        Arc::clone(&ctx.screens),
        ctx.viewers.clone(),
        out.clone(),
    );
    let ended = loop {
        let body = tokio::select! {
            body = wire::read_body_within(&mut recv, MAX_PEER_FRAME) => body,
            _ = conn.closed() => break Ended::Stream,
        };
        let Ok(body) = body else {
            break Ended::Stream; // the client finished, or the stream broke
        };
        let (id, op) = match wire::decode(&body) {
            Ok(Frame::Request { id, op }) => (id, op),
            Ok(Frame::SubscribeStatus) => {
                match subscribe_status(registry, peer, ctx, &out, &mut status_push).await {
                    Subscribed::Started | Subscribed::Refused => continue,
                    Subscribed::Gone => break Ended::Connection(NOT_PAIRED.into()),
                    Subscribed::Closed => break Ended::Stream,
                }
            }
            // Inline: the lookup behind an attach is bounded, and requests
            // already admitted keep running meanwhile.
            Ok(frame @ (Frame::Attach { .. } | Frame::Detach { .. } | Frame::Resize { .. })) => {
                if attachments.handle(frame).await == Handled::Gone {
                    break Ended::Connection(NOT_PAIRED.into());
                }
                continue;
            }
            Ok(other) => {
                tracing::debug!(
                    "remote: {} sent an unexpected frame on its control stream: {other:?}",
                    peer.short()
                );
                break Ended::Stream;
            }
            Err(e) => match wire::request_id(&body) {
                // A request this host cannot read — an op from a newer
                // Allele, say. Answer it rather than leave the caller waiting.
                Some(id) => {
                    tracing::info!("remote: {} sent an unreadable request: {e}", peer.short());
                    let bad = Response::Error {
                        code: ErrorCode::BadRequest,
                        message: e.to_string(),
                    };
                    if out.send(Frame::Response { id, result: bad }).await.is_err() {
                        break Ended::Stream;
                    }
                    continue;
                }
                None => break Ended::Stream,
            },
        };

        match admit(registry, peer, &op) {
            Admission::Gone => break Ended::Connection(NOT_PAIRED.into()),
            Admission::Refused(result) => {
                if out.send(Frame::Response { id, result }).await.is_err() {
                    break Ended::Stream;
                }
            }
            Admission::Allowed => {
                let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
                    break Ended::Stream;
                };
                // The wait for a slot can be long (creates clone workspaces);
                // a grant revoked meanwhile must stop this request too.
                if !still_allowed(registry, peer, &op) {
                    tracing::debug!(
                        "remote: {} {} refused: grant changed while queued",
                        peer.short(),
                        op_name(&op)
                    );
                    let result = denied(&op, need(&op));
                    if out.send(Frame::Response { id, result }).await.is_err() {
                        break Ended::Stream;
                    }
                    continue;
                }
                let (out, dispatch) = (out.clone(), Arc::clone(&ctx.dispatch));
                tokio::spawn(async move {
                    let result = redact(dispatch.call(localise(op)).await);
                    let _ = out.send(Frame::Response { id, result }).await;
                    drop(permit);
                });
            }
        }
    };

    // Stop the screen streams first: their tasks hold the outbox open.
    drop(attachments);
    // The push holds an outbox sender and never finishes on its own; stop it
    // or the writer below would wait for it forever.
    if let Some(push) = status_push.take() {
        push.abort();
    }
    // Let in-flight answers finish and be written, unless the peer is gone.
    drop(out);
    tokio::select! {
        _ = writer => {}
        _ = conn.closed() => {}
    }
    ended
}

// ── status feed ───────────────────────────────────────────────────────────

enum Subscribed {
    Started,
    /// No `view`; the peer was told and the stream carries on.
    Refused,
    /// The peer is no longer trusted.
    Gone,
    /// The outbox is closed: the stream is ending.
    Closed,
}

const STATUS_NEEDS_VIEW: &str = "the status feed needs view permission; grant it to this Mac in \
                                 the other Mac's Settings → Machines";

/// Start (or restart) this stream's status push. A second `subscribe_status`
/// is a client asking to resync, so the old push stops first.
async fn subscribe_status(
    registry: &PeerRegistry,
    peer: PeerId,
    ctx: &ControlContext,
    out: &tokio::sync::mpsc::Sender<Frame>,
    push: &mut Option<JoinHandle<()>>,
) -> Subscribed {
    if let Some(old) = push.take() {
        old.abort();
        let _ = old.await;
    }
    let Some(trusted) = registry.get(peer) else {
        tracing::info!(
            "remote: {} subscribe_status refused: no longer paired",
            peer.short()
        );
        return Subscribed::Gone;
    };
    let granted = trusted.permissions.view;
    tracing::info!(
        "remote: \u{201c}{}\u{201d} ({}) subscribe_status {}",
        trusted.machine_name,
        peer.short(),
        if granted {
            "allowed"
        } else {
            "refused: needs view"
        }
    );
    if !granted {
        let refused = Frame::StatusRefused {
            message: STATUS_NEEDS_VIEW.into(),
        };
        return match out.send(refused).await {
            Ok(()) => Subscribed::Refused,
            Err(_) => Subscribed::Closed,
        };
    }
    *push = Some(tokio::spawn(push_status(
        ctx.status_feed.clone(),
        out.clone(),
        registry.clone(),
        peer,
    )));
    Subscribed::Started
}

/// Queue a snapshot, then every change, until the stream ends or `view` is
/// revoked. A subscriber that falls behind the feed's buffer gets a fresh
/// snapshot rather than a gap.
async fn push_status(
    feed: StatusFeed,
    out: tokio::sync::mpsc::Sender<Frame>,
    registry: PeerRegistry,
    peer: PeerId,
) {
    loop {
        let sub = feed.subscribe();
        let mut deltas = sub.deltas;
        if out.send(sub.snapshot.into()).await.is_err() {
            return;
        }
        loop {
            match deltas.recv().await {
                Ok(delta) => {
                    // Live, per change: revoking `view` stops the very next one.
                    if !registry.permissions(peer).is_some_and(|p| p.view) {
                        tracing::info!(
                            "remote: {} status feed stopped: view revoked",
                            peer.short()
                        );
                        let refused = Frame::StatusRefused {
                            message: STATUS_NEEDS_VIEW.into(),
                        };
                        let _ = out.send(refused).await;
                        return;
                    }
                    if out.send(delta.into()).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(
                        "remote: {} fell {missed} changes behind; resending a snapshot",
                        peer.short()
                    );
                    break;
                }
                Err(RecvError::Closed) => return,
            }
        }
    }
}

enum Admission {
    Allowed,
    Refused(Response),
    /// The peer is no longer trusted.
    Gone,
}

/// Decide one request against the peer's grant as it is now, and write its
/// one log line.
fn admit(registry: &PeerRegistry, peer: PeerId, op: &Request) -> Admission {
    let Some(trusted) = registry.get(peer) else {
        tracing::info!(
            "remote: {} {} refused: no longer paired",
            peer.short(),
            op_name(op)
        );
        return Admission::Gone;
    };
    let need = need(op);
    let granted = allows(trusted.permissions, need);
    tracing::info!(
        "remote: \u{201c}{}\u{201d} ({}) {} {}",
        trusted.machine_name,
        peer.short(),
        op_name(op),
        if granted {
            "allowed"
        } else if need == Need::Control {
            "refused: needs control"
        } else {
            "refused: needs view"
        }
    );
    if granted {
        Admission::Allowed
    } else {
        Admission::Refused(denied(op, need))
    }
}

fn still_allowed(registry: &PeerRegistry, peer: PeerId, op: &Request) -> bool {
    registry
        .permissions(peer)
        .is_some_and(|p| allows(p, need(op)))
}

fn denied(op: &Request, need: Need) -> Response {
    let what = match need {
        Need::View => "view",
        Need::Control => "control",
    };
    Response::Error {
        code: ErrorCode::PermissionDenied,
        message: format!(
            "{} needs {what} permission; grant it to this Mac in the other Mac's \
             Settings → Machines",
            op_name(op)
        ),
    }
}

/// Strip from an answer what only means something on this machine: a
/// project's source path (which names this Mac's user and folders) and a
/// session's `reply_to` socket. A peer can use neither, and `view` is status,
/// not this machine's layout.
fn redact(response: Response) -> Response {
    match response {
        Response::Projects { mut projects } => {
            for p in &mut projects {
                p.source_path = String::new();
            }
            Response::Projects { projects }
        }
        Response::Sessions { mut sessions } => {
            for s in &mut sessions {
                s.reply_to = None;
            }
            Response::Sessions { sessions }
        }
        Response::Status { mut session } => {
            session.reply_to = None;
            Response::Status { session }
        }
        Response::Created { mut session } => {
            session.reply_to = None;
            Response::Created { session }
        }
        other => other,
    }
}

/// Encode a frame for the writer. A response too large for the wire becomes
/// an error the caller can read, rather than a request that never answers.
fn fit(frame: &Frame) -> Option<Vec<u8>> {
    match wire::encode(frame) {
        Ok(body) => Some(body),
        Err(e) => {
            let Frame::Response { id, .. } = frame else {
                tracing::warn!("remote: dropped a frame that would not encode: {e}");
                return None;
            };
            wire::encode(&Frame::Response {
                id: *id,
                result: Response::Error {
                    code: ErrorCode::Internal,
                    message: format!("the answer is too large for the remote link: {e}"),
                },
            })
            .ok()
        }
    }
}

/// Close `conn` for `reason`, after letting the peer hang up first: closing a
/// QUIC connection at once can drop the refusal still in flight.
pub async fn close_after_linger(conn: &Arc<dyn PeerConnection>, reason: &str) {
    let _ = timeout(super::pairing::host::LINGER, conn.closed()).await;
    conn.close(reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_ops_need_view_and_changing_ops_need_control() {
        let view = [
            Request::ProjectsList,
            Request::SessionsList,
            Request::SessionsStatus {
                session_id: "s".into(),
            },
        ];
        let control = [
            Request::SessionsInterrupt {
                session_id: "s".into(),
            },
            Request::SessionsDiscard {
                session_id: "s".into(),
            },
            Request::SessionsCreate(crate::dispatch::protocol::CreateRequest {
                project: "p".into(),
                name: "n".into(),
                prompt: "x".into(),
                orchestration: crate::session::Orchestration::Nothing,
                caller_session_id: None,
                caller_reply_to: None,
                group: None,
            }),
        ];
        for op in &view {
            assert_eq!(need(op), Need::View, "{op:?}");
        }
        for op in &control {
            assert_eq!(need(op), Need::Control, "{op:?}");
        }
    }

    #[test]
    fn a_grant_covers_only_its_own_need() {
        let view_only = PeerPermissions::NEW_PEER;
        assert!(allows(view_only, Need::View));
        assert!(!allows(view_only, Need::Control));
        let control_only = PeerPermissions {
            view: false,
            control: true,
        };
        assert!(!allows(control_only, Need::View));
        assert!(allows(control_only, Need::Control));
    }

    /// Captures formatted log lines for the duration of a closure.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn logged(f: impl FnOnce()) -> Vec<String> {
        let lines = Lines::default();
        let sink = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_string).collect()
    }

    /// AC: each remote request writes one log line naming the peer — allowed
    /// and refused alike.
    #[test]
    fn each_request_logs_one_line_naming_the_peer() {
        let registry = PeerRegistry::load(Arc::new(
            crate::repositories::fakes::InMemoryPeersRepository::default(),
        ));
        let id = super::super::identity::LocalIdentity::ephemeral().id();
        registry
            .add_paired(super::super::peers::TrustedPeer::newly_paired(id, "Laptop"))
            .unwrap();
        for (op, verdict) in [
            (Request::SessionsList, "allowed"),
            (
                Request::SessionsDiscard {
                    session_id: "s".into(),
                },
                "refused: needs control",
            ),
        ] {
            let lines = logged(|| {
                admit(&registry, id, &op);
            });
            assert_eq!(lines.len(), 1, "{lines:?}");
            let line = &lines[0];
            assert!(line.contains("Laptop"), "{line}");
            assert!(line.contains(&id.short()), "{line}");
            assert!(line.contains(op_name(&op)), "{line}");
            assert!(line.contains(verdict), "{line}");
        }
    }

    #[test]
    fn a_remote_create_drops_host_local_caller_claims() {
        let op = Request::SessionsCreate(crate::dispatch::protocol::CreateRequest {
            project: "p".into(),
            name: "n".into(),
            prompt: "x".into(),
            orchestration: crate::session::Orchestration::Nothing,
            caller_session_id: Some("a-local-session".into()),
            caller_reply_to: Some("uds:/tmp/cc-socks/1.sock".into()),
            group: Some("g".into()),
        });
        let Request::SessionsCreate(req) = localise(op) else {
            panic!("still a create");
        };
        assert_eq!(req.caller_session_id, None);
        assert_eq!(req.caller_reply_to, None);
        assert_eq!(req.group.as_deref(), Some("g"), "everything else kept");
    }

    #[test]
    fn answers_lose_host_local_paths_and_sockets() {
        let projects = redact(Response::Projects {
            projects: vec![crate::dispatch::protocol::ProjectSummary {
                id: "p".into(),
                name: "allele".into(),
                source_path: "/Users/someone/code/allele".into(),
                session_count: 1,
            }],
        });
        let Response::Projects { projects } = projects else {
            panic!()
        };
        assert_eq!(projects[0].source_path, "");
        assert_eq!(projects[0].name, "allele");

        let summary = crate::dispatch::protocol::SessionSummary {
            session_id: "s".into(),
            name: "n".into(),
            project: "allele".into(),
            state: crate::dispatch::protocol::SessionState::Running,
            state_age_secs: 0,
            dispatched: true,
            depth: 1,
            reply_to: Some("uds:/tmp/cc-socks/1.sock".into()),
            group: None,
        };
        let Response::Sessions { sessions } = redact(Response::Sessions {
            sessions: vec![summary.clone()],
        }) else {
            panic!()
        };
        assert_eq!(sessions[0].reply_to, None);
        let Response::Status { session } = redact(Response::Status {
            session: Box::new(summary),
        }) else {
            panic!()
        };
        assert_eq!(session.reply_to, None);
    }

    #[test]
    fn an_oversized_response_becomes_a_readable_error() {
        let huge = Frame::Response {
            id: 3,
            result: Response::Error {
                code: ErrorCode::Internal,
                message: "x".repeat(MAX_PEER_FRAME),
            },
        };
        let body = fit(&huge).expect("replaced, not dropped");
        match wire::decode(&body).unwrap() {
            Frame::Response {
                id: 3,
                result: Response::Error { message, .. },
            } => assert!(message.contains("too large"), "{message}"),
            other => panic!("{other:?}"),
        }
    }
}
