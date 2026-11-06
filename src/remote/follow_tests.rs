//! The status feed end to end over the in-memory network (DEV-1162): two real
//! `RemoteService`s, one hosting and one following, with the real gate,
//! control stream, push task, `PeerLink` and follower between them.
//!
//! | criterion | test |
//! | -- | -- |
//! | a subscription receives every host session | `a_client_follows_a_host_from_its_first_snapshot` |
//! | a host status change reaches the client within 2 s | `a_host_status_change_reaches_the_client_within_two_seconds` |
//! | a closed session disappears; a rename shows | `removals_and_renames_reach_the_client` |
//! | the client resubscribes after the host drops, unaided | `a_dropped_host_is_resubscribed_without_user_action` |
//! | while unreachable, the client shows when it last was | `a_dropped_host_is_resubscribed_without_user_action` |
//! | a subscriber that lags gets a fresh snapshot | `a_lagging_subscriber_is_resynced_with_a_fresh_snapshot` |
//! | the feed needs `view` | `a_peer_without_view_is_refused_the_feed` |
//!
//! Remote ops on the same stream (DEV-1168):
//!
//! | criterion | test |
//! | -- | -- |
//! | the client learns its grant from the host's hello | `the_client_learns_its_grant_from_the_hosts_hello` |
//! | an op reaches the host and its answer comes back | `an_op_reaches_the_host_and_its_answer_comes_back` |
//! | the host's grant check reaches the client as an answer | `a_create_without_control_is_answered_permission_denied` |
//! | no live stream: unreachable, at once | `an_op_for_a_host_with_no_stream_is_unreachable` |
//! | a host that refused the stream: unreachable | `an_op_for_a_host_that_refused_the_feed_is_unreachable` |
//! | no answer: times out | `an_op_the_host_never_answers_times_out` |
//! | unpairing drops the grant | `following_stops_when_the_host_is_unpaired` |

#![cfg(test)]

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tokio::time::timeout;

use super::control::{ControlContext, HostName, RemoteDispatch, Unavailable};
use super::follow::CallError;
use super::listener::{self, Gate};
use super::pairing::HostPairing;
use super::peers::{PeerPermissions, TrustedPeer};
use super::registry::PeerRegistry;
use super::screen::ScreenSource;
use super::service::{Binder, RemoteService};
use super::testing::{MemNet, MemTransport};
use super::transport::{PeerConnection, PeerId, PeerStream, PeerTransport, TransportConfig};
use super::wire::{read_frame_within, write_frame, Frame, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::dispatch::protocol::{
    CreateRequest, CreatedSession, ErrorCode, ProjectSummary, Request, Response, SessionState,
};
use crate::errors::Result;
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::session::Orchestration;
use crate::settings::RemoteSettings;
use crate::status_feed::client::Reachability;
use crate::status_feed::model::{FeedStatus, SessionEntry};
use crate::status_feed::{RemoteHostState, StatusFeed};
use crate::terminal::mirror::MirrorTerm;

/// Binds on the in-memory network and keeps every endpoint it made, so a test
/// can reach the host's side of a connection. Like the Keychain-backed
/// production binder, every bind after the first reuses the first identity.
pub(super) struct MemBinder {
    net: Arc<MemNet>,
    made: Mutex<Vec<Arc<MemTransport>>>,
}

#[async_trait]
impl Binder for MemBinder {
    async fn bind(&self, _: &TransportConfig) -> Result<Arc<dyn PeerTransport>> {
        let mut made = self.made.lock().unwrap();
        let t = match made.first() {
            Some(first) => self.net.endpoint_as(first.local_id()),
            None => self.net.endpoint(),
        };
        made.push(Arc::clone(&t));
        Ok(t)
    }
}

pub(super) struct Mac {
    pub(super) svc: RemoteService,
    binder: Arc<MemBinder>,
    pub(super) feed: StatusFeed,
    pub(super) id: PeerId,
    /// The terminals this Mac serves to `attach`, by session id (DEV-1167).
    pub(super) screens: Arc<TestScreens>,
}

/// Terminals by session id, for `attach`.
#[derive(Default)]
pub(super) struct TestScreens(
    pub(super) Mutex<std::collections::HashMap<String, Arc<dyn MirrorTerm>>>,
);

#[async_trait]
impl ScreenSource for TestScreens {
    async fn find(&self, session_id: &str) -> Result<Option<Arc<dyn MirrorTerm>>> {
        Ok(self.0.lock().unwrap().get(session_id).cloned())
    }
}

impl Mac {
    pub(super) async fn start(net: &Arc<MemNet>, name: &str, allow: bool) -> Self {
        Self::start_with(net, name, allow, Arc::new(Unavailable)).await
    }

    async fn start_with(
        net: &Arc<MemNet>,
        name: &str,
        allow: bool,
        dispatch: Arc<dyn RemoteDispatch>,
    ) -> Self {
        let binder = Arc::new(MemBinder {
            net: Arc::clone(net),
            made: Mutex::default(),
        });
        let feed = StatusFeed::new();
        let screens = Arc::new(TestScreens::default());
        let settings = RemoteSettings {
            allow_connections: allow,
            ..Default::default()
        };
        let control = ControlContext {
            dispatch,
            host_name: HostName::new(Some(name.into()), || None),
            status_feed: feed.clone(),
            screens: screens.clone(),
            viewers: Default::default(),
        };
        let svc = RemoteService::new(
            tokio::runtime::Handle::current(),
            Arc::new(InMemoryPeersRepository::default()),
            binder.clone(),
            &settings,
            control,
        );
        let id = svc.transport().await.expect("bind").local_id();
        Self {
            svc,
            binder,
            feed,
            id,
            screens,
        }
    }

    pub(super) fn trust(&self, other: &Mac, name: &str) {
        self.svc
            .registry()
            .add_paired(TrustedPeer::newly_paired(other.id, name))
            .unwrap();
    }

    /// The host's end of every connection a client opened to it.
    pub(super) fn accepted(&self) -> Vec<Arc<super::testing::MemConn>> {
        self.binder.made.lock().unwrap()[0].accepted()
    }
}

fn entry(id: &str, label: &str, status: FeedStatus) -> SessionEntry {
    SessionEntry {
        id: id.into(),
        label: label.into(),
        project_id: "p1".into(),
        project_name: "allele".into(),
        group: None,
        status,
        attention: None,
        last_activity_ms: 1,
    }
}

/// A desk Mac hosting, a laptop following it — paired both ways, the laptop
/// with its own switch off (following is outbound).
pub(super) async fn desk_and_laptop(net: &Arc<MemNet>) -> (Mac, Mac) {
    let desk = Mac::start(net, "Desk", true).await;
    let laptop = Mac::start(net, "Laptop", false).await;
    desk.trust(&laptop, "Laptop");
    laptop.trust(&desk, "Desk");
    (desk, laptop)
}

fn host_state(laptop: &Mac, desk: &Mac) -> Option<RemoteHostState> {
    laptop.svc.hosts().get(desk.id)
}

/// Wait up to 5 s for `cond` on the laptop's view of the desk.
async fn until(laptop: &Mac, desk: &Mac, cond: impl Fn(&RemoteHostState) -> bool) {
    let waited = timeout(Duration::from_secs(5), async {
        loop {
            if host_state(laptop, desk).is_some_and(|h| cond(&h)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "timed out; the laptop sees {:?}",
        host_state(laptop, desk)
    );
}

fn status_of(h: &RemoteHostState, id: &str) -> Option<FeedStatus> {
    h.sessions().iter().find(|s| s.id == id).map(|s| s.status)
}

#[tokio::test]
async fn a_client_follows_a_host_from_its_first_snapshot() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed.publish(vec![
        entry("a", "one", FeedStatus::Idle),
        entry("b", "two", FeedStatus::AwaitingInput),
    ]);

    until(&laptop, &desk, |h| h.sessions().len() == 2).await;
    let h = host_state(&laptop, &desk).unwrap();
    assert_eq!(h.reachability(), Reachability::Connected);
    assert_eq!(h.awaiting_input().count(), 1);
}

#[tokio::test]
async fn a_host_status_change_reaches_the_client_within_two_seconds() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed
        .publish(vec![entry("a", "one", FeedStatus::Running)]);
    until(&laptop, &desk, |h| {
        status_of(h, "a") == Some(FeedStatus::Running)
    })
    .await;

    let mut transitions = laptop.svc.hosts().subscribe_transitions();
    let started = Instant::now();
    desk.feed
        .publish(vec![entry("a", "one", FeedStatus::AwaitingInput)]);
    until(&laptop, &desk, |h| {
        status_of(h, "a") == Some(FeedStatus::AwaitingInput)
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );

    // And DEV-1164 is told, as a live transition from this host.
    let t = timeout(Duration::from_secs(2), transitions.recv())
        .await
        .expect("a transition in time")
        .unwrap();
    assert_eq!(t.peer, desk.id);
    assert!(t.transition.enters_awaiting_input());
}

#[tokio::test]
async fn removals_and_renames_reach_the_client() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed.publish(vec![
        entry("a", "one", FeedStatus::Idle),
        entry("b", "two", FeedStatus::Idle),
    ]);
    until(&laptop, &desk, |h| h.sessions().len() == 2).await;

    desk.feed
        .publish(vec![entry("b", "fix the build", FeedStatus::Idle)]);
    until(&laptop, &desk, |h| {
        h.sessions().len() == 1 && h.sessions()[0].label == "fix the build"
    })
    .await;
}

/// The host sleeping and waking looks, from the client, like its connection
/// dying and the host answering again. Closing the host's end of the
/// connection is that, without the sleep.
#[tokio::test]
async fn a_dropped_host_is_resubscribed_without_user_action() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed
        .publish(vec![entry("a", "one", FeedStatus::Running)]);
    until(&laptop, &desk, |h| {
        h.reachability() == Reachability::Connected
    })
    .await;

    let before_drop = SystemTime::now();
    for conn in desk.accepted() {
        conn.close("host went to sleep");
    }
    until(&laptop, &desk, |h| {
        matches!(h.reachability(), Reachability::Unreachable { .. })
    })
    .await;
    let h = host_state(&laptop, &desk).unwrap();
    let Reachability::Unreachable { since } = h.reachability() else {
        unreachable!()
    };
    assert!(since >= before_drop, "since is when it went away");
    assert_eq!(h.sessions().len(), 1, "the last-known sessions stay listed");

    // Things change on the host while the laptop is away. Nobody clicks
    // anything; the link redials and the follower resubscribes.
    desk.feed.publish(vec![
        entry("a", "one", FeedStatus::ResponseReady),
        entry("c", "new", FeedStatus::Idle),
    ]);
    until(&laptop, &desk, |h| {
        h.reachability() == Reachability::Connected
            && status_of(h, "a") == Some(FeedStatus::ResponseReady)
            && h.sessions().len() == 2
    })
    .await;
    assert!(
        desk.accepted().len() >= 2,
        "a second connection carried the new subscription"
    );
}

/// DEV-1186: a relay change on the follower rebinds its endpoint; the
/// follower's link reconnects through the new one and the host's state
/// recovers without anyone touching it.
#[tokio::test]
async fn a_follower_recovers_after_its_relay_changes() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed
        .publish(vec![entry("a", "one", FeedStatus::Running)]);
    until(&laptop, &desk, |h| {
        h.reachability() == Reachability::Connected
    })
    .await;
    let first = desk.accepted();
    assert_eq!(first.len(), 1);

    laptop.svc.set_settings(&RemoteSettings {
        relay_url: Some("https://relay.example.com".into()),
        ..Default::default()
    });
    let rebound = timeout(Duration::from_secs(5), async {
        while laptop.binder.made.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(rebound.is_ok(), "the laptop rebinds once the relay settles");
    assert!(
        first[0].is_closed(),
        "the rebind dropped the old connection"
    );
    assert_eq!(laptop.svc.transport().await.unwrap().local_id(), laptop.id);

    // The host moves on; the follower catches up over the new endpoint.
    desk.feed.publish(vec![
        entry("a", "one", FeedStatus::ResponseReady),
        entry("b", "two", FeedStatus::Idle),
    ]);
    until(&laptop, &desk, |h| {
        h.reachability() == Reachability::Connected
            && status_of(h, "a") == Some(FeedStatus::ResponseReady)
            && h.sessions().len() == 2
    })
    .await;
    assert!(desk.accepted().len() >= 2, "a new connection carried it");
    let cleared = timeout(Duration::from_secs(2), async {
        while laptop.svc.status().reconnecting {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(cleared.is_ok(), "the pane notice clears once it is back");
}

#[tokio::test]
async fn following_stops_when_the_host_is_unpaired() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed.publish(vec![entry("a", "one", FeedStatus::Idle)]);
    until(&laptop, &desk, |h| h.sessions().len() == 1).await;

    assert!(laptop.svc.hosts().grant(desk.id).is_some());
    laptop.svc.remove_peer(desk.id).unwrap();
    let gone = timeout(Duration::from_secs(5), async {
        while host_state(&laptop, &desk).is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "an unpaired host is dropped from the list");
    assert_eq!(
        laptop.svc.hosts().grant(desk.id),
        None,
        "and so is what it let this Mac do"
    );
}

// ── the host's push, over a raw control stream ───────────────────────────

/// A host running just the gate and listener, and a client speaking frames.
async fn raw_host(
    net: &Arc<MemNet>,
    permissions: PeerPermissions,
) -> (
    StatusFeed,
    PeerRegistry,
    PeerStream,
    Arc<dyn PeerConnection>,
) {
    let host = net.endpoint();
    let laptop = net.endpoint();
    let feed = StatusFeed::new();
    let registry = PeerRegistry::load(Arc::new(InMemoryPeersRepository::default()));
    let mut peer = TrustedPeer::newly_paired(laptop.local_id(), "Laptop");
    peer.permissions = permissions;
    registry.add_paired(peer).unwrap();
    let gate = Gate::new(
        registry.clone(),
        Arc::new(HostPairing::default()),
        Arc::new(AtomicBool::new(true)),
        ControlContext {
            dispatch: Arc::new(Unavailable),
            host_name: HostName::new(Some("Desk".into()), || None),
            status_feed: feed.clone(),
            screens: Arc::new(crate::remote::screen::NoScreens),
            viewers: Default::default(),
        },
    );
    tokio::spawn(listener::run(host.clone() as Arc<dyn PeerTransport>, gate));

    let conn = laptop.dial_mem(host.local_id()).await;
    let mut stream = conn.open_stream().await.unwrap();
    let hello = Frame::Hello {
        protocol: PROTOCOL_VERSION,
        machine_name: "Laptop".into(),
        permissions: None,
    };
    write_frame(&mut stream.send, &hello).await.unwrap();
    assert!(matches!(read(&mut stream).await, Frame::Hello { .. }));
    write_frame(&mut stream.send, &Frame::SubscribeStatus)
        .await
        .unwrap();
    (feed, registry, stream, conn)
}

async fn read(stream: &mut PeerStream) -> Frame {
    timeout(
        Duration::from_secs(5),
        read_frame_within(&mut stream.recv, MAX_PEER_FRAME),
    )
    .await
    .expect("a frame in time")
    .expect("a readable frame")
}

#[tokio::test]
async fn a_lagging_subscriber_is_resynced_with_a_fresh_snapshot() {
    let net = MemNet::new();
    let (feed, _registry, mut stream, _conn) = raw_host(&net, PeerPermissions::NEW_PEER).await;
    let Frame::StatusSnapshot { seq: first, .. } = read(&mut stream).await else {
        panic!("a subscription starts with a snapshot")
    };

    // The client stops reading while the host changes far more than the
    // stream, the outbox and the feed's buffer can hold between them.
    for i in 0..3_000u32 {
        let status = if i % 2 == 0 {
            FeedStatus::Running
        } else {
            FeedStatus::Idle
        };
        feed.publish(vec![entry("a", "one", status)]);
        if i % 100 == 0 {
            tokio::task::yield_now().await;
        }
    }
    let last_seq = feed.subscribe().snapshot.seq;

    // Reading again: consecutive deltas, then — instead of the lost ones —
    // a fresh snapshot, then the rest. Fold it all into a client store.
    let mut client = RemoteHostState::new();
    client.apply_snapshot(
        crate::status_feed::StatusSnapshot {
            seq: first,
            sessions: vec![],
        },
        SystemTime::now(),
    );
    let mut resnapshots = 0;
    let mut expected = first + 1;
    loop {
        match read(&mut stream).await {
            Frame::StatusDelta { seq, change } => {
                assert_eq!(seq, expected, "deltas between snapshots never skip");
                expected += 1;
                client.apply_delta(
                    crate::status_feed::StatusDelta { seq, change },
                    SystemTime::now(),
                );
            }
            Frame::StatusSnapshot { seq, sessions } => {
                resnapshots += 1;
                expected = seq + 1;
                client.apply_snapshot(
                    crate::status_feed::StatusSnapshot { seq, sessions },
                    SystemTime::now(),
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        if expected > last_seq {
            break;
        }
    }
    assert!(resnapshots >= 1, "the lag was answered with a snapshot");
    assert_eq!(
        client.sessions(),
        feed.subscribe().snapshot.sessions.as_slice(),
        "and the client ends where the host is"
    );
}

#[tokio::test]
async fn a_peer_without_view_is_refused_the_feed() {
    let net = MemNet::new();
    let no_view = PeerPermissions {
        view: false,
        control: false,
    };
    let (_feed, _registry, mut stream, conn) = raw_host(&net, no_view).await;
    assert!(matches!(
        read(&mut stream).await,
        Frame::StatusRefused { .. }
    ));
    assert!(!conn.is_closed(), "the control stream carries on");
}

// ── remote ops on the follower's stream (DEV-1168) ───────────────────────

/// Answers like a host's dispatch drain would, and remembers what it was asked.
#[derive(Default)]
struct Script {
    asked: Mutex<Vec<Request>>,
    /// Never answer: for the timeout test.
    silent: bool,
}

#[async_trait]
impl RemoteDispatch for Script {
    async fn call(&self, op: Request) -> Response {
        self.asked.lock().unwrap().push(op.clone());
        if self.silent {
            std::future::pending::<()>().await;
        }
        match op {
            Request::ProjectsList => Response::Projects {
                projects: vec![ProjectSummary {
                    id: "p1".into(),
                    name: "allele".into(),
                    source_path: "/Users/someone/allele".into(),
                    session_count: 0,
                }],
            },
            Request::SessionsCreate(req) => Response::Created {
                session: CreatedSession {
                    session_id: "s-new".into(),
                    name: req.name,
                    project: req.project,
                    state: SessionState::Running,
                    reply_to: None,
                },
            },
            _ => Response::Error {
                code: ErrorCode::BadRequest,
                message: "not scripted".into(),
            },
        }
    }
}

async fn scripted_desk_and_laptop(net: &Arc<MemNet>, script: Arc<Script>) -> (Mac, Mac) {
    let desk = Mac::start_with(net, "Desk", true, script).await;
    let laptop = Mac::start(net, "Laptop", false).await;
    desk.trust(&laptop, "Laptop");
    laptop.trust(&desk, "Desk");
    until(&laptop, &desk, |h| {
        h.reachability() == Reachability::Connected
    })
    .await;
    (desk, laptop)
}

fn create(name: &str) -> Request {
    Request::SessionsCreate(CreateRequest {
        project: "p1".into(),
        name: name.into(),
        prompt: String::new(),
        orchestration: Orchestration::Nothing,
        caller_session_id: None,
        caller_reply_to: None,
        group: None,
    })
}

#[tokio::test]
async fn the_client_learns_its_grant_from_the_hosts_hello() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    let stranger = net.endpoint().local_id();
    assert_eq!(laptop.svc.hosts().grant(stranger), None, "never greeted");

    until(&laptop, &desk, |h| {
        h.reachability() == Reachability::Connected
    })
    .await;
    assert_eq!(
        laptop.svc.hosts().grant(desk.id),
        Some(PeerPermissions::NEW_PEER),
        "a new pairing is view only, and the host says so"
    );
}

#[tokio::test]
async fn an_op_reaches_the_host_and_its_answer_comes_back() {
    let net = MemNet::new();
    let script = Arc::new(Script::default());
    let (desk, laptop) = scripted_desk_and_laptop(&net, Arc::clone(&script)).await;

    let Ok(Response::Projects { projects }) = laptop.svc.call(desk.id, Request::ProjectsList).await
    else {
        panic!("projects_list should be answered");
    };
    assert_eq!(projects[0].name, "allele");
    assert_eq!(
        projects[0].source_path, "",
        "the host's paths stay on the host"
    );

    desk.svc.set_control(laptop.id, true).unwrap();
    let answer = laptop.svc.call(desk.id, create("from the laptop")).await;
    let Ok(Response::Created { session }) = answer else {
        panic!("a create with control should start a session: {answer:?}");
    };
    assert_eq!(session.session_id, "s-new");
    assert!(
        matches!(
            script.asked.lock().unwrap().last(),
            Some(Request::SessionsCreate(r)) if r.name == "from the laptop"
        ),
        "the create ran on the host"
    );
}

#[tokio::test]
async fn a_create_without_control_is_answered_permission_denied() {
    let net = MemNet::new();
    let script = Arc::new(Script::default());
    let (desk, laptop) = scripted_desk_and_laptop(&net, Arc::clone(&script)).await;

    let answer = laptop.svc.call(desk.id, create("nope")).await;
    assert!(
        matches!(
            answer,
            Ok(Response::Error {
                code: ErrorCode::PermissionDenied,
                ..
            })
        ),
        "got {answer:?}"
    );
    assert!(script.asked.lock().unwrap().is_empty(), "nothing ran");
}

#[tokio::test]
async fn an_op_for_a_host_with_no_stream_is_unreachable() {
    let net = MemNet::new();
    let laptop = Mac::start(&net, "Laptop", false).await;
    let unpaired = net.endpoint().local_id();
    assert_eq!(
        laptop.svc.call(unpaired, Request::ProjectsList).await,
        Err(CallError::Unreachable),
        "not followed at all"
    );

    // Paired, but its switch is off: the follower never gets a stream.
    let desk = Mac::start(&net, "Desk", false).await;
    desk.trust(&laptop, "Laptop");
    laptop.trust(&desk, "Desk");
    let started = Instant::now();
    let answer = timeout(Duration::from_secs(5), async {
        loop {
            // The follower may not exist for a moment after pairing.
            match laptop.svc.call(desk.id, Request::ProjectsList).await {
                Err(CallError::Unreachable) => return,
                other => {
                    let _ = other;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    })
    .await;
    assert!(answer.is_ok(), "answered unreachable, not left waiting");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn an_op_for_a_host_that_refused_the_feed_is_unreachable() {
    let net = MemNet::new();
    let desk = Mac::start(&net, "Desk", true).await;
    let laptop = Mac::start(&net, "Laptop", false).await;
    let mut no_view = TrustedPeer::newly_paired(laptop.id, "Laptop");
    no_view.permissions = PeerPermissions {
        view: false,
        control: true,
    };
    desk.svc.registry().add_paired(no_view).unwrap();
    laptop.trust(&desk, "Desk");

    // The hello still states the grant before the feed is refused.
    let greeted = timeout(Duration::from_secs(5), async {
        while laptop.svc.hosts().grant(desk.id).is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(greeted.is_ok());
    // Refused, the follower holds no stream for 30 s; ops are not queued
    // behind that.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let answer = timeout(
        Duration::from_secs(2),
        laptop.svc.call(desk.id, Request::ProjectsList),
    )
    .await
    .expect("answered at once");
    assert_eq!(answer, Err(CallError::Unreachable));
}

#[tokio::test]
async fn an_op_the_host_never_answers_times_out() {
    let net = MemNet::new();
    let script = Arc::new(Script {
        silent: true,
        ..Default::default()
    });
    let (desk, laptop) = scripted_desk_and_laptop(&net, script).await;

    let answer = laptop
        .svc
        .call_within(desk.id, Request::ProjectsList, Duration::from_millis(200))
        .await;
    assert_eq!(answer, Err(CallError::TimedOut));
}
