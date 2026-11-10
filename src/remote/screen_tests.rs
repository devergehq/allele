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
//!
//! Remote input (DEV-1166):
//!
//! | criterion | test |
//! | -- | -- |
//! | typing on a `control` client appears in the host session | `typing_on_a_control_client_appears_in_the_host_session` |
//! | Ctrl-C from the client reaches the PTY as `0x03` | `ctrl_c_from_the_client_reaches_the_host_pty_as_etx` |
//! | keys reach the host as the keymap encoded them | `keys_reach_the_host_exactly_as_the_keymap_encodes_them` |
//! | a multi-line paste arrives as one bracketed paste | `a_multiline_paste_arrives_as_one_bracketed_write` |
//! | a paste into a program without bracketed paste is the bare text | `a_paste_without_bracketed_mode_is_one_plain_write` |
//! | input from a `view` peer is dropped and refused | `input_from_a_view_peer_is_dropped_and_refused` |
//! | `control` is read live, per frame | `control_granted_or_withdrawn_mid_attach_applies_to_the_next_key` |
//! | an oversized input frame is refused, not written | `an_oversized_input_frame_is_refused_and_not_written` |
//! | the client refuses an oversized paste itself | `the_client_refuses_an_oversized_paste_before_sending` |
//! | the host lists the attached machine until it detaches | `the_host_lists_an_attached_machine_until_it_detaches` |
//!
//! Echo latency (DEV-1242):
//!
//! | criterion | test |
//! | -- | -- |
//! | input adds at most one frame per key to the 30/s cap | `input_adds_at_most_one_frame_per_key_to_the_frame_rate_cap` |
//! | refused input adds no frames | `refused_input_adds_no_frames` |
//! | A/B echo latency (manual, `--ignored`) | `measure_keystroke_echo_latency` |

#![cfg(test)]

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
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
use super::viewers::Viewers;
use super::wire::{read_frame_within, write_frame, Frame, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::dispatch::protocol::{ErrorCode, Request, Response};
use crate::errors::{AlleleError, Result};
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::terminal::keymap::KeymapConfig;
use crate::terminal::mirror::{
    capture, capture_history, MirrorTerm, ScreenFrame, SharedTerm, MIN_FRAME_INTERVAL,
};

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

/// Every write remote input made to a PTY, one entry per write.
type Typed = Arc<Mutex<Vec<Vec<u8>>>>;

/// Terminals by session id, as the app's sessions would hold them.
/// `s-busy` stands for a host that could not answer the lookup in time.
///
/// Input written to a terminal is recorded in `typed` and echoed into the
/// `Term`, as a shell with echo on would, so the client sees what it typed.
#[derive(Default)]
struct Screens {
    terms: Mutex<HashMap<String, HostTerm>>,
    typed: Typed,
    /// Every "Fit to this window" that reached the host (DEV-1167).
    resized: Mutex<Vec<(String, u16, u16)>>,
}

#[async_trait]
impl ScreenSource for Screens {
    async fn find(&self, session_id: &str) -> Result<Option<Arc<dyn MirrorTerm>>> {
        if session_id == "s-busy" {
            return Err(AlleleError::Remote(
                "the other Mac is busy; try again".into(),
            ));
        }
        let terms = self.terms.lock().unwrap();
        Ok(terms.get(session_id).map(|term| {
            let typed = Arc::clone(&self.typed);
            let echo = Arc::downgrade(term);
            let sink = move |bytes: &[u8]| {
                typed.lock().unwrap().push(bytes.to_vec());
                if let Some(term) = echo.upgrade() {
                    feed(&term, bytes);
                }
                true
            };
            Arc::new(SharedTerm::new(term, Some(HISTORY)).with_input(Box::new(sink)))
                as Arc<dyn MirrorTerm>
        }))
    }

    async fn resize(&self, session_id: &str, columns: u16, lines: u16) -> Result<bool> {
        let Some(term) = self.terms.lock().unwrap().get(session_id).cloned() else {
            return Ok(false);
        };
        term.lock()
            .resize(TermSize::new(columns as usize, lines as usize));
        self.resized
            .lock()
            .unwrap()
            .push((session_id.to_string(), columns, lines));
        Ok(true)
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
    viewers: Viewers,
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
        screens
            .terms
            .lock()
            .unwrap()
            .insert("s-1".into(), term.clone());
        let viewers = Viewers::default();
        let control = ControlContext {
            dispatch: Arc::new(Lists),
            host_name: HostName::new(Some("Studio".into()), || None),
            screens: screens.clone(),
            status_feed: crate::status_feed::StatusFeed::new(),
            viewers: viewers.clone(),
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
            viewers,
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
const CONTROL: PeerPermissions = PeerPermissions {
    view: true,
    control: true,
};
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
    host.screens.terms.lock().unwrap().remove("s-1");
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

// ── remote input (DEV-1166) ──────────────────────────────────────────────

impl Host {
    /// Every PTY write remote input has made, in order.
    fn typed(&self) -> Vec<Vec<u8>> {
        self.screens.typed.lock().unwrap().clone()
    }

    /// The host screen's text, rows joined by newlines.
    fn screen_text(&self) -> String {
        let cap = capture(&self.term.lock());
        cap.screen
            .iter()
            .map(|row| row.iter().map(|cell| cell.c).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn ctrl() -> gpui::Modifiers {
    gpui::Modifiers {
        control: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn typing_on_a_control_client_appears_in_the_host_session() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    for key in ["l", "s"] {
        let bytes = KeymapConfig::default()
            .resolve(key, &gpui::Modifiers::default(), Some(key))
            .unwrap();
        attached.send_input(&bytes).unwrap();
    }
    eventually("the keys reach the host PTY", || {
        host.typed() == vec![b"l".to_vec(), b"s".to_vec()]
    })
    .await;
    assert!(host.screen_text().contains("ls"), "{}", host.screen_text());

    // And the echo comes back to the client's mirror.
    let grid = attached.grid();
    eventually("the client sees its own typing", || {
        grid.lock()
            .row(0)
            .is_some_and(|row| row.iter().map(|c| c.c).collect::<String>().contains("ls"))
    })
    .await;
}

#[tokio::test]
async fn ctrl_c_from_the_client_reaches_the_host_pty_as_etx() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    let bytes = KeymapConfig::default()
        .resolve("c", &ctrl(), Some("c"))
        .unwrap();
    assert_eq!(bytes, vec![0x03], "the keymap's Ctrl-C");
    attached.send_input(&bytes).unwrap();
    // 0x03 written to the PTY is what makes its line discipline send SIGINT
    // to the foreground job: the same write local Ctrl-C makes.
    eventually("ETX reaches the PTY", || host.typed() == vec![vec![0x03]]).await;
}

#[tokio::test]
async fn keys_reach_the_host_exactly_as_the_keymap_encodes_them() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    let keymap = KeymapConfig::default();
    let none = gpui::Modifiers::default();
    let shift = gpui::Modifiers {
        shift: true,
        ..Default::default()
    };
    let alt = gpui::Modifiers {
        alt: true,
        ..Default::default()
    };
    let keys = [
        keymap.resolve("enter", &none, None),
        keymap.resolve("enter", &shift, None),
        keymap.resolve("up", &none, None),
        keymap.resolve("tab", &shift, None),
        keymap.resolve("escape", &none, None),
        keymap.resolve("backspace", &none, None),
        keymap.resolve("b", &alt, Some("∫")),
        keymap.resolve("é", &none, Some("é")),
    ];
    let sent: Vec<Vec<u8>> = keys.into_iter().map(|k| k.expect("a key")).collect();
    for bytes in &sent {
        attached.send_input(bytes).unwrap();
    }
    eventually("every key arrives, in order", || host.typed() == sent).await;
}

#[tokio::test]
async fn a_multiline_paste_arrives_as_one_bracketed_write() {
    let net = MemNet::new();
    let host = Host::start(&net);
    // The program asked for bracketed paste, as Claude Code does.
    feed(&host.term, b"\x1b[?2004h");
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    attached.send_paste("one\ntwo\nthree").unwrap();
    eventually("the paste is written", || !host.typed().is_empty()).await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        host.typed(),
        vec![b"\x1b[200~one\ntwo\nthree\x1b[201~".to_vec()],
        "one write, wrapped once"
    );
}

#[tokio::test]
async fn a_paste_without_bracketed_mode_is_one_plain_write() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    attached.send_paste("one\ntwo").unwrap();
    eventually("the paste is written", || {
        host.typed() == vec![b"one\ntwo".to_vec()]
    })
    .await;
}

#[tokio::test]
async fn input_from_a_view_peer_is_dropped_and_refused() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    let mut changed = attached.changed();

    attached.send_input(b"y").unwrap();
    attached.send_input(b"\r").unwrap();
    let refusal = timeout(WAIT, async {
        loop {
            if let Some(message) = attached.take_refusal() {
                return message;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .expect("a refusal in time");
    assert!(refusal.contains("control"), "{refusal}");

    // Taken once.
    sleep(Duration::from_millis(100)).await;
    let _ = attached.take_refusal();
    assert_eq!(attached.take_refusal(), None);
    assert!(host.typed().is_empty(), "nothing reached the PTY");
    // The mirror itself carries on.
    feed(&host.term, b"still streaming");
    let grid = attached.grid();
    eventually("the mirror still follows the host", || {
        grid.lock().row(0).is_some_and(|row| {
            row.iter()
                .map(|c| c.c)
                .collect::<String>()
                .contains("still")
        })
    })
    .await;
}

#[tokio::test]
async fn control_granted_or_withdrawn_mid_attach_applies_to_the_next_key() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    attached.send_input(b"a").unwrap();
    eventually("refused while view only", || {
        attached.take_refusal().is_some()
    })
    .await;

    host.registry.set_control(laptop.local_id(), true).unwrap();
    attached.send_input(b"b").unwrap();
    eventually("written once control is granted", || {
        host.typed() == vec![b"b".to_vec()]
    })
    .await;

    host.registry.set_control(laptop.local_id(), false).unwrap();
    attached.send_input(b"c").unwrap();
    eventually("refused once control is withdrawn", || {
        attached.take_refusal().is_some()
    })
    .await;
    assert_eq!(host.typed(), vec![b"b".to_vec()]);
}

#[tokio::test]
async fn an_oversized_input_frame_is_refused_and_not_written() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;

    attach(&mut control, 100, "s-1").await;
    let mut screen = conn.accept_stream().await.unwrap();
    assert!(matches!(read(&mut screen).await, Frame::ScreenOpen { .. }));

    // A client that skips its own check.
    let big = Frame::Input {
        text: "x".repeat(super::screen::MAX_INPUT_BYTES + 1),
        paste: true,
    };
    write_frame(&mut screen.send, &big).await.unwrap();
    let code = timeout(WAIT, async {
        loop {
            if let Frame::InputRefused { code, .. } = read(&mut screen).await {
                return code;
            }
        }
    })
    .await
    .expect("a refusal in time");
    assert_eq!(code, ErrorCode::BadRequest);
    assert!(host.typed().is_empty());

    // The stream still takes input that fits.
    let key = Frame::Input {
        text: "k".into(),
        paste: false,
    };
    write_frame(&mut screen.send, &key).await.unwrap();
    eventually("a normal key still lands", || {
        host.typed() == vec![b"k".to_vec()]
    })
    .await;
}

#[tokio::test]
async fn the_client_refuses_an_oversized_paste_before_sending() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();

    let big = "x".repeat(super::screen::MAX_INPUT_BYTES + 1);
    let err = attached.send_paste(&big).unwrap_err().to_string();
    assert!(err.contains("too much"), "{err}");
    let fits = "x".repeat(super::screen::MAX_INPUT_BYTES);
    attached.send_paste(&fits).unwrap();
    eventually("a paste at the limit lands whole", || {
        host.typed() == vec![fits.as_bytes().to_vec()]
    })
    .await;
}

#[tokio::test]
async fn the_host_lists_an_attached_machine_until_it_detaches() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(conn);
    assert!(host.viewers.machines("s-1").is_empty());

    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    eventually("the host lists the machine", || {
        host.viewers.machines("s-1") == vec!["Laptop".to_string()]
    })
    .await;

    let version = host.viewers.version();
    attached.detach(&mut control.send).await.unwrap();
    // The chip's poller redraws on a version change every 500 ms; the
    // entry itself must go well inside the 2 s the chip is allowed.
    timeout(Duration::from_millis(1500), async {
        while !host.viewers.machines("s-1").is_empty() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the entry goes promptly after detach");
    assert_ne!(host.viewers.version(), version);
}

// ── Fit to this window (DEV-1167) ───────────────────────────────────────

fn resize(session_id: &str, columns: u16, lines: u16) -> Frame {
    Frame::Resize {
        session_id: session_id.into(),
        columns,
        lines,
    }
}

/// A `resize` has no reply; give the host time to have acted on one.
async fn settle() {
    sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn fit_to_window_resizes_an_attached_session_and_the_mirror_follows() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(Arc::clone(&conn));
    let attachment = client.attach(&mut control.send, "s-1").await.unwrap();

    write_frame(&mut control.send, &resize("s-1", 100, 30))
        .await
        .unwrap();
    let grid = attachment.grid();
    eventually("the mirror at the new size", || {
        use crate::terminal::grid_source::GridView as _;
        let g = grid.lock();
        g.columns() == 100 && g.screen_lines() == 30
    })
    .await;
    assert_eq!(
        *host.screens.resized.lock().unwrap(),
        vec![("s-1".to_string(), 100, 30)]
    );
}

#[tokio::test]
async fn fit_to_window_needs_control() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, VIEW).await;
    let client = ScreenClient::new(Arc::clone(&conn));
    let _attachment = client.attach(&mut control.send, "s-1").await.unwrap();

    write_frame(&mut control.send, &resize("s-1", 100, 30))
        .await
        .unwrap();
    settle().await;
    assert!(host.screens.resized.lock().unwrap().is_empty());
    assert_eq!(
        host.term.lock().columns(),
        COLUMNS,
        "the host keeps its size"
    );
}

#[tokio::test]
async fn fit_to_window_ignores_a_session_not_attached_on_this_stream() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, _conn, mut control) = host.peer(&net, CONTROL).await;

    write_frame(&mut control.send, &resize("s-1", 100, 30))
        .await
        .unwrap();
    settle().await;
    assert!(
        host.screens.resized.lock().unwrap().is_empty(),
        "a peer may only resize what it is looking at"
    );
}

// ── echo latency (DEV-1242) ──────────────────────────────────────────────

/// Screen frames a raw screen stream receives while the host screen changes
/// every 2 ms (so every 33 ms tick has a frame to send) and `keys` inputs are
/// typed evenly through `window`. Returns the count and how long it took.
async fn frames_while_the_screen_is_busy(
    permissions: PeerPermissions,
    keys: u32,
    window: Duration,
) -> (usize, Duration) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, permissions).await;
    attach(&mut control, 100, "s-1").await;
    let mut screen = timeout(WAIT, conn.accept_stream()).await.unwrap().unwrap();
    assert!(matches!(read(&mut screen).await, Frame::ScreenOpen { .. }));
    assert!(matches!(
        read(&mut screen).await,
        Frame::Screen {
            frame: ScreenFrame::Snapshot(_)
        }
    ));

    let busy = host.term.clone();
    let spinner = tokio::spawn(async move {
        for n in 0u64.. {
            feed(&busy, format!("\x1b[{LINES};1H{n}").as_bytes());
            sleep(Duration::from_millis(2)).await;
        }
    });
    let PeerStream { mut send, mut recv } = screen;
    let frames = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&frames);
    let reader = tokio::spawn(async move {
        while let Ok(frame) = read_frame_within(&mut recv, MAX_PEER_FRAME).await {
            if matches!(frame, Frame::Screen { .. }) {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let start = std::time::Instant::now();
    for _ in 0..keys {
        sleep(window / keys).await;
        let key = Frame::Input {
            text: "x".into(),
            paste: false,
        };
        write_frame(&mut send, &key).await.unwrap();
    }
    let elapsed = start.elapsed();
    let count = frames.load(Ordering::SeqCst);
    spinner.abort();
    reader.abort();
    (count, elapsed)
}

/// What the 33 ms tick alone may send in `elapsed`: its first tick fires at
/// once, then one per interval.
fn ticks_in(elapsed: Duration) -> usize {
    (elapsed.as_millis() / MIN_FRAME_INTERVAL.as_millis()) as usize + 1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_adds_at_most_one_frame_per_key_to_the_frame_rate_cap() {
    let keys = 10;
    let (frames, elapsed) =
        frames_while_the_screen_is_busy(CONTROL, keys, Duration::from_millis(600)).await;
    let ticks = ticks_in(elapsed);
    assert!(
        frames <= ticks + keys as usize,
        "{frames} frames in {elapsed:?}: over {ticks} ticks + {keys} echoes"
    );
    // And the screen really was busy, so the cap was under test.
    assert!(frames >= ticks / 2, "{frames} frames for {ticks} ticks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_input_adds_no_frames() {
    // A `view` peer's keys are dropped: nothing was written, so no echo.
    let (frames, elapsed) =
        frames_while_the_screen_is_busy(VIEW, 10, Duration::from_millis(600)).await;
    let ticks = ticks_in(elapsed);
    assert!(
        frames <= ticks,
        "{frames} frames in {elapsed:?}: over {ticks} ticks"
    );
}

/// The DEV-1242 A/B harness: keystroke → echo visible in the client's
/// mirrored grid, over MemNet with an instantly-echoing PTY, so the number is
/// Allele's own pipeline (input reader → PTY → frame loop → encode → wire →
/// apply), with no network or program time in it. Keys are spaced by a
/// pseudo-random 0–40 ms so they land at random phases of the host's tick.
///
/// A measurement, not a check: timing depends on the machine. Run with
/// `cargo test --release --bin Allele measure_keystroke_echo_latency -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; run with --ignored --nocapture"]
async fn measure_keystroke_echo_latency() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let (_laptop, conn, mut control) = host.peer(&net, CONTROL).await;
    let client = ScreenClient::new(conn);
    let attached = client.attach(&mut control.send, "s-1").await.unwrap();
    let grid = attached.grid();
    let text = |g: &crate::terminal::mirror::MirroredGrid| -> String {
        (0..LINES as i32)
            .filter_map(|r| {
                g.row(r)
                    .map(|row| row.iter().map(|c| c.c).collect::<String>())
            })
            .collect::<Vec<_>>()
            .join("")
            .replace(' ', "")
    };
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut samples = Vec::new();
    for i in 0..120usize {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        sleep(Duration::from_millis(seed % 41)).await;
        let before = text(&grid.lock()).len();
        let start = std::time::Instant::now();
        attached.send_input(b"x").unwrap();
        loop {
            if text(&grid.lock()).len() > before {
                break;
            }
            tokio::task::yield_now().await;
            if start.elapsed() > WAIT {
                panic!("no echo for key {i}");
            }
        }
        samples.push(start.elapsed());
        if i % 30 == 29 {
            // Keep the line from wrapping off an 8-row screen.
            attached.send_input(b"\r\n").unwrap();
            sleep(Duration::from_millis(80)).await;
        }
    }
    samples.sort();
    let pct = |p: usize| samples[samples.len() * p / 100];
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "keystroke echo n={} min {:?} p50 {:?} mean {:?} p90 {:?} p99 {:?} max {:?}",
        samples.len(),
        samples[0],
        pct(50),
        mean,
        pct(90),
        pct(99),
        samples[samples.len() - 1]
    );
}
