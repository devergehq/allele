//! `allele.ext/1` on the control socket: `hello`, capabilities, and the
//! `subscribe{sessions}` stream (DEV-1254).
//!
//! The wire types live in the `allele-contract` crate; this module is the
//! server side of them. Everything before [`view`] runs on a connection's
//! socket thread. [`view`] runs on the foreground and is the only place
//! Allele's internal session types meet the contract's.
//!
//! ## The legacy tier is untouched
//!
//! A connection becomes `ext/1` only by sending `hello` as its first op. Until
//! then [`super::server`] parses every line exactly as it always has, so a
//! client that predates this module gets the same bytes back, error messages
//! included. That is also why the new ops are not variants of
//! [`super::protocol::Request`]: adding one would change the "expected one of"
//! list in the parse error every legacy client can receive.
//!
//! ## What a capability is
//!
//! A claim-scoped switch, not a security boundary. Every client runs as the
//! machine's user on a 0600 socket and `client.id` is whatever it says.
//! Capabilities exist so a desk that only meant to watch cannot start
//! sessions by accident, and so the record says who did what.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use allele_contract::{
    self as contract, Capability, ErrorReply, Frame, Hello, HelloResult, Reply, SessionEntry,
    Subscribe, Topic, BUILTIN_MCP_CLIENT_ID, CONTRACT,
};
use serde::Deserialize;

use super::hub::{Hub, Subscription, View};
use super::protocol::{ErrorCode, Request, Response};
use super::server::{forward, ControlRequest};

/// How long a socket thread waits for the foreground to answer a query.
/// The drain runs every 20 ms; this only bounds a wedged or exiting app.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one write to a subscriber may block. A client that has stopped
/// reading fills its socket buffer and then this times the writer out, so a
/// dead reader holds a thread for seconds, not forever.
pub(crate) const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// One `ext/1` connection's standing, after its `hello`.
#[derive(Debug, Clone)]
pub(crate) struct Connection {
    granted: Vec<Capability>,
}

impl Connection {
    fn has(&self, capability: Capability) -> bool {
        self.granted.contains(&capability)
    }
}

/// What a line on an `ext/1` connection produced.
pub(crate) enum Outcome {
    /// One reply line.
    Reply(String),
    /// The connection is now a stream; hand it to [`stream`].
    Stream(Subscription),
}

#[derive(Deserialize)]
struct OpProbe {
    #[serde(default)]
    op: Option<String>,
}

fn op_of(line: &str) -> Option<String> {
    serde_json::from_str::<OpProbe>(line)
        .ok()
        .and_then(|p| p.op)
}

/// Whether `line` is a `hello`. Anything else on a fresh connection takes the
/// legacy path.
pub(crate) fn is_hello(line: &str) -> bool {
    op_of(line).as_deref() == Some("hello")
}

/// What a client id may be granted. No manifests exist yet, so this is
/// `observe` for everyone, plus `dispatch` for Allele's own MCP server.
fn grantable(client_id: &str) -> &'static [Capability] {
    if client_id == BUILTIN_MCP_CLIENT_ID {
        &[Capability::Observe, Capability::Dispatch]
    } else {
        &[Capability::Observe]
    }
}

/// Answer a `hello`. `Ok` upgrades the connection; `Err` is the reply line and
/// leaves it in the legacy tier.
pub(crate) fn hello(line: &str, hub: &Hub) -> Result<(Connection, String), String> {
    let hello = match serde_json::from_str::<contract::Request>(line) {
        Ok(contract::Request::Hello(hello)) => hello,
        Ok(_) => {
            return Err(error_line(
                contract::ErrorCode::BadRequest,
                "expected hello",
            ))
        }
        Err(e) => {
            return Err(error_line(
                contract::ErrorCode::BadRequest,
                &format!("could not parse hello: {e}"),
            ))
        }
    };
    let Hello {
        contract: asked,
        client,
        capabilities,
    } = hello;
    if !contract::is_supported(&asked) {
        return Err(error_line(
            contract::ErrorCode::UnsupportedContract,
            &format!("this Allele serves {CONTRACT}; the client asked for {asked:?}"),
        ));
    }

    let allowed = grantable(&client.id);
    let mut granted: Vec<Capability> = Vec::new();
    for capability in capabilities {
        if allowed.contains(&capability) && !granted.contains(&capability) {
            granted.push(capability);
        }
    }

    let view = query(hub).map_err(|e| error_line(contract::ErrorCode::Internal, &e))?;
    let reply = Reply::Hello(HelloResult {
        contract_versions: vec![CONTRACT.to_string()],
        epoch: hub.epoch().to_string(),
        granted: granted.clone(),
        dispatch_limits: view.limits,
    });
    Ok((Connection { granted }, encode(&reply)))
}

/// Answer one line on an upgraded connection.
pub(crate) fn handle(
    conn: &Connection,
    line: &str,
    tx: &Sender<ControlRequest>,
    hub: &Hub,
) -> Outcome {
    match op_of(line).as_deref() {
        Some("hello") => {
            return Outcome::Reply(error_line(
                contract::ErrorCode::BadRequest,
                "this connection has already said hello",
            ))
        }
        Some("subscribe") => return subscribe(conn, line, hub),
        _ => {}
    }

    // Everything else is a legacy op, parsed and refused exactly as the
    // legacy tier would, then gated by capability.
    let request = match serde_json::from_str::<Request>(line) {
        Ok(request) => request,
        Err(e) => {
            return Outcome::Reply(legacy_line(&Response::Error {
                code: ErrorCode::BadRequest,
                message: format!("could not parse request: {e}"),
            }))
        }
    };
    let needs = match request {
        Request::ProjectsList | Request::SessionsList | Request::SessionsStatus { .. } => {
            Capability::Observe
        }
        Request::SessionsCreate(_)
        | Request::SessionsDiscard { .. }
        | Request::SessionsInterrupt { .. } => Capability::Dispatch,
    };
    if !conn.has(needs) {
        return Outcome::Reply(not_granted(needs));
    }

    Outcome::Reply(match request {
        Request::SessionsList => match query(hub) {
            Ok(view) => encode(&Reply::Sessions {
                sessions: view.sessions.clone(),
            }),
            Err(e) => error_line(contract::ErrorCode::Internal, &e),
        },
        Request::SessionsStatus { session_id } => match query(hub) {
            Ok(view) => status_of(&view, &session_id),
            Err(e) => error_line(contract::ErrorCode::Internal, &e),
        },
        other => legacy_line(&forward(other, tx)),
    })
}

/// `sessions_status` after `hello`: live first, then archived, and
/// `session_not_found` only for an id Allele never had.
fn status_of(view: &View, session_id: &str) -> String {
    view.sessions
        .iter()
        .chain(&view.archived)
        .find(|e| e.session_id == session_id)
        .map(|e| encode(&Reply::Status { session: e.clone() }))
        .unwrap_or_else(|| legacy_line(&super::protocol::no_such_session(session_id)))
}

fn subscribe(conn: &Connection, line: &str, hub: &Hub) -> Outcome {
    if !conn.has(Capability::Observe) {
        return Outcome::Reply(not_granted(Capability::Observe));
    }
    let Subscribe { topics, filter } = match serde_json::from_str::<contract::Request>(line) {
        Ok(contract::Request::Subscribe(s)) => s,
        Ok(_) => {
            return Outcome::Reply(error_line(
                contract::ErrorCode::BadRequest,
                "expected subscribe",
            ))
        }
        Err(e) => {
            return Outcome::Reply(error_line(
                contract::ErrorCode::BadRequest,
                &format!("could not parse subscribe: {e}"),
            ))
        }
    };
    if topics.is_empty() {
        return Outcome::Reply(error_line(
            contract::ErrorCode::BadRequest,
            "subscribe needs at least one topic",
        ));
    }
    if topics.contains(&Topic::Activity) {
        return Outcome::Reply(not_granted(Capability::Activity));
    }
    if let Some(other) = topics.iter().find(|t| **t != Topic::Sessions) {
        return Outcome::Reply(error_line(
            contract::ErrorCode::BadRequest,
            &format!("topic {other:?} is not served by this Allele; only `sessions` is"),
        ));
    }
    Outcome::Stream(hub.subscribe(filter))
}

/// Write a subscription's frames until it lags or the client goes away.
///
/// Runs on the connection's own thread, which is the only thing a slow client
/// can hold up. The `lagged` flag is checked before every write, so a dropped
/// subscriber says so instead of draining a backlog its client will discard.
pub(crate) fn stream(sub: Subscription, out: &mut impl Write) {
    let write = |out: &mut dyn Write, line: &str| -> bool {
        out.write_all(line.as_bytes()).is_ok()
            && out.write_all(b"\n").is_ok()
            && out.flush().is_ok()
    };
    let lagged = || encode(&Frame::Lagged);
    loop {
        if sub.lagged.load(Ordering::SeqCst) {
            write(out, &lagged());
            return;
        }
        match sub.frames.recv_timeout(Duration::from_secs(1)) {
            Ok(line) => {
                if sub.lagged.load(Ordering::SeqCst) {
                    write(out, &lagged());
                    return;
                }
                if !write(out, &line) {
                    return; // client hung up or stopped reading
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if sub.lagged.load(Ordering::SeqCst) {
                    write(out, &lagged());
                }
                return;
            }
        }
    }
}

fn query(hub: &Hub) -> Result<Arc<View>, String> {
    hub.query()
        .recv_timeout(QUERY_TIMEOUT)
        .map_err(|_| "Allele did not answer in time".to_string())
}

fn not_granted(capability: Capability) -> String {
    let name = serde_json::to_string(&capability).unwrap_or_default();
    error_line(
        contract::ErrorCode::CapabilityNotGranted,
        &format!("this op needs the {name} capability, which this connection was not granted"),
    )
}

fn error_line(code: contract::ErrorCode, message: &str) -> String {
    encode(&Reply::Error(ErrorReply {
        code,
        message: message.to_string(),
    }))
}

fn encode(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| {
        format!(r#"{{"result":"error","code":"internal","message":"could not encode reply: {e}"}}"#)
    })
}

fn legacy_line(response: &Response) -> String {
    encode(response)
}

// ── The foreground half ─────────────────────────────────────────────────

/// Build the hub's view of every session. Foreground only.
pub(crate) fn view(
    state: &crate::app_state::AppState,
    cx: &gpui::App,
    with_archives: bool,
) -> View {
    let mut view = View {
        limits: contract::DispatchLimits {
            max_depth: state.user_settings.dispatch.max_depth,
            max_sessions: state.user_settings.dispatch.max_sessions as u64,
            live_dispatched: super::admission::live_dispatched_count(state) as u64,
        },
        ..View::default()
    };
    for project in &state.projects {
        for session in &project.sessions {
            view.sessions.push(entry(session, &project.name, cx));
        }
        for archived in &project.archives {
            view.archived_ids.insert(archived.id.clone());
            if with_archives {
                view.archived.push(archived_entry(archived, &project.name));
            }
        }
    }
    view
}

/// One live session as the contract sees it.
pub(crate) fn entry(
    session: &crate::session::Session,
    project: &str,
    cx: &gpui::App,
) -> SessionEntry {
    use crate::session::SessionOrigin;
    let (parent, dispatched_by) = match &session.origin {
        SessionOrigin::Human => (None, None),
        SessionOrigin::Dispatched {
            by_session_id,
            by_label,
            ..
        } => (
            (!by_session_id.is_empty()).then(|| by_session_id.clone()),
            Some(by_label.clone()),
        ),
    };
    SessionEntry {
        session_id: session.id.clone(),
        name: session.label.clone(),
        project: project.to_string(),
        group: session.group.clone(),
        state: state_of(session.status),
        input: input_of(session.status, session.terminal_view.is_some()),
        state_age_secs: session
            .status_since
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or(0),
        turn: session.turn,
        dispatched: session.origin.is_dispatched(),
        depth: session.origin.depth(),
        parent,
        dispatched_by,
        reply_to: super::address::for_session(session, cx),
        started_at: super::hub::unix_secs(session.started_at),
        archived_at: None,
        archive_ref: None,
    }
}

fn archived_entry(archived: &crate::state::ArchivedSession, project: &str) -> SessionEntry {
    SessionEntry {
        session_id: archived.id.clone(),
        name: archived.label.clone(),
        project: project.to_string(),
        state: contract::State::Archived,
        input: contract::Input::NoProcess,
        archived_at: Some(archived.archived_at),
        archive_ref: Some(format!("refs/allele/archive/{}", archived.id)),
        ..SessionEntry::default()
    }
}

pub(crate) fn state_of(status: crate::session::SessionStatus) -> contract::State {
    use crate::session::SessionStatus as S;
    match status {
        S::Running => contract::State::Running,
        S::ResponseReady => contract::State::ResponseReady,
        S::AwaitingInput => contract::State::AwaitingInput,
        S::Idle => contract::State::Idle,
        S::Suspended => contract::State::Suspended,
        S::Done => contract::State::Done,
    }
}

/// Whether text sent now would reach the agent. Allele states the fact; how
/// long delivery takes is the client's expectation to own.
pub(crate) fn input_of(
    status: crate::session::SessionStatus,
    has_process: bool,
) -> contract::Input {
    use crate::session::SessionStatus as S;
    if !has_process {
        return contract::Input::NoProcess;
    }
    match status {
        S::AwaitingInput => contract::Input::BlockedOnPrompt,
        S::Running => contract::Input::DeliverableAtNextBoundary,
        S::Idle | S::ResponseReady => contract::Input::DeliverableNow,
        S::Suspended | S::Done => contract::Input::NoProcess,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    use allele_contract::{Event, State};

    /// A control socket on a temp path with its own hub, a stand-in for the
    /// foreground that publishes `view` whenever the hub wants one, and a
    /// legacy responder that answers every forwarded op with an empty list.
    struct Rig {
        _dir: tempfile::TempDir,
        path: PathBuf,
        hub: &'static Hub,
        view: Arc<Mutex<View>>,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("control.sock");
        let hub: &'static Hub = Box::leak(Box::new(Hub::new()));
        let (tx, rx) = std::sync::mpsc::channel::<ControlRequest>();
        assert!(super::super::server::spawn_listener_with(
            path.clone(),
            tx,
            hub
        ));
        std::thread::spawn(move || {
            while let Ok(req) = rx.recv() {
                let _ = req.reply.send(Response::Projects { projects: vec![] });
            }
        });
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        std::thread::spawn(move || loop {
            if hub.wants_view(Instant::now()).is_some() {
                let v = shared.lock().expect("view").clone();
                hub.publish(v, Instant::now());
            }
            std::thread::sleep(Duration::from_millis(5));
        });
        Rig {
            _dir: dir,
            path,
            hub,
            view,
        }
    }

    struct Client {
        out: UnixStream,
        reader: BufReader<UnixStream>,
    }

    impl Client {
        fn connect(rig: &Rig) -> Self {
            let stream = UnixStream::connect(&rig.path).expect("connects");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            Client {
                out: stream.try_clone().expect("clone"),
                reader: BufReader::new(stream),
            }
        }

        fn send(&mut self, line: &str) {
            self.out.write_all(line.as_bytes()).expect("write");
            self.out.write_all(b"\n").expect("write");
        }

        fn read(&mut self) -> serde_json::Value {
            let mut line = String::new();
            self.reader.read_line(&mut line).expect("read");
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
        }

        fn call(&mut self, line: &str) -> serde_json::Value {
            self.send(line);
            self.read()
        }

        fn hello(&mut self, id: &str, capabilities: &str) -> serde_json::Value {
            self.call(&format!(
                r#"{{"op":"hello","contract":"allele.ext/1","client":{{"id":"{id}","version":"1"}},"capabilities":{capabilities}}}"#
            ))
        }
    }

    fn entry(id: &str, group: Option<&str>) -> SessionEntry {
        SessionEntry {
            session_id: id.into(),
            name: id.into(),
            project: "p".into(),
            group: group.map(String::from),
            state: State::Idle,
            ..SessionEntry::default()
        }
    }

    /// AC: `hello` with an unknown major contract returns `unsupported_contract`.
    #[test]
    fn hello_with_an_unknown_major_is_unsupported() {
        let rig = rig();
        let mut c = Client::connect(&rig);
        let reply = c.call(r#"{"op":"hello","contract":"allele.ext/2","client":{"id":"x"}}"#);
        assert_eq!(reply["result"], "error");
        assert_eq!(reply["code"], "unsupported_contract");
        // And the connection is still the legacy tier.
        let legacy = c.call(r#"{"op":"subscribe","topics":["sessions"]}"#);
        assert_eq!(legacy["code"], "bad_request");
    }

    #[test]
    fn hello_reports_versions_epoch_grants_and_limits() {
        let rig = rig();
        rig.view.lock().expect("view").limits = contract::DispatchLimits {
            max_depth: 2,
            max_sessions: 7,
            live_dispatched: 3,
        };
        let mut c = Client::connect(&rig);
        let reply = c.hello("review-desk", r#"["observe","dispatch","badge"]"#);
        assert_eq!(reply["result"], "hello");
        assert_eq!(
            reply["contract_versions"],
            serde_json::json!(["allele.ext/1"])
        );
        assert_eq!(reply["epoch"], rig.hub.epoch());
        assert_eq!(
            reply["granted"],
            serde_json::json!(["observe"]),
            "a client with no manifest gets observe only"
        );
        assert_eq!(
            reply["dispatch_limits"],
            serde_json::json!({"max_depth":2,"max_sessions":7,"live_dispatched":3})
        );
    }

    #[test]
    fn the_builtin_mcp_server_may_dispatch() {
        let rig = rig();
        let mut c = Client::connect(&rig);
        let reply = c.hello(BUILTIN_MCP_CLIENT_ID, r#"["observe","dispatch"]"#);
        assert_eq!(reply["granted"], serde_json::json!(["observe", "dispatch"]));
        let projects = c.call(r#"{"op":"projects_list"}"#);
        assert_eq!(projects["result"], "projects", "legacy ops still work");
    }

    /// AC: a client without `observe` that subscribes with a filter gets
    /// `capability_not_granted`.
    #[test]
    fn subscribing_without_observe_is_not_granted() {
        let rig = rig();
        let mut c = Client::connect(&rig);
        assert_eq!(c.hello("nosy", "[]")["granted"], serde_json::json!([]));
        let reply =
            c.call(r#"{"op":"subscribe","topics":["sessions"],"filter":{"group":"review"}}"#);
        assert_eq!(reply["code"], "capability_not_granted");
        let list = c.call(r#"{"op":"sessions_list"}"#);
        assert_eq!(list["code"], "capability_not_granted");
    }

    #[test]
    fn dispatch_ops_need_dispatch() {
        let rig = rig();
        let mut c = Client::connect(&rig);
        c.hello("review-desk", r#"["observe"]"#);
        let reply = c.call(r#"{"op":"sessions_discard","session_id":"s"}"#);
        assert_eq!(reply["code"], "capability_not_granted");
    }

    #[test]
    fn activity_is_not_served_and_says_so() {
        let rig = rig();
        let mut c = Client::connect(&rig);
        c.hello("review-desk", r#"["observe","activity"]"#);
        let reply = c.call(r#"{"op":"subscribe","topics":["activity"]}"#);
        assert_eq!(reply["code"], "capability_not_granted");
    }

    #[test]
    fn a_subscription_is_a_snapshot_then_deltas() {
        let rig = rig();
        rig.view.lock().expect("view").sessions = vec![entry("a", None)];
        let mut c = Client::connect(&rig);
        c.hello("review-desk", r#"["observe"]"#);
        c.send(r#"{"op":"subscribe","topics":["sessions"]}"#);
        let snapshot = c.read();
        assert_eq!(snapshot["frame"], "snapshot");
        assert_eq!(snapshot["seq"], 0);
        assert_eq!(snapshot["sessions"][0]["session_id"], "a");

        rig.view.lock().expect("view").sessions[0].state = State::Running;
        let delta = c.read();
        assert_eq!(delta["frame"], "delta");
        assert_eq!(delta["seq"], 1);
        assert_eq!(delta["event"], "session.status_changed");
        assert_eq!(delta["session"]["state"], "running");
    }

    #[test]
    fn widened_status_answers_archived_sessions() {
        let rig = rig();
        {
            let mut view = rig.view.lock().expect("view");
            view.sessions = vec![entry("live", None)];
            let mut gone = entry("gone", None);
            gone.state = State::Archived;
            gone.archive_ref = Some("refs/allele/archive/gone".into());
            view.archived = vec![gone];
        }
        let mut c = Client::connect(&rig);
        c.hello("review-desk", r#"["observe"]"#);
        let live = c.call(r#"{"op":"sessions_status","session_id":"live"}"#);
        assert_eq!(live["session"]["turn"], 0, "the widened entry");
        let gone = c.call(r#"{"op":"sessions_status","session_id":"gone"}"#);
        assert_eq!(gone["session"]["state"], "archived");
        let never = c.call(r#"{"op":"sessions_status","session_id":"never"}"#);
        assert_eq!(never["code"], "session_not_found");
    }

    /// AC: a reader that never reads is dropped, and the foreground is
    /// unaffected. Here the "foreground" is the rig's publisher thread, and
    /// it is timed: no publish may take as long as a frame.
    #[test]
    fn a_client_that_never_reads_is_dropped() {
        let rig = rig();
        rig.view.lock().expect("view").sessions = vec![entry("a", None)];
        let mut c = Client::connect(&rig);
        c.hello("review-desk", r#"["observe"]"#);
        c.send(r#"{"op":"subscribe","topics":["sessions"]}"#);
        // Never read again. Push far more changes than the queue holds, as
        // fast as the hub will take them, timing each publish.
        let started = Instant::now();
        while rig.hub.subscriber_count() == 0 && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut slowest = Duration::ZERO;
        for turn in 0..(super::super::hub::QUEUE_CAPACITY as u64 * 4) {
            let mut v = rig.view.lock().expect("view").clone();
            v.sessions[0].turn = turn + 1;
            v.sessions[0].name = "x".repeat(512); // fill the socket buffer sooner
            let t = Instant::now();
            rig.hub.publish(v, Instant::now());
            slowest = slowest.max(t.elapsed());
        }
        assert_eq!(
            rig.hub.subscriber_count(),
            0,
            "the stalled reader was dropped"
        );
        assert!(
            slowest < Duration::from_millis(16),
            "a publish took {slowest:?}"
        );
        drop(c);
    }

    /// A dropped subscriber's writer says `lagged` before closing.
    #[test]
    fn a_lagged_stream_ends_with_lagged() {
        let hub = Hub::new();
        let sub = hub.subscribe(None);
        hub.publish(View::default(), Instant::now());
        sub.lagged.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        stream(sub, &mut out);
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text, "{\"frame\":\"lagged\"}\n");
    }

    /// The contract's error codes must name every code Allele can send, or a
    /// typed client reads a real refusal as `unknown`.
    #[test]
    fn every_legacy_error_code_is_in_the_contract() {
        use super::super::protocol::ErrorCode as Legacy;
        for code in [
            Legacy::AlleleNotRunning,
            Legacy::ProjectNotRegistered,
            Legacy::SourcePathMissing,
            Legacy::WorktreeDirty,
            Legacy::CloneFailed,
            Legacy::BranchResolutionFailed,
            Legacy::AgentNotConfigured,
            Legacy::CapacityExceeded,
            Legacy::DepthLimitExceeded,
            Legacy::PromptDeliveryUnconfirmed,
            Legacy::NotDispatched,
            Legacy::SessionNotFound,
            Legacy::PermissionDenied,
            Legacy::BadRequest,
            Legacy::Internal,
        ] {
            let wire = serde_json::to_value(code).expect("encodes");
            let parsed: contract::ErrorCode = serde_json::from_value(wire).expect("parses");
            assert_ne!(parsed, contract::ErrorCode::Unknown, "{code:?}");
        }
    }

    #[test]
    fn input_follows_state_and_process() {
        use crate::session::SessionStatus as S;
        assert_eq!(
            input_of(S::AwaitingInput, true),
            contract::Input::BlockedOnPrompt
        );
        assert_eq!(
            input_of(S::Running, true),
            contract::Input::DeliverableAtNextBoundary
        );
        assert_eq!(
            input_of(S::ResponseReady, true),
            contract::Input::DeliverableNow
        );
        assert_eq!(input_of(S::Idle, false), contract::Input::NoProcess);
    }

    // ── Through a real AppState ─────────────────────────────────────────

    use crate::app_state::fixture::Fixture;
    use crate::project::Project;
    use crate::session::{Session, SessionOrigin};
    use gpui::TestAppContext;

    fn project(sessions: &[(&str, Option<&str>)]) -> Project {
        let mut p = Project::new("alpha".to_string(), PathBuf::from("/tmp/alpha"));
        for (id, group) in sessions {
            let now = std::time::SystemTime::now();
            let mut s = Session::suspended_from_persisted(
                (*id).into(),
                format!("label {id}"),
                now,
                now,
                Duration::ZERO,
                None,
            );
            s.group = group.map(String::from);
            p.sessions.push(s);
        }
        p
    }

    fn hook(session_id: &str, kind: &str) -> crate::hooks::HookEvent {
        crate::hooks::HookEvent {
            session_id: session_id.into(),
            kind: kind.into(),
            cwd: None,
            payload: None,
        }
    }

    /// AC: a subscriber with `filter:{group:"review"}` gets `status_changed`
    /// within 1 s of a Notification hook for a `review`-group session, and
    /// nothing for an ungrouped one. Driven through `apply_hook_event`, the
    /// path the hook poller takes. In the running app the poller adds up to
    /// 250 ms and the diff interval up to 200 ms on top of what is timed here.
    #[gpui::test]
    fn a_notification_hook_reaches_a_group_subscriber(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![project(&[("r1", Some("review")), ("u1", None)])], cx);
        let hub = Hub::new();
        let sub = hub.subscribe(Some(contract::Filter {
            group: Some("review".into()),
            ..contract::Filter::default()
        }));
        let publish = |fx: &Fixture, cx: &mut TestAppContext| {
            fx.update(cx, |state, _w, cx| {
                hub.publish(view(state, cx, false), Instant::now())
            });
        };
        publish(&fx, cx);
        let snapshot: Frame =
            serde_json::from_str(&sub.frames.try_recv().expect("snapshot")).expect("frame");
        assert!(
            matches!(&snapshot, Frame::Snapshot { sessions, .. } if sessions.len() == 1),
            "only the review session: {snapshot:?}"
        );

        let hooked = Instant::now();
        fx.update(cx, |state, _w, cx| {
            state.apply_hook_event(hook("r1", "notification"), cx);
            state.apply_hook_event(hook("u1", "notification"), cx);
        });
        publish(&fx, cx);

        let frames: Vec<Frame> = sub
            .frames
            .try_iter()
            .map(|l| serde_json::from_str(&l).expect("frame"))
            .collect();
        assert!(hooked.elapsed() < Duration::from_secs(1));
        match frames.as_slice() {
            [Frame::Delta {
                seq: 1,
                event: Event::SessionStatusChanged { session },
            }] => {
                assert_eq!(session.session_id, "r1");
                assert_eq!(session.state, State::AwaitingInput);
            }
            other => panic!("expected one status_changed for r1, got {other:?}"),
        }
    }

    /// `parent`, `dispatched_by` and `started_at` come from what Allele
    /// already stores: the dispatch origin and the creation time.
    #[gpui::test]
    fn the_entry_carries_lineage_and_start_time(cx: &mut TestAppContext) {
        let mut p = project(&[("lead", None), ("child", None)]);
        p.sessions[1].origin = SessionOrigin::Dispatched {
            by_session_id: "lead".into(),
            by_label: "Lead".into(),
            depth: 1,
        };
        let started = p.sessions[1].started_at;
        let fx = Fixture::with_projects(vec![p], cx);
        let v = fx.update(cx, |state, _w, cx| view(state, cx, false));
        let child = &v.sessions[1];
        assert_eq!(child.parent.as_deref(), Some("lead"));
        assert_eq!(child.dispatched_by.as_deref(), Some("Lead"));
        assert_eq!(child.depth, 1);
        assert_eq!(child.started_at, super::super::hub::unix_secs(started));
        assert_eq!(v.sessions[0].parent, None);
        assert_eq!(v.sessions[0].dispatched_by, None);
    }
}
