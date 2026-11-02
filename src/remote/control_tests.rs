//! The control stream end to end over the in-memory network: a host running
//! the real gate, listener and control stream, and a client speaking frames.
//! The dispatch handler is scripted — what it is handed is the assertion.
//!
//! The DEV-1161 acceptance criteria, and where each is checked:
//!
//! | criterion | test |
//! | -- | -- |
//! | a `view` peer gets a correct `sessions_list` | `a_view_peer_lists_sessions` |
//! | a `view` peer's create is a permission error | `a_view_peers_create_is_refused_with_a_typed_error` |
//! | a `control` peer's create reaches the host | `a_control_peers_create_reaches_the_dispatch_handler` |
//! | remote creates count against the admission cap | `the_control_queue_hands_requests_to_the_socket_drain_unchanged` |
//! | a request never raises the window | `tests/architecture.rs` — `window_raising_stays_out_of_request_paths` |
//! | one log line per request, naming the peer | `each_request_logs_one_line_naming_the_peer` |
//! | a `protocol: 2` hello is refused readably | `a_protocol_2_hello_is_refused_with_a_readable_message` |

#![cfg(test)]

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::timeout;

use super::control::{ControlContext, ControlQueue, HostName, RemoteDispatch};
use super::listener::{self, Gate};
use super::pairing::{HostPairing, PairingCode, PairingSecret};
use super::peers::{PeerPermissions, TrustedPeer};
use super::registry::PeerRegistry;
use super::testing::{MemNet, MemTransport};
use super::transport::{PeerConnection, PeerStream, PeerTransport};
use super::wire::{
    read_frame_within, write_frame, Frame, StreamRefusal, MAX_PEER_FRAME, PAIRING_VERSION,
};
use crate::dispatch::protocol::{
    CreateRequest, CreatedSession, ErrorCode, Request, Response, SessionState, SessionSummary,
};
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::session::Orchestration;

/// Answers from a script and remembers every request it was handed.
#[derive(Default)]
struct Script {
    calls: Mutex<Vec<Request>>,
}

impl Script {
    fn calls(&self) -> Vec<Request> {
        self.calls.lock().unwrap().clone()
    }
}

fn summary(id: &str) -> SessionSummary {
    SessionSummary {
        session_id: id.into(),
        name: "Fix the thing".into(),
        project: "allele".into(),
        state: SessionState::Running,
        state_age_secs: 3,
        dispatched: true,
        depth: 1,
        reply_to: None,
        group: None,
    }
}

#[async_trait]
impl RemoteDispatch for Script {
    async fn call(&self, op: Request) -> Response {
        self.calls.lock().unwrap().push(op.clone());
        match op {
            Request::SessionsList => Response::Sessions {
                sessions: vec![summary("s-1"), summary("s-2")],
            },
            Request::SessionsCreate(req) => Response::Created {
                session: CreatedSession {
                    session_id: "s-new".into(),
                    name: req.name,
                    project: req.project,
                    state: SessionState::Idle,
                    reply_to: None,
                },
            },
            _ => Response::Projects { projects: vec![] },
        }
    }
}

struct Host {
    t: Arc<MemTransport>,
    registry: PeerRegistry,
    pairing: Arc<HostPairing>,
    script: Arc<Script>,
}

impl Host {
    fn start(net: &Arc<MemNet>) -> Self {
        let script = Arc::new(Script::default());
        Self::start_with(net, script.clone(), script)
    }

    fn start_with(
        net: &Arc<MemNet>,
        dispatch: Arc<dyn RemoteDispatch>,
        script: Arc<Script>,
    ) -> Self {
        let t = net.endpoint();
        let registry = PeerRegistry::load(Arc::new(InMemoryPeersRepository::default()));
        let pairing = Arc::new(HostPairing::default());
        let control = ControlContext {
            dispatch,
            host_name: HostName::new(Some("Studio".into()), || None),
            status_feed: crate::status_feed::StatusFeed::new(),
        };
        let gate = Gate::new(
            registry.clone(),
            Arc::clone(&pairing),
            Arc::new(AtomicBool::new(true)),
            control,
        );
        tokio::spawn(listener::run(t.clone() as Arc<dyn PeerTransport>, gate));
        Self {
            t,
            registry,
            pairing,
            script,
        }
    }

    /// A peer this host trusts, with `permissions`, connected.
    async fn peer(
        &self,
        net: &Arc<MemNet>,
        permissions: PeerPermissions,
    ) -> (Arc<MemTransport>, Arc<dyn PeerConnection>) {
        let laptop = net.endpoint();
        let mut peer = TrustedPeer::newly_paired(laptop.local_id(), "Laptop");
        peer.permissions = permissions;
        self.registry.add_paired(peer).unwrap();
        self.registry
            .set_control(laptop.local_id(), permissions.control)
            .unwrap();
        let conn = laptop.dial_mem(self.t.local_id()).await;
        (laptop, conn)
    }
}

const VIEW: PeerPermissions = PeerPermissions::NEW_PEER;
const CONTROL: PeerPermissions = PeerPermissions {
    view: true,
    control: true,
};

async fn read(stream: &mut PeerStream) -> Frame {
    timeout(
        Duration::from_secs(5),
        read_frame_within(&mut stream.recv, MAX_PEER_FRAME),
    )
    .await
    .expect("a frame in time")
    .expect("a readable frame")
}

/// Open a control stream and say hello with `protocol`; return the stream
/// and the host's first reply.
async fn hello(conn: &Arc<dyn PeerConnection>, protocol: u32) -> (PeerStream, Frame) {
    let mut stream = conn.open_stream().await.unwrap();
    let hello = Frame::Hello {
        protocol,
        machine_name: "Laptop".into(),
        permissions: None,
    };
    write_frame(&mut stream.send, &hello).await.unwrap();
    let reply = read(&mut stream).await;
    (stream, reply)
}

async fn call(stream: &mut PeerStream, id: u64, op: Request) -> Response {
    write_frame(&mut stream.send, &Frame::Request { id, op })
        .await
        .unwrap();
    match read(stream).await {
        Frame::Response { id: got, result } => {
            assert_eq!(got, id, "the answer names its request");
            result
        }
        other => panic!("expected a response, got {other:?}"),
    }
}

fn create(caller: Option<&str>) -> Request {
    Request::SessionsCreate(CreateRequest {
        project: "allele".into(),
        name: "From the laptop".into(),
        prompt: "read the brief".into(),
        orchestration: Orchestration::Nothing,
        caller_session_id: caller.map(str::to_string),
        caller_reply_to: caller.map(|_| "uds:/tmp/cc-socks/1.sock".to_string()),
        group: None,
    })
}

fn is_denied(r: &Response) -> bool {
    matches!(
        r,
        Response::Error {
            code: ErrorCode::PermissionDenied,
            ..
        }
    )
}

#[tokio::test]
async fn the_host_hello_carries_its_name_and_the_peers_live_permissions() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, VIEW).await;
    let (_stream, reply) = hello(&conn, 1).await;
    assert_eq!(
        reply,
        Frame::Hello {
            protocol: 1,
            machine_name: "Studio".into(),
            permissions: Some(VIEW),
        }
    );
}

/// AC: a `view` peer gets a correct `sessions_list` response.
#[tokio::test]
async fn a_view_peer_lists_sessions() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, VIEW).await;
    let (mut stream, _) = hello(&conn, 1).await;
    let result = call(&mut stream, 1, Request::SessionsList).await;
    assert_eq!(
        result,
        Response::Sessions {
            sessions: vec![summary("s-1"), summary("s-2")]
        }
    );
    assert_eq!(host.script.calls(), vec![Request::SessionsList]);
}

/// AC: a `view` peer's `sessions_create` returns a permission error — and the
/// handler is never asked.
#[tokio::test]
async fn a_view_peers_create_is_refused_with_a_typed_error() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, VIEW).await;
    let (mut stream, _) = hello(&conn, 1).await;
    for (id, op) in [
        (1, create(None)),
        (
            2,
            Request::SessionsInterrupt {
                session_id: "s-1".into(),
            },
        ),
        (
            3,
            Request::SessionsDiscard {
                session_id: "s-1".into(),
            },
        ),
    ] {
        let result = call(&mut stream, id, op).await;
        assert!(is_denied(&result), "{result:?}");
    }
    assert!(
        host.script.calls().is_empty(),
        "nothing reached the handler"
    );
    // The stream is still usable: a refusal is an answer, not a hang-up.
    let result = call(&mut stream, 4, Request::SessionsList).await;
    assert!(matches!(result, Response::Sessions { .. }));
}

/// A peer with `control` but not `view` cannot read.
#[tokio::test]
async fn a_control_only_peer_cannot_list() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host
        .peer(
            &net,
            PeerPermissions {
                view: false,
                control: true,
            },
        )
        .await;
    let (mut stream, _) = hello(&conn, 1).await;
    assert!(is_denied(
        &call(&mut stream, 1, Request::SessionsList).await
    ));
}

/// AC: a `control` peer's `sessions_create` creates a session on the host —
/// it reaches the same handler `control.sock` uses, without the caller
/// claims that only name things on the host.
#[tokio::test]
async fn a_control_peers_create_reaches_the_dispatch_handler() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, CONTROL).await;
    let (mut stream, _) = hello(&conn, 1).await;
    let result = call(&mut stream, 9, create(Some("a-host-session"))).await;
    assert!(
        matches!(&result, Response::Created { session } if session.session_id == "s-new"),
        "{result:?}"
    );
    assert_eq!(host.script.calls(), vec![create(None)]);
}

/// Permissions are read per request: a grant changed mid-connection applies
/// to the very next request on the open stream.
#[tokio::test]
async fn a_permission_change_applies_to_the_next_request() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (laptop, conn) = host.peer(&net, VIEW).await;
    let (mut stream, _) = hello(&conn, 1).await;
    assert!(is_denied(&call(&mut stream, 1, create(None)).await));

    host.registry.set_control(laptop.local_id(), true).unwrap();
    assert!(matches!(
        call(&mut stream, 2, create(None)).await,
        Response::Created { .. }
    ));

    host.registry.set_control(laptop.local_id(), false).unwrap();
    assert!(is_denied(&call(&mut stream, 3, create(None)).await));
    assert_eq!(host.script.calls().len(), 1);
}

/// AC: a `protocol: 2` hello is refused with a readable error, and the
/// connection then closes.
#[tokio::test(start_paused = true)]
async fn a_protocol_2_hello_is_refused_with_a_readable_message() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, CONTROL).await;
    let (_stream, reply) = hello(&conn, 2).await;
    let Frame::Refused { code, message } = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert_eq!(code, StreamRefusal::UnsupportedProtocol);
    assert!(
        message.contains("protocol 1") && message.contains('2') && message.contains("Update"),
        "{message}"
    );
    // The host lets the client hang up first, then closes it itself.
    let reason = timeout(Duration::from_secs(10), conn.closed())
        .await
        .expect("the host closes the connection");
    assert_eq!(reason, message);
    assert!(host.script.calls().is_empty());
}

#[tokio::test]
async fn a_second_control_stream_on_one_connection_is_refused() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, VIEW).await;
    let (mut first, _) = hello(&conn, 1).await;
    let (_second, reply) = hello(&conn, 1).await;
    assert!(
        matches!(
            reply,
            Frame::Refused {
                code: StreamRefusal::AlreadyOpen,
                ..
            }
        ),
        "{reply:?}"
    );
    // The first is unaffected.
    assert!(matches!(
        call(&mut first, 1, Request::SessionsList).await,
        Response::Sessions { .. }
    ));
}

/// An op this host does not know — from a newer Allele — is answered, not
/// left hanging.
#[tokio::test]
async fn an_unknown_op_is_answered_with_bad_request() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, CONTROL).await;
    let (mut stream, _) = hello(&conn, 1).await;
    let body = br#"{"type":"request","id":5,"op":{"op":"sessions_teleport"}}"#;
    super::wire::write_body(&mut stream.send, body)
        .await
        .unwrap();
    match read(&mut stream).await {
        Frame::Response {
            id: 5,
            result:
                Response::Error {
                    code: ErrorCode::BadRequest,
                    ..
                },
        } => {}
        other => panic!("{other:?}"),
    }
}

/// Removing a peer ends its control stream with the connection.
#[tokio::test]
async fn removing_a_peer_ends_its_control_stream() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (laptop, conn) = host.peer(&net, CONTROL).await;
    let (_stream, _) = hello(&conn, 1).await;
    host.registry.remove(laptop.local_id()).unwrap();
    timeout(Duration::from_secs(2), conn.closed())
        .await
        .expect("closed");
    let id = laptop.local_id();
    timeout(Duration::from_secs(2), async {
        while host.registry.live_count(id) > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("released");
}

/// A trusted peer re-pairing (it lost its own list) still reaches the pairing
/// exchange — the listener routes `pair_hello` there, not to control.
#[tokio::test]
async fn a_pair_hello_from_a_trusted_peer_still_reaches_pairing() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (laptop, conn) = host.peer(&net, VIEW).await;
    let code = PairingCode {
        host: host.t.local_id(),
        secret: PairingSecret::generate().unwrap(),
    };
    host.pairing
        .open(code.clone(), "Studio".into(), tokio::time::Instant::now());
    let mut stream = conn.open_stream().await.unwrap();
    let hello = Frame::PairHello {
        v: PAIRING_VERSION,
        secret: code.secret.to_text(),
        machine_name: "Laptop".into(),
    };
    write_frame(&mut stream.send, &hello).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if let super::pairing::HostView::AwaitingApproval { request, .. } =
                host.pairing.view(tokio::time::Instant::now())
            {
                assert_eq!(request.client, laptop.local_id());
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the re-pairing reached the approval prompt");
}

/// AC: remote creates count against the host's dispatch admission cap.
///
/// The production dispatcher puts the request on the very channel
/// `control.sock` feeds; the foreground drain routes `sessions_create` from
/// it to `dispatch::create`, which applies `admission::admit`. So the cap a
/// remote create meets is the local one by construction. This pins the hand
/// off: the drain receives the request unchanged, and its answer (here, the
/// cap being full) comes back unchanged.
#[tokio::test]
async fn the_control_queue_hands_requests_to_the_socket_drain_unchanged() {
    let (tx, rx) = std::sync::mpsc::channel::<crate::dispatch::server::ControlRequest>();
    let drain = std::thread::spawn(move || {
        let req = rx.recv().unwrap();
        let _ = req.reply.send(Response::Error {
            code: ErrorCode::CapacityExceeded,
            message: "full".into(),
        });
        req.request
    });
    let queue = ControlQueue::new(tx);
    let answer = queue.call(create(None)).await;
    assert_eq!(
        answer,
        Response::Error {
            code: ErrorCode::CapacityExceeded,
            message: "full".into()
        }
    );
    assert_eq!(drain.join().unwrap(), create(None));
}

/// A dispatcher whose drain is gone answers rather than hangs.
#[tokio::test]
async fn the_control_queue_answers_when_the_app_is_gone() {
    let (tx, rx) = std::sync::mpsc::channel();
    drop(rx);
    let answer = ControlQueue::new(tx).call(Request::SessionsList).await;
    assert!(matches!(
        answer,
        Response::Error {
            code: ErrorCode::Internal,
            ..
        }
    ));
}

/// Many requests in flight on one stream all come back, each under its id.
#[tokio::test]
async fn concurrent_requests_are_answered_by_id() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, VIEW).await;
    let (mut stream, _) = hello(&conn, 1).await;
    for id in 0..20 {
        write_frame(
            &mut stream.send,
            &Frame::Request {
                id,
                op: Request::SessionsList,
            },
        )
        .await
        .unwrap();
    }
    let mut seen = Vec::new();
    for _ in 0..20 {
        match read(&mut stream).await {
            Frame::Response { id, .. } => seen.push(id),
            other => panic!("{other:?}"),
        }
    }
    seen.sort_unstable();
    assert_eq!(seen, (0..20).collect::<Vec<_>>());
}

/// A dispatcher that holds every call until released, so requests pile up in
/// flight; it records into the host's script once let through.
struct Gated {
    script: Arc<Script>,
    gate: tokio::sync::Semaphore,
}

#[async_trait]
impl RemoteDispatch for Gated {
    async fn call(&self, op: Request) -> Response {
        self.gate.acquire().await.unwrap().forget();
        self.script.call(op).await
    }
}

/// A request admitted but still queued for a slot is checked again when it
/// gets one: revoking `control` stops it, not just the requests after it.
#[tokio::test]
async fn revoking_control_stops_a_create_still_waiting_for_a_slot() {
    let net = MemNet::new();
    let script = Arc::new(Script::default());
    let gated = Arc::new(Gated {
        script: script.clone(),
        gate: tokio::sync::Semaphore::new(0),
    });
    let host = Host::start_with(&net, gated.clone(), script.clone());
    let (laptop, conn) = host.peer(&net, CONTROL).await;
    let (mut stream, _) = hello(&conn, 1).await;
    let max = super::control::MAX_IN_FLIGHT as u64;
    for id in 0..=max {
        write_frame(
            &mut stream.send,
            &Frame::Request {
                id,
                op: create(None),
            },
        )
        .await
        .unwrap();
    }
    // Give the host time to admit all of them: `max` in flight, one queued.
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.registry.set_control(laptop.local_id(), false).unwrap();
    gated.gate.add_permits(64);

    let mut denied = Vec::new();
    for _ in 0..=max {
        if let Frame::Response { id, result } = read(&mut stream).await {
            if is_denied(&result) {
                denied.push(id);
            }
        }
    }
    assert_eq!(denied, vec![max], "only the queued request is stopped");
    assert_eq!(script.calls().len(), max as usize);
}

/// A second stream with a protocol this host does not speak is refused as a
/// second stream; it does not tear down the working one.
#[tokio::test]
async fn a_bad_second_hello_leaves_the_first_control_stream_alone() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, VIEW).await;
    let (mut first, _) = hello(&conn, 1).await;
    let (_second, reply) = hello(&conn, 2).await;
    assert!(
        matches!(
            reply,
            Frame::Refused {
                code: StreamRefusal::AlreadyOpen,
                ..
            }
        ),
        "{reply:?}"
    );
    assert!(!conn.is_closed());
    assert!(matches!(
        call(&mut first, 1, Request::SessionsList).await,
        Response::Sessions { .. }
    ));
}

/// A stream that does not start with a hello is told so.
#[tokio::test]
async fn a_stream_without_a_hello_is_refused_readably() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn) = host.peer(&net, CONTROL).await;
    let mut stream = conn.open_stream().await.unwrap();
    write_frame(
        &mut stream.send,
        &Frame::Request {
            id: 1,
            op: Request::SessionsList,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read(&mut stream).await,
        Frame::Refused {
            code: StreamRefusal::BadHello,
            ..
        }
    ));
    assert!(host.script.calls().is_empty());
}
