//! The allele control socket (DEV-415).
//!
//! A Unix domain socket at `~/.allele/control.sock`, mode 0600, speaking the
//! newline-delimited JSON in [`super::protocol`].
//!
//! ## Why a socket rather than a spool
//!
//! Allele already ingests Claude hook events by polling `~/.allele/events/`,
//! and reusing that would have needed no new listener. It was rejected for one
//! reason: **a write to a spool always succeeds.** A caller cannot distinguish
//! "allele is busy" from "allele is not running" except by timing out, and the
//! contract here is a legible error rather than a hang. `connect()` to a socket
//! that is not there fails immediately, which is why
//! [`ErrorCode::AlleleNotRunning`](super::protocol::ErrorCode::AlleleNotRunning)
//! exists as a code allele itself can never send.
//!
//! ## Threading
//!
//! `UnixListener` is blocking, so accept and per-connection reads run on a
//! dedicated OS thread. Each parsed request is handed to the GPUI foreground
//! thread over a channel, carrying a reply sender the socket thread blocks on.
//! Foreground work needs a `&mut Window` — session creation does — which is
//! what [`super::update_in_main_window`] exists to provide.
//!
//! Note the drain is timer-based, like the hook poller. That is *not* in
//! tension with rejecting the spool: the spool's fatal property was the
//! ambiguity of a write that always succeeds, not its latency. Here the
//! connection either exists or is refused, and the drain interval only bounds
//! how long an accepted request waits.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use tracing::{info, warn};

use super::ext;
use super::hub::Hub;
use super::protocol::{ErrorCode, Request, Response};

/// How long the socket thread waits for the foreground thread to answer.
///
/// Generous, because a legitimate `sessions.create` clones a workspace and
/// resolves a branch. The timeout exists so a wedged or shutting-down app
/// produces an error rather than a socket that never replies.
pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// A request for the foreground drain, with the channel its answer goes back
/// on. The socket and the remote listener (DEV-1161) both produce these, so
/// both are answered by one handler and counted against one admission cap.
pub struct ControlRequest {
    pub request: Request,
    pub reply: Sender<Response>,
}

/// A remote screen stream (DEV-1165) asking the foreground drain for a
/// session's terminal. The answer is a weak handle, so a mirror never keeps a
/// closed session's terminal alive. It is the session's own terminal, never
/// a drawer shell. `None` when there is no such session or it has no terminal.
pub struct ScreenLookup {
    pub session_id: String,
    pub reply: Sender<Option<std::sync::Arc<dyn crate::terminal::mirror::MirrorTerm>>>,
}

/// A paired machine asking to resize a session's terminal to its window
/// (DEV-1167). `true` when the session had a terminal to resize. The host's
/// next local interaction with the session restores its own size.
pub struct ScreenResize {
    pub session_id: String,
    pub columns: u16,
    pub lines: u16,
    pub reply: Sender<bool>,
}

/// File name of the control socket under `<root>/.allele`.
const SOCKET_NAME: &str = "control.sock";

pub fn socket_path() -> Option<PathBuf> {
    Some(control_socket_in(&crate::paths::root()?))
}

/// The control socket an explicit root would serve.
///
/// [`socket_path`] is defined in terms of it so the two cannot drift, and a
/// test can ask which socket a given launch picks without fixing the
/// process-global root (DEV-943).
pub(crate) fn control_socket_in(root: &Path) -> PathBuf {
    crate::paths::allele_dir_in(root).join(SOCKET_NAME)
}

/// Bind the control socket and start accepting, forwarding each request on
/// `tx` to the foreground drain.
///
/// `false` if the socket could not be bound — in which case local dispatch is
/// unavailable and the app carries on normally. A session manager that refuses
/// to launch because an optional automation surface is missing would be a
/// worse trade.
pub fn spawn_listener(tx: Sender<ControlRequest>) -> bool {
    socket_path().is_some_and(|path| spawn_listener_at(path, tx))
}

/// [`spawn_listener`] against an explicit path.
///
/// Split out so tests can bind a temporary socket. A test that bound the real
/// `~/.allele/control.sock` would fight the user's running Allele for it.
pub fn spawn_listener_at(path: PathBuf, tx: Sender<ControlRequest>) -> bool {
    spawn_listener_with(path, tx, Hub::global())
}

/// [`spawn_listener_at`] with its own `allele.ext/1` hub, so a test's
/// subscribers and epoch are not shared with every other test in the binary.
pub(crate) fn spawn_listener_with(
    path: PathBuf,
    tx: Sender<ControlRequest>,
    hub: &'static Hub,
) -> bool {
    // A socket file left by a previous run would make `bind` fail with
    // EADDRINUSE even though nothing is listening. Only remove it when
    // nothing answers, so two running Alleles cannot silently steal the
    // socket from one another.
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            warn!(
                "control socket {} is already served by another Allele; \
                 dispatch disabled in this instance",
                path.display()
            );
            return false;
        }
        let _ = std::fs::remove_file(&path);
    }

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            warn!("could not bind control socket {}: {e}", path.display());
            return false;
        }
    };

    // Owner-only. The socket can start sessions with the user's full
    // privileges, so it must not be reachable by other accounts on the box.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            warn!("could not chmod control socket to 0600: {e} — refusing to serve it");
            let _ = std::fs::remove_file(&path);
            return false;
        }
    }

    if std::thread::Builder::new()
        .name("allele-control".into())
        .spawn(move || accept_loop(listener, tx, hub))
        .is_err()
    {
        return false;
    }

    info!("control socket listening on {}", path.display());
    true
}

fn accept_loop(listener: UnixListener, tx: Sender<ControlRequest>, hub: &'static Hub) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // One thread per connection. Connections are few (an
                // orchestrator or two) and long-lived, and this keeps a slow
                // client from blocking every other caller's accept.
                let tx = tx.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("allele-control-conn".into())
                    .spawn(move || serve(stream, tx, hub))
                {
                    warn!("control socket: could not spawn connection thread: {e}");
                }
            }
            Err(e) => {
                warn!("control socket accept failed: {e}");
                return;
            }
        }
    }
}

fn serve(stream: UnixStream, tx: Sender<ControlRequest>, hub: &'static Hub) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut out = write_half;
    let reader = BufReader::new(stream);
    // `Some` once the client has said `hello` (DEV-1254). Until then every
    // line takes the legacy path below, unchanged, byte for byte.
    let mut ext: Option<ext::Connection> = None;

    let reply = |out: &mut UnixStream, mut line: String| -> bool {
        line.push('\n');
        out.write_all(line.as_bytes()).is_ok() && out.flush().is_ok()
    };

    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }

        if let Some(conn) = &ext {
            match ext::handle(conn, &line, &tx, hub) {
                ext::Outcome::Reply(encoded) => {
                    if !reply(&mut out, encoded) {
                        return;
                    }
                }
                ext::Outcome::Stream(subscription) => {
                    // From here the connection only carries frames out. A
                    // reader that stops reading must not hold this thread
                    // forever, so writes are bounded.
                    let _ = out.set_write_timeout(Some(ext::STREAM_WRITE_TIMEOUT));
                    ext::stream(subscription, &mut out);
                    return;
                }
            }
            continue;
        }
        if ext::is_hello(&line) {
            let encoded = match ext::hello(&line, hub) {
                Ok((conn, encoded)) => {
                    ext = Some(conn);
                    encoded
                }
                Err(encoded) => encoded,
            };
            if !reply(&mut out, encoded) {
                return;
            }
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => forward(request, &tx),
            Err(e) => err(
                ErrorCode::BadRequest,
                format!("could not parse request: {e}"),
            ),
        };

        let Ok(mut encoded) = serde_json::to_string(&response) else {
            return;
        };
        encoded.push('\n');
        if out.write_all(encoded.as_bytes()).is_err() || out.flush().is_err() {
            return; // client hung up
        }
    }
}

/// Hand a request to the foreground thread and block for its answer.
pub(crate) fn forward(request: Request, tx: &Sender<ControlRequest>) -> Response {
    let (reply_tx, reply_rx) = channel::<Response>();
    if tx
        .send(ControlRequest {
            request,
            reply: reply_tx,
        })
        .is_err()
    {
        return err(ErrorCode::Internal, "Allele is shutting down".to_string());
    }
    match reply_rx.recv_timeout(REPLY_TIMEOUT) {
        Ok(response) => response,
        Err(_) => err(
            ErrorCode::Internal,
            "Allele did not answer in time".to_string(),
        ),
    }
}

fn err(code: ErrorCode, message: String) -> Response {
    Response::Error { code, message }
}

/// Remove the socket file on shutdown so the next run binds cleanly.
pub fn cleanup() {
    if let Some(path) = socket_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;

    /// Bind a listener on a temp path and answer requests with `responder`.
    fn serve_with(
        dir: &tempfile::TempDir,
        responder: impl Fn(Request) -> Response + Send + 'static,
    ) -> PathBuf {
        let path = dir.path().join("control.sock");
        let (tx, rx) = channel();
        assert!(spawn_listener_at(path.clone(), tx), "binds");
        std::thread::spawn(move || {
            while let Ok(req) = rx.recv() {
                let _ = req.reply.send(responder(req.request));
            }
        });
        path
    }

    fn call(path: &PathBuf, line: &str) -> Response {
        let stream = UnixStream::connect(path).expect("connects");
        let mut out = BufWriter::new(stream.try_clone().expect("clone"));
        out.write_all(line.as_bytes()).expect("write");
        out.write_all(b"\n").expect("write");
        out.flush().expect("flush");
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).expect("read");
        serde_json::from_str(&reply).expect("parses response")
    }

    #[test]
    fn a_request_gets_its_answer_back_over_the_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |_| Response::Projects { projects: vec![] });
        assert_eq!(
            call(&path, r#"{"op":"projects_list"}"#),
            Response::Projects { projects: vec![] }
        );
    }

    /// Two calls on one connection: the framing has to survive being reused,
    /// which is the whole point of line-delimiting rather than one-shot.
    #[test]
    fn one_connection_serves_many_requests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |_| Response::Sessions { sessions: vec![] });
        let stream = UnixStream::connect(&path).expect("connects");
        let mut out = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        for _ in 0..3 {
            out.write_all(b"{\"op\":\"sessions_list\"}\n")
                .expect("write");
            let mut reply = String::new();
            reader.read_line(&mut reply).expect("read");
            assert!(reply.contains("sessions"), "got {reply}");
        }
    }

    /// Garbage must come back as a typed error rather than closing the
    /// connection — a caller that gets silence cannot tell it from a hang.
    #[test]
    fn malformed_json_is_a_typed_error_not_a_dropped_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |_| Response::Sessions { sessions: vec![] });
        match call(&path, "{not json") {
            Response::Error { code, .. } => assert_eq!(code, ErrorCode::BadRequest),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    /// Raw reply bytes for one line on a fresh connection.
    fn raw_call(path: &PathBuf, line: &str) -> String {
        let stream = UnixStream::connect(path).expect("connects");
        let mut out = stream.try_clone().expect("clone");
        out.write_all(line.as_bytes()).expect("write");
        out.write_all(b"\n").expect("write");
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).expect("read");
        reply
    }

    /// A client that never says `hello` is the legacy tier (DEV-1254), and it
    /// must get byte-for-byte what it got before `allele.ext/1` existed. The
    /// literals were captured on master before ext/1 landed: an ext op sent
    /// without `hello` is still an unknown variant. The list of ops in that
    /// message grows when a legacy op is added — DEV-1267's three decision ops
    /// — and nothing else in it changes.
    #[test]
    fn a_client_without_hello_gets_the_legacy_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |request| match request {
            Request::ProjectsList => Response::Projects { projects: vec![] },
            Request::SessionsStatus { session_id } => {
                super::super::protocol::no_such_session(&session_id)
            }
            _ => Response::Sessions { sessions: vec![] },
        });
        for (line, want) in [
            (r#"{"op":"projects_list"}"#, "{\"result\":\"projects\",\"projects\":[]}\n"),
            (r#"{"op":"sessions_list"}"#, "{\"result\":\"sessions\",\"sessions\":[]}\n"),
            (
                r#"{"op":"sessions_status","session_id":"x"}"#,
                "{\"result\":\"error\",\"code\":\"session_not_found\",\"message\":\"no session with id x\"}\n",
            ),
            (
                r#"{"op":"subscribe","topics":["sessions"]}"#,
                "{\"result\":\"error\",\"code\":\"bad_request\",\"message\":\"could not parse request: unknown variant `subscribe`, expected one of `projects_list`, `sessions_list`, `sessions_status`, `sessions_create`, `sessions_discard`, `sessions_interrupt`, `decisions_post`, `decisions_list`, `decisions_withdraw` at line 1 column 17\"}\n",
            ),
            (
                "{not json",
                "{\"result\":\"error\",\"code\":\"bad_request\",\"message\":\"could not parse request: key must be a string at line 1 column 2\"}\n",
            ),
        ] {
            assert_eq!(raw_call(&path, line), want, "for {line}");
        }
    }

    /// The socket can start sessions with the user's full privileges, so it
    /// must not be reachable by other accounts on the machine.
    #[test]
    fn socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |_| Response::Sessions { sessions: vec![] });
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// A socket file left by a crashed run must not block the next one.
    #[test]
    fn a_stale_socket_file_is_reclaimed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("control.sock");
        std::fs::write(&path, b"stale").expect("write stale file");
        assert!(
            spawn_listener_at(path.clone(), channel().0),
            "should reclaim"
        );
    }

    /// But a socket something is still answering on must not be stolen —
    /// otherwise a second Allele silently takes dispatch from the first.
    #[test]
    fn a_live_socket_is_not_stolen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |_| Response::Sessions { sessions: vec![] });
        assert!(
            !spawn_listener_at(path, channel().0),
            "second listener must refuse a socket that is already answering"
        );
    }
}
