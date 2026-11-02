//! Screen streams end to end over the in-memory network (DEV-1165): a host
//! running the real gate, listener and control stream, a real alacritty
//! `Term` behind it, and a client using [`ScreenClient`].
//!
//! | criterion | test |
//! | -- | -- |
//! | attach opens a host-initiated stream: `screen_open`, then a snapshot | `attach_opens_a_screen_stream_that_starts_with_a_snapshot` |
//! | a peer without `view` is refused | `a_peer_without_view_is_refused_attach` |
//! | an unknown session is refused with its own code | `an_unknown_session_is_refused_with_session_not_found` |
//! | the client's grid follows the host screen | `the_client_grid_follows_the_host_screen` |
//! | scrolling up loads host scrollback | `scrolling_up_on_the_client_loads_host_scrollback` |
//! | detach stops the stream | `detach_ends_the_hosts_screen_stream` |
//! | requests still answered while attached | `requests_are_still_answered_while_a_session_is_attached` |
//! | removing the peer ends its streams | `removing_the_peer_ends_its_screen_stream` |
//! | an attach the host refuses fails the client call | `a_refused_attach_fails_the_client_call_with_the_hosts_message` |
//! | a failure for another attach never fails this one | `a_failure_for_another_attach_does_not_fail_a_pending_one` |
//! | a host that cannot look the session up says so | `a_lookup_the_host_cannot_answer_is_internal_not_not_found` |
//! | a re-attach that fails leaves the working stream | `a_failed_reattach_keeps_the_working_stream` |
//! | a full-size screen fits the link | `a_full_size_colour_screen_streams` |

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use async_trait::async_trait;
use tokio::time::{sleep, timeout};

use super::control::{ControlContext, HostName, RemoteDispatch};
use super::listener::{self, Gate};
use super::pairing::HostPairing;
use super::peers::{PeerPermissions, TrustedPeer};
use super::registry::PeerRegistry;
use super::screen::{ScreenClient, ScreenSource};
use super::testing::{MemNet, MemTransport};
use super::transport::{PeerConnection, PeerStream, PeerTransport};
use super::wire::{read_frame_within, write_frame, Frame, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::dispatch::protocol::{ErrorCode, Request, Response};
use crate::errors::{AlleleError, Result};
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::terminal::mirror::{capture, capture_history, MirrorTerm, ScreenFrame, SharedTerm};

const COLUMNS: usize = 40;
const LINES: usize = 8;
const HISTORY: usize = 1_000;
const WAIT: Duration = Duration::from_secs(5);

type HostTerm = Arc<FairMutex<Term<VoidListener>>>;

fn term(columns: usize, lines: usize) -> HostTerm {
    let config = Config {
        scrolling_history: HISTORY,
        ..Default::default()
    };
    Arc::new(FairMutex::new(Term::new(
        config,
        &TermSize::new(columns, lines),
        VoidListener,
    )))
}

fn feed(term: &HostTerm, bytes: &[u8]) {
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut *term.lock(), bytes);
}

/// Terminals by session id, as the app's sessions would hold them.
/// `s-busy` stands for a host that could not answer the lookup in time.
#[derive(Default)]
struct Screens(Mutex<HashMap<String, HostTerm>>);

#[async_trait]
impl ScreenSource for Screens {
    async fn find(&self, session_id: &str) -> Result<Option<Arc<dyn MirrorTerm>>> {
        if session_id == "s-busy" {
            return Err(AlleleError::Remote(
                "the other Mac is busy; try again".into(),
            ));
        }
        let terms = self.0.lock().unwrap();
        Ok(terms
            .get(session_id)
            .map(|term| Arc::new(SharedTerm::new(term, Some(HISTORY))) as Arc<dyn MirrorTerm>))
    }
}

/// Answers `sessions_list` with nothing; enough to show requests still flow.
struct Lists;

#[async_trait]
impl RemoteDispatch for Lists {
    async fn call(&self, _: Request) -> Response {
        Response::Sessions { sessions: vec![] }
    }
}

struct Host {
    t: Arc<MemTransport>,
    registry: PeerRegistry,
    /// The one session this host has, `s-1`.
    term: HostTerm,
    screens: Arc<Screens>,
}

impl Host {
    fn start(net: &Arc<MemNet>) -> Self {
        Self::start_sized(net, COLUMNS, LINES)
    }

    fn start_sized(net: &Arc<MemNet>, columns: usize, lines: usize) -> Self {
        let t = net.endpoint();
        let registry = PeerRegistry::load(Arc::new(InMemoryPeersRepository::default()));
        let term = term(columns, lines);
        let screens = Arc::new(Screens::default());
        screens.0.lock().unwrap().insert("s-1".into(), term.clone());
        let control = ControlContext {
            dispatch: Arc::new(Lists),
            host_name: HostName::new(Some("Studio".into()), || None),
            screens: screens.clone(),
        };
        let gate = Gate::new(
            registry.clone(),
            Arc::new(HostPairing::default()),
            Arc::new(AtomicBool::new(true)),
            control,
        );
        tokio::spawn(listener::run(t.clone() as Arc<dyn PeerTransport>, gate));
        Self {
            t,
            registry,
            term,
            screens,
        }
    }

    /// A trusted peer with `permissions`, connected, its control stream open
    /// and the host's hello read.
    async fn peer(
        &self,
        net: &Arc<MemNet>,
        permissions: PeerPermissions,
    ) -> (Arc<MemTransport>, Arc<dyn PeerConnection>, PeerStream) {
        let laptop = net.endpoint();
        let mut peer = TrustedPeer::newly_paired(laptop.local_id(), "Laptop");
        peer.permissions = permissions;
        self.registry.add_paired(peer).unwrap();
        let conn = laptop.dial_mem(self.t.local_id()).await;
        let mut control = conn.open_stream().await.unwrap();
        let hello = Frame::Hello {
            protocol: PROTOCOL_VERSION,
            machine_name: "Laptop".into(),
            permissions: None,
        };
        write_frame(&mut control.send, &hello).await.unwrap();
        assert!(matches!(read(&mut control).await, Frame::Hello { .. }));
        (laptop, conn, control)
    }
}

const VIEW: PeerPermissions = PeerPermissions::NEW_PEER;
const NOTHING: PeerPermissions = PeerPermissions {
    view: false,
    control: false,
};

async fn read(stream: &mut PeerStream) -> Frame {
    timeout(WAIT, read_frame_within(&mut stream.recv, MAX_PEER_FRAME))
        .await
        .expect("a frame in time")
        .expect("a readable frame")
}

/// Raw attaches use ids from 100, clear of any `ScreenClient`'s.
async fn attach(control: &mut PeerStream, attach_id: u64, session_id: &str) {
    let attach = Frame::Attach {
        attach_id,
        session_id: session_id.into(),
    };
    write_frame(&mut control.send, &attach).await.unwrap();
}

/// Poll `ok` until it holds, or fail after [`WAIT`].
async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn attach_opens_a_screen_stream_that_starts_with_a_snapshot() {
    let net = MemNet::new();
    let host = Host::start(&net);
    feed(&host.term, b"hello from the host");
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;

    attach(&mut control, 100, "s-1").await;
    let mut screen = timeout(WAIT, conn.accept_stream())
        .await
        .expect("the host opens a stream")
        .unwrap();

    match read(&mut screen).await {
        Frame::ScreenOpen {
            attach_id,
            session_id,
        } => {
            assert_eq!(attach_id, 100, "the host echoes the client's attach id");
            assert_eq!(session_id, "s-1");
        }
        other => panic!("expected screen_open, got {other:?}"),
    }
    match read(&mut screen).await {
        Frame::Screen {
            frame: ScreenFrame::Snapshot(snapshot),
        } => {
            assert_eq!(snapshot.columns as usize, COLUMNS);
            assert_eq!(snapshot.lines as usize, LINES);
        }
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[tokio::test]
async fn a_peer_without_view_is_refused_attach() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, NOTHING).await;

    attach(&mut control, 100, "s-1").await;
    match read(&mut control).await {
        Frame::AttachFailed {
            attach_id,
            session_id,
            code,
            ..
        } => {
            assert_eq!(attach_id, 100);
            assert_eq!(session_id, "s-1");
            assert_eq!(code, ErrorCode::PermissionDenied);
        }
        other => panic!("expected attach_failed, got {other:?}"),
    }
    let opened = timeout(Duration::from_millis(200), conn.accept_stream()).await;
    assert!(opened.is_err(), "no screen stream for a refused attach");
}

#[tokio::test]
async fn an_unknown_session_is_refused_with_session_not_found() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, _conn, mut control) = host.peer(&net, VIEW).await;

    attach(&mut control, 100, "s-nope").await;
    match read(&mut control).await {
        Frame::AttachFailed { code, .. } => assert_eq!(code, ErrorCode::SessionNotFound),
        other => panic!("expected attach_failed, got {other:?}"),
    }
}

#[tokio::test]
async fn the_client_grid_follows_the_host_screen() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    assert_eq!(attached.session_id(), "s-1");

    let grid = attached.grid();
    let matches = || {
        let host_screen = capture(&host.term.lock()).screen;
        let grid = grid.lock();
        host_screen
            .iter()
            .enumerate()
            .all(|(i, row)| grid.row(i as i32) == Some(row))
    };
    eventually("the first snapshot", matches).await;

    feed(
        &host.term,
        b"\x1b[2J\x1b[Hline one\r\nline two\r\n\x1b[1;31mred\x1b[0m",
    );
    eventually("the grid catches up with new output", matches).await;
}

#[tokio::test]
async fn scrolling_up_on_the_client_loads_host_scrollback() {
    let net = MemNet::new();
    let host = Host::start(&net);
    for i in 0..300 {
        feed(&host.term, format!("row {i}\r\n").as_bytes());
    }
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    let grid = attached.grid();

    eventually("the snapshot", || grid.lock().history_size() > 0).await;
    attached.scroll_display(250);
    eventually("scrollback paged in", || {
        grid.lock().loaded_history() >= 250
    })
    .await;

    let page = capture_history(&host.term.lock(), 0, 250);
    let grid = grid.lock();
    for (i, row) in page.iter().enumerate() {
        assert_eq!(grid.row(-(i as i32) - 1), Some(row), "scrollback row {i}");
    }
}

#[tokio::test]
async fn detach_ends_the_hosts_screen_stream() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;

    attach(&mut control, 100, "s-1").await;
    let mut screen = conn.accept_stream().await.unwrap();
    assert!(matches!(read(&mut screen).await, Frame::ScreenOpen { .. }));

    let detach = Frame::Detach {
        session_id: "s-1".into(),
    };
    write_frame(&mut control.send, &detach).await.unwrap();

    // Drain until the host's end closes; frames already in flight are fine.
    let ended = timeout(WAIT, async {
        while read_frame_within(&mut screen.recv, MAX_PEER_FRAME)
            .await
            .is_ok()
        {}
    })
    .await;
    assert!(ended.is_ok(), "the host stops the stream on detach");
}

#[tokio::test]
async fn requests_are_still_answered_while_a_session_is_attached() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let _attached = client.attach(&mut control.send, "s-1").await.unwrap();

    let request = Frame::Request {
        id: 7,
        op: Request::SessionsList,
    };
    write_frame(&mut control.send, &request).await.unwrap();
    match read(&mut control).await {
        Frame::Response { id, result } => {
            assert_eq!(id, 7);
            assert!(matches!(result, Response::Sessions { .. }));
        }
        other => panic!("expected a response, got {other:?}"),
    }
}

#[tokio::test]
async fn removing_the_peer_ends_its_screen_stream() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    let mut changed = attached.changed();

    host.registry.remove(laptop.local_id()).unwrap();

    // The mirror task ends with the stream, which closes `changed`.
    let closed = timeout(WAIT, async { while changed.changed().await.is_ok() {} }).await;
    assert!(closed.is_ok(), "the stream ends once the peer is removed");
}

#[tokio::test]
async fn a_refused_attach_fails_the_client_call_with_the_hosts_message() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, control) = host.peer(&net, VIEW).await;
    let PeerStream { mut send, mut recv } = control;
    let client = Arc::new(ScreenClient::new(conn));

    // The control stream's reader, as the client app will have one.
    let routes = Arc::clone(&client);
    tokio::spawn(async move {
        while let Ok(frame) = read_frame_within(&mut recv, MAX_PEER_FRAME).await {
            routes.on_control_frame(&frame);
        }
    });

    let err = match client.attach(&mut send, "s-nope").await {
        Ok(_) => panic!("an unknown session cannot attach"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("no session with id s-nope"),
        "the host's message reaches the caller: {err}"
    );
}

#[tokio::test]
async fn a_failure_for_another_attach_does_not_fail_a_pending_one() {
    // A host that never answers, so the attach stays pending as long as the
    // test needs: no race with a real answer.
    let net = MemNet::new();
    let (laptop, silent) = (net.endpoint(), net.endpoint());
    let conn = laptop.dial_mem(silent.local_id()).await;
    let client = Arc::new(ScreenClient::new(conn));
    let pending = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.attach(&mut tokio::io::sink(), "s-1").await })
    };
    sleep(Duration::from_millis(20)).await;

    // A failure left over from an earlier attach of the same session...
    let failed = |attach_id, message: &str| Frame::AttachFailed {
        attach_id,
        session_id: "s-1".into(),
        code: ErrorCode::SessionNotFound,
        message: message.into(),
    };
    assert!(client.on_control_frame(&failed(999, "the session has ended")));
    sleep(Duration::from_millis(20)).await;
    assert!(
        !pending.is_finished(),
        "a stale failure leaves the attach pending"
    );

    // ...while one for this attach (the client's first id) ends it.
    assert!(client.on_control_frame(&failed(1, "this one")));
    let err = match timeout(WAIT, pending).await.unwrap().unwrap() {
        Ok(_) => panic!("the attach was refused"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("this one"), "{err}");
}

#[tokio::test]
async fn a_lookup_the_host_cannot_answer_is_internal_not_not_found() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, _conn, mut control) = host.peer(&net, VIEW).await;

    attach(&mut control, 100, "s-busy").await;
    match read(&mut control).await {
        Frame::AttachFailed { code, message, .. } => {
            assert_eq!(
                code,
                ErrorCode::Internal,
                "a busy host is not a missing session"
            );
            assert!(message.contains("try again"), "{message}");
        }
        other => panic!("expected attach_failed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_failed_reattach_keeps_the_working_stream() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;

    attach(&mut control, 100, "s-1").await;
    let mut screen = conn.accept_stream().await.unwrap();
    assert!(matches!(read(&mut screen).await, Frame::ScreenOpen { .. }));
    assert!(matches!(read(&mut screen).await, Frame::Screen { .. }));

    // The session can no longer be found, so the re-attach fails...
    host.screens.0.lock().unwrap().remove("s-1");
    attach(&mut control, 101, "s-1").await;
    match read(&mut control).await {
        Frame::AttachFailed {
            attach_id, code, ..
        } => {
            assert_eq!(attach_id, 101);
            assert_eq!(code, ErrorCode::SessionNotFound);
        }
        other => panic!("expected attach_failed, got {other:?}"),
    }

    // ...and the stream already running carries on.
    feed(&host.term, b"still here");
    assert!(
        matches!(read(&mut screen).await, Frame::Screen { .. }),
        "the first stream still sends diffs"
    );
}

#[tokio::test]
async fn a_full_size_colour_screen_streams() {
    let net = MemNet::new();
    let host = Host::start_sized(&net, 200, 60);
    // A style change on every cell: the worst case for the run encoding.
    let mut output = String::new();
    for line in 0..60 {
        for col in 0..200 {
            let c = (line * 200 + col) % 256;
            output.push_str(&format!("\x1b[38;5;{c};48;5;{}m#", 255 - c));
        }
        output.push_str("\r\n");
    }
    feed(&host.term, output.as_bytes());

    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    let grid = attached.grid();
    eventually("a 200x60 snapshot arrives", || {
        let host_screen = capture(&host.term.lock()).screen;
        let grid = grid.lock();
        host_screen
            .iter()
            .enumerate()
            .all(|(i, row)| grid.row(i as i32) == Some(row))
    })
    .await;
}
