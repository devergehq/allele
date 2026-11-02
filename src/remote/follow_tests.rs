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

#![cfg(test)]

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use tokio::time::timeout;

use super::control::{ControlContext, HostName, Unavailable};
use super::listener::{self, Gate};
use super::pairing::HostPairing;
use super::peers::{PeerPermissions, TrustedPeer};
use super::registry::PeerRegistry;
use super::service::{Binder, RemoteService};
use super::testing::{MemNet, MemTransport};
use super::transport::{PeerConnection, PeerId, PeerStream, PeerTransport, TransportConfig};
use super::wire::{read_frame_within, write_frame, Frame, MAX_PEER_FRAME, PROTOCOL_VERSION};
use crate::errors::Result;
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::settings::RemoteSettings;
use crate::status_feed::client::Reachability;
use crate::status_feed::model::{FeedStatus, SessionEntry};
use crate::status_feed::{RemoteHostState, StatusFeed};

/// Binds on the in-memory network and keeps every endpoint it made, so a test
/// can reach the host's side of a connection.
struct MemBinder {
    net: Arc<MemNet>,
    made: Mutex<Vec<Arc<MemTransport>>>,
}

#[async_trait]
impl Binder for MemBinder {
    async fn bind(&self, _: &TransportConfig) -> Result<Arc<dyn PeerTransport>> {
        let t = self.net.endpoint();
        self.made.lock().unwrap().push(Arc::clone(&t));
        Ok(t)
    }
}

struct Mac {
    svc: RemoteService,
    binder: Arc<MemBinder>,
    feed: StatusFeed,
    id: PeerId,
}

impl Mac {
    async fn start(net: &Arc<MemNet>, name: &str, allow: bool) -> Self {
        let binder = Arc::new(MemBinder {
            net: Arc::clone(net),
            made: Mutex::default(),
        });
        let feed = StatusFeed::new();
        let settings = RemoteSettings {
            allow_connections: allow,
            ..Default::default()
        };
        let control = ControlContext {
            dispatch: Arc::new(Unavailable),
            host_name: HostName::new(Some(name.into()), || None),
            status_feed: feed.clone(),
            screens: Arc::new(crate::remote::screen::NoScreens),
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
        }
    }

    fn trust(&self, other: &Mac, name: &str) {
        self.svc
            .registry()
            .add_paired(TrustedPeer::newly_paired(other.id, name))
            .unwrap();
    }

    /// The host's end of every connection a client opened to it.
    fn accepted(&self) -> Vec<Arc<super::testing::MemConn>> {
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
async fn desk_and_laptop(net: &Arc<MemNet>) -> (Mac, Mac) {
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

#[tokio::test]
async fn following_stops_when_the_host_is_unpaired() {
    let net = MemNet::new();
    let (desk, laptop) = desk_and_laptop(&net).await;
    desk.feed.publish(vec![entry("a", "one", FeedStatus::Idle)]);
    until(&laptop, &desk, |h| h.sessions().len() == 1).await;

    laptop.svc.remove_peer(desk.id).unwrap();
    let gone = timeout(Duration::from_secs(5), async {
        while host_state(&laptop, &desk).is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "an unpaired host is dropped from the list");
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
