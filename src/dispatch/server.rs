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
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use tracing::{info, warn};

use super::protocol::{ErrorCode, Request, Response};

/// How long the socket thread waits for the foreground thread to answer.
///
/// Generous, because a legitimate `sessions.create` clones a workspace and
/// resolves a branch. The timeout exists so a wedged or shutting-down app
/// produces an error rather than a socket that never replies.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// A request from the socket, with the channel its answer goes back on.
pub struct ControlRequest {
    pub request: Request,
    pub reply: Sender<Response>,
}

pub fn socket_path() -> Option<PathBuf> {
    Some(crate::hooks::base_dir()?.join("control.sock"))
}

/// Bind the control socket and start accepting.
///
/// Returns the receiver the foreground drain reads from, or `None` if the
/// socket could not be bound — in which case dispatch is unavailable and the
/// app carries on normally. A session manager that refuses to launch because
/// an optional automation surface is missing would be a worse trade.
pub fn spawn_listener() -> Option<Receiver<ControlRequest>> {
    spawn_listener_at(socket_path()?)
}

/// [`spawn_listener`] against an explicit path.
///
/// Split out so tests can bind a temporary socket. A test that bound the real
/// `~/.allele/control.sock` would fight the user's running Allele for it.
pub fn spawn_listener_at(path: PathBuf) -> Option<Receiver<ControlRequest>> {
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
            return None;
        }
        let _ = std::fs::remove_file(&path);
    }

    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            warn!("could not bind control socket {}: {e}", path.display());
            return None;
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
            return None;
        }
    }

    let (tx, rx) = channel::<ControlRequest>();
    std::thread::Builder::new()
        .name("allele-control".into())
        .spawn(move || accept_loop(listener, tx))
        .ok()?;

    info!("control socket listening on {}", path.display());
    Some(rx)
}

fn accept_loop(listener: UnixListener, tx: Sender<ControlRequest>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // One thread per connection. Connections are few (an
                // orchestrator or two) and long-lived, and this keeps a slow
                // client from blocking every other caller's accept.
                let tx = tx.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("allele-control-conn".into())
                    .spawn(move || serve(stream, tx))
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

fn serve(stream: UnixStream, tx: Sender<ControlRequest>) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut out = write_half;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
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
fn forward(request: Request, tx: &Sender<ControlRequest>) -> Response {
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
        let rx = spawn_listener_at(path.clone()).expect("binds");
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
        assert!(spawn_listener_at(path.clone()).is_some(), "should reclaim");
    }

    /// But a socket something is still answering on must not be stolen —
    /// otherwise a second Allele silently takes dispatch from the first.
    #[test]
    fn a_live_socket_is_not_stolen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = serve_with(&dir, |_| Response::Sessions { sessions: vec![] });
        assert!(
            spawn_listener_at(path).is_none(),
            "second listener must refuse a socket that is already answering"
        );
    }
}
