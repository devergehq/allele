//! A remote tab's screen end to end over the in-memory network (DEV-1167):
//! two real `RemoteService`s, the laptop following the desk, and
//! [`RemoteScreen`] attaching over the follower's control stream.
//!
//! | criterion | test |
//! | -- | -- |
//! | opening a remote session attaches and mirrors it | `opening_a_remote_session_mirrors_it` |
//! | losing the connection keeps the last frame | `a_dropped_host_keeps_the_last_frame_then_reattaches` |
//! | reconnecting reattaches without user action | `a_dropped_host_keeps_the_last_frame_then_reattaches` |
//! | closing the tab detaches, the host session keeps running | `dropping_the_screen_detaches_and_leaves_the_session_running` |
//! | a session the host cannot show says so | `an_unknown_session_is_shown_as_unavailable` |
//! | two tabs on one host share its control stream | `two_sessions_on_one_host_mirror_at_once` |
//! | typing in a tab reaches the host session (with control) | `typing_reaches_the_host_session_with_control` |
//! | without control, typing is refused and said so | `typing_without_control_is_refused_with_a_reason` |
//! | the tab reads the link's path live, relay → direct (DEV-1243) | `the_screen_reports_the_links_path_as_it_changes` |

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use tokio::time::timeout;

use super::follow_tests::{desk_and_laptop, Mac};
use super::testing::MemNet;
use super::transport::{PathInfo, PathKind, PeerConnection};
use super::viewer::{RemoteScreen, ScreenPhase};
use crate::status_feed::client::Reachability;
use crate::terminal::grid_source::GridView;
use crate::terminal::mirror::{MirrorTerm, SharedTerm};

const WAIT: Duration = Duration::from_secs(5);

type HostTerm = Arc<FairMutex<Term<VoidListener>>>;

fn term() -> HostTerm {
    let config = Config {
        scrolling_history: 100,
        ..Default::default()
    };
    Arc::new(FairMutex::new(Term::new(
        config,
        &TermSize::new(40, 6),
        VoidListener,
    )))
}

fn feed(term: &HostTerm, bytes: &[u8]) {
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut *term.lock(), bytes);
}

/// Serve `term` as `session_id` on `host`. The source keeps one handle; every
/// screen the host is streaming holds another.
fn serve(host: &Mac, session_id: &str, term: &HostTerm) {
    let handle: Arc<dyn MirrorTerm> = Arc::new(SharedTerm::new(term, Some(100)));
    host.screens
        .0
        .lock()
        .unwrap()
        .insert(session_id.into(), handle);
}

fn handles(host: &Mac, session_id: &str) -> usize {
    Arc::strong_count(host.screens.0.lock().unwrap().get(session_id).unwrap())
}

fn line_text(screen: &RemoteScreen, line: i32) -> String {
    let Some(grid) = screen.grid() else {
        return String::new();
    };
    let grid = grid.lock();
    let text: String = (0..grid.columns()).map(|c| grid.cell(line, c).c).collect();
    text.trim_end().to_string()
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let waited = timeout(WAIT, async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

#[tokio::test]
async fn opening_a_remote_session_mirrors_it() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    let term = term();
    feed(&term, b"hello from the desk");
    serve(&desk, "s-1", &term);

    let screen = laptop.svc.open_screen(desk.id, "s-1");
    until("a live screen", || screen.phase() == ScreenPhase::Live).await;
    until("the host's text", || {
        line_text(&screen, 0) == "hello from the desk"
    })
    .await;

    // Host size is authoritative: the client grid is the host's grid.
    let grid = screen.grid().unwrap();
    assert_eq!(grid.lock().columns(), 40);
    assert_eq!(grid.lock().screen_lines(), 6);

    feed(&term, b"\r\nsecond line");
    until("a later change", || line_text(&screen, 1) == "second line").await;
}

#[tokio::test]
async fn the_screen_reports_the_links_path_as_it_changes() {
    let net = MemNet::new();
    let relayed = PathInfo {
        kind: PathKind::Relay,
        rtt: Duration::from_millis(900),
    };
    net.set_path(Some(relayed));
    let (desk, laptop) = desk_and_laptop(&net).await;
    let term = term();
    serve(&desk, "s-1", &term);

    let screen = laptop.svc.open_screen(desk.id, "s-1");
    until("a live screen", || screen.phase() == ScreenPhase::Live).await;
    assert_eq!(screen.path(), Some(relayed), "every connect starts relayed");

    // Hole-punching succeeds: same connection, new path. No reconnect, no
    // frame, no tick — the indicator must read it anyway.
    let direct = PathInfo {
        kind: PathKind::Direct,
        rtt: Duration::from_millis(4),
    };
    net.set_path(Some(direct));
    assert_eq!(screen.path(), Some(direct));
    assert_eq!(screen.path().unwrap().to_string(), "direct · 4 ms");
}

#[tokio::test]
async fn a_dropped_host_keeps_the_last_frame_then_reattaches() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    let term = term();
    feed(&term, b"before the drop");
    serve(&desk, "s-1", &term);
    let screen = laptop.svc.open_screen(desk.id, "s-1");
    until("the first frame", || {
        line_text(&screen, 0) == "before the drop"
    })
    .await;

    for conn in desk.accepted() {
        conn.close("host went to sleep");
    }
    until("the tab to notice", || screen.phase() != ScreenPhase::Live).await;
    assert_eq!(
        line_text(&screen, 0),
        "before the drop",
        "the last frame stays on screen while the host is away"
    );

    // The host changes while away; nobody touches the tab.
    feed(&term, b"\r\nafter the drop");
    until("a reattach", || {
        screen.phase() == ScreenPhase::Live && line_text(&screen, 1) == "after the drop"
    })
    .await;
}

#[tokio::test]
async fn dropping_the_screen_detaches_and_leaves_the_session_running() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    let term = term();
    feed(&term, b"still here");
    serve(&desk, "s-1", &term);
    assert_eq!(handles(&desk, "s-1"), 1);

    let screen = laptop.svc.open_screen(desk.id, "s-1");
    until("a live screen", || screen.phase() == ScreenPhase::Live).await;
    assert!(
        handles(&desk, "s-1") >= 2,
        "the host's streamer holds the terminal"
    );

    drop(screen);
    until("the host to let go", || handles(&desk, "s-1") == 1).await;

    // The session is untouched and the follower's link is still up.
    feed(&term, b"\r\nand running");
    assert_eq!(
        Arc::strong_count(&term),
        1,
        "only the host holds its terminal"
    );
    let h = laptop.svc.hosts().get(desk.id).unwrap();
    assert_eq!(h.reachability(), Reachability::Connected);
}

#[tokio::test]
async fn an_unknown_session_is_shown_as_unavailable() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;

    let screen = laptop.svc.open_screen(desk.id, "gone");
    until("unavailable", || {
        matches!(screen.phase(), ScreenPhase::Unavailable(_))
    })
    .await;
    assert!(screen.grid().is_none());
}

#[tokio::test]
async fn two_sessions_on_one_host_mirror_at_once() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    let (a, b) = (term(), term());
    feed(&a, b"left");
    feed(&b, b"right");
    serve(&desk, "a", &a);
    serve(&desk, "b", &b);

    let left = laptop.svc.open_screen(desk.id, "a");
    let right = laptop.svc.open_screen(desk.id, "b");
    until("both", || {
        line_text(&left, 0) == "left" && line_text(&right, 0) == "right"
    })
    .await;
}

/// Serve `term` with input enabled; returns what was typed into it.
fn serve_typed(host: &Mac, session_id: &str, term: &HostTerm) -> Arc<std::sync::Mutex<Vec<u8>>> {
    let typed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_typed = Arc::clone(&typed);
    let handle: Arc<dyn MirrorTerm> = Arc::new(SharedTerm::new(term, Some(100)).with_input(
        Box::new(move |bytes: &[u8]| {
            sink_typed.lock().unwrap().extend_from_slice(bytes);
            true
        }),
    ));
    host.screens
        .0
        .lock()
        .unwrap()
        .insert(session_id.into(), handle);
    typed
}

#[tokio::test]
async fn typing_reaches_the_host_session_with_control() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.svc.set_control(laptop.id, true).unwrap();
    let term = term();
    let typed = serve_typed(&desk, "s-1", &term);

    let screen = laptop.svc.open_screen(desk.id, "s-1");
    until("a live screen", || screen.phase() == ScreenPhase::Live).await;
    screen.send_input(b"ls\r").unwrap();
    until("the keys on the host", || {
        typed.lock().unwrap().as_slice() == b"ls\r"
    })
    .await;
}

#[tokio::test]
async fn typing_without_control_is_refused_with_a_reason() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    let term = term();
    let typed = serve_typed(&desk, "s-1", &term);

    let screen = laptop.svc.open_screen(desk.id, "s-1");
    until("a live screen", || screen.phase() == ScreenPhase::Live).await;
    screen.send_input(b"y\r").unwrap();
    let mut refusal = None;
    until("a refusal", || {
        refusal = refusal.take().or_else(|| screen.take_refusal());
        refusal.is_some()
    })
    .await;
    assert!(typed.lock().unwrap().is_empty(), "nothing reached the PTY");
    assert!(
        screen.take_refusal().is_none(),
        "a refusal is reported once"
    );
}
