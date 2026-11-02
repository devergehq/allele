//! DEV-1186: changing the relay rebinds the endpoint live.
//!
//! Driven through `RemoteService` with an in-memory binder that, like the
//! Keychain-backed production binder, binds the same identity every time and
//! records the config it was asked to bind with.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::errors::Result;
use crate::remote::control::ControlContext;
use crate::remote::link::{LinkState, PeerLink, ReconnectPolicy};
use crate::remote::service::{Binder, RemoteService, RELAY_SETTLE};
use crate::remote::testing::MemNet;
use crate::remote::transport::{PeerId, PeerTransport, RelayChoice, TransportConfig};
use crate::remote::LocalIdentity;
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::settings::RemoteSettings;

const NEW_RELAY: &str = "https://relay.example.com";

struct StableBinder {
    net: Arc<MemNet>,
    id: PeerId,
    binds: Mutex<Vec<TransportConfig>>,
    /// Binds after this many succeed fail, like a relay iroh cannot use.
    fail_after: usize,
}

impl StableBinder {
    fn binds(&self) -> Vec<TransportConfig> {
        self.binds.lock().unwrap().clone()
    }
}

#[async_trait]
impl Binder for StableBinder {
    async fn bind(&self, config: &TransportConfig) -> Result<Arc<dyn PeerTransport>> {
        let mut binds = self.binds.lock().unwrap();
        binds.push(config.clone());
        if binds.len() > self.fail_after {
            return Err(crate::errors::AlleleError::Remote("bind refused".into()));
        }
        Ok(self.net.endpoint_as(self.id))
    }
}

fn settings(allow: bool, relay: Option<&str>) -> RemoteSettings {
    RemoteSettings {
        allow_connections: allow,
        relay_url: relay.map(str::to_string),
        ..Default::default()
    }
}

fn service(net: &Arc<MemNet>, allow: bool) -> (RemoteService, Arc<StableBinder>) {
    service_failing_after(net, allow, usize::MAX)
}

fn service_failing_after(
    net: &Arc<MemNet>,
    allow: bool,
    fail_after: usize,
) -> (RemoteService, Arc<StableBinder>) {
    let binder = Arc::new(StableBinder {
        net: Arc::clone(net),
        id: LocalIdentity::ephemeral().id(),
        binds: Mutex::new(Vec::new()),
        fail_after,
    });
    let svc = RemoteService::new(
        tokio::runtime::Handle::current(),
        Arc::new(InMemoryPeersRepository::default()),
        Arc::clone(&binder) as Arc<dyn Binder>,
        &settings(allow, None),
        ControlContext::unavailable(),
    );
    (svc, binder)
}

/// Poll `cond` until it holds or `within` passes.
async fn eventually(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

fn past_settle() -> Duration {
    RELAY_SETTLE + Duration::from_millis(500)
}

/// AC: a relay change makes new connections use the new relay, no restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_change_rebinds_with_the_new_relay() {
    let net = MemNet::new();
    let (svc, binder) = service(&net, true);
    svc.reconcile_now().await;
    assert_eq!(binder.binds().len(), 1);
    assert_eq!(binder.binds()[0].relay, RelayChoice::N0);

    svc.set_settings(&settings(true, Some(NEW_RELAY)));
    assert!(eventually(Duration::from_secs(5), || binder.binds().len() == 2).await);
    assert_eq!(
        binder.binds()[1].relay,
        RelayChoice::Custom(NEW_RELAY.into())
    );
    assert!(eventually(Duration::from_secs(1), || svc.status().listening).await);
}

/// AC: the local id is unchanged after a rebind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_endpoint_keeps_its_id_across_a_rebind() {
    let net = MemNet::new();
    let (svc, binder) = service(&net, true);
    let handle = svc.endpoint().await.unwrap();
    let before = handle.local_id();

    svc.set_settings(&settings(true, Some(NEW_RELAY)));
    assert!(eventually(Duration::from_secs(5), || binder.binds().len() == 2).await);
    let after = svc.endpoint().await.unwrap();
    assert_eq!(after.local_id(), before);
    assert!(Arc::ptr_eq(&handle, &after), "links keep one handle");
}

/// Typing a URL saves once per keystroke; the endpoint rebinds once, for the
/// value the field settled on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_changes_rebinds_once_after_it_settles() {
    let net = MemNet::new();
    let (svc, binder) = service(&net, true);
    svc.reconcile_now().await;

    for url in [
        "https://r",
        "https://re",
        "https://relay.example.co",
        NEW_RELAY,
    ] {
        svc.set_settings(&settings(true, Some(url)));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(RELAY_SETTLE / 2).await;
    assert_eq!(binder.binds().len(), 1, "still settling");

    assert!(eventually(Duration::from_secs(5), || binder.binds().len() == 2).await);
    tokio::time::sleep(past_settle()).await;
    let binds = binder.binds();
    assert_eq!(binds.len(), 2, "exactly one rebind: {binds:?}");
    assert_eq!(binds[1].relay, RelayChoice::Custom(NEW_RELAY.into()));
}

/// A machine-name change also calls `set_settings`; it must not rebind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_relay_does_not_rebind() {
    let net = MemNet::new();
    let (svc, binder) = service(&net, true);
    svc.reconcile_now().await;

    let mut renamed = settings(true, None);
    renamed.machine_name = Some("Studio".into());
    svc.set_settings(&renamed);
    tokio::time::sleep(past_settle()).await;
    assert_eq!(binder.binds().len(), 1);
}

/// With nothing bound, a relay change only updates what the next bind uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_change_while_down_binds_nothing() {
    let net = MemNet::new();
    let (svc, binder) = service(&net, false);
    svc.reconcile_now().await;
    svc.set_settings(&settings(false, Some(NEW_RELAY)));
    tokio::time::sleep(past_settle()).await;
    assert!(binder.binds().is_empty());

    svc.set_allow(true);
    assert!(eventually(Duration::from_secs(5), || binder.binds().len() == 1).await);
    assert_eq!(
        binder.binds()[0].relay,
        RelayChoice::Custom(NEW_RELAY.into())
    );
}

/// AC: an invalid relay URL is rejected before anything is torn down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_relay_is_rejected_before_anything_is_torn_down() {
    let net = MemNet::new();
    let peer = net.endpoint();
    let (svc, binder) = service(&net, true);
    let handle = svc.endpoint().await.unwrap();
    let conn = handle.dial(peer.local_id()).await.unwrap();

    svc.set_settings(&settings(true, Some("relay.example.com:443")));
    let status = svc.status();
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("invalid relay URL"),
        "{status:?}"
    );
    tokio::time::sleep(past_settle()).await;
    assert_eq!(binder.binds().len(), 1, "no rebind");
    assert!(svc.status().listening);
    assert!(!svc.status().reconnecting);
    assert!(!conn.is_closed(), "the live connection was left alone");
}

/// AC: live connections reconnect within 10 s of the change — here, a
/// `PeerLink` dialing through the service's endpoint handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_reconnects_through_the_new_endpoint_within_ten_seconds() {
    let net = MemNet::new();
    let host = net.endpoint();
    let (svc, binder) = service(&net, true);
    let handle = svc.endpoint().await.unwrap();
    let link = PeerLink::spawn(
        &tokio::runtime::Handle::current(),
        handle,
        host.local_id(),
        ReconnectPolicy::default(),
    );
    let first = link.wait_connected().await.unwrap();
    assert!(!svc.status().reconnecting);

    svc.set_settings(&settings(true, Some(NEW_RELAY)));
    let changed = tokio::time::Instant::now();
    assert!(eventually(Duration::from_secs(5), || binder.binds().len() == 2).await);
    assert!(first.is_closed(), "the rebind dropped the old connection");

    let mut state = link.subscribe();
    let second = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let LinkState::Connected(c) = &*state.borrow_and_update() {
                if !Arc::ptr_eq(c, &first) && !c.is_closed() {
                    return Arc::clone(c);
                }
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .expect("reconnected within 10 s");
    assert!(changed.elapsed() < Duration::from_secs(10));
    assert_eq!(second.remote_id(), host.local_id());
    assert!(
        eventually(Duration::from_secs(2), || !svc.status().reconnecting).await,
        "the notice clears once the link is back"
    );
    link.shutdown().await;
}

/// The pane's notice: on from the rebind until the peers are back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_says_reconnecting_until_the_peer_is_back() {
    let net = MemNet::new();
    let host = net.endpoint();
    let (svc, binder) = service(&net, true);
    let handle = svc.endpoint().await.unwrap();
    let _conn = handle.dial(host.local_id()).await.unwrap();

    svc.set_settings(&settings(true, Some(NEW_RELAY)));
    assert!(eventually(Duration::from_secs(5), || binder.binds().len() == 2).await);
    // Nobody redials: the peer stays missing, so the notice stays up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(svc.status().reconnecting);

    let _again = handle.dial(host.local_id()).await.unwrap();
    assert!(!svc.status().reconnecting);
}

/// Correcting an invalid relay back to the one in use clears the error,
/// though nothing needs rebinding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoring_the_relay_in_use_clears_the_invalid_relay_error() {
    let net = MemNet::new();
    let (svc, binder) = service(&net, true);
    let _handle = svc.endpoint().await.unwrap();

    svc.set_settings(&settings(true, Some("relay.example.com:443")));
    assert!(svc.status().error.is_some());
    svc.set_settings(&settings(true, None));
    assert_eq!(svc.status().error, None);
    tokio::time::sleep(past_settle()).await;
    assert_eq!(binder.binds().len(), 1, "no rebind");
}

/// A rebind that fails reports the failure and drops the reconnect notice,
/// rather than claiming to reconnect through an endpoint that is down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_rebind_does_not_say_reconnecting() {
    let net = MemNet::new();
    let host = net.endpoint();
    let (svc, binder) = service_failing_after(&net, true, 1);
    let handle = svc.endpoint().await.unwrap();
    let _conn = handle.dial(host.local_id()).await.unwrap();

    svc.set_settings(&settings(true, Some(NEW_RELAY)));
    assert!(
        eventually(Duration::from_secs(5), || svc
            .status()
            .error
            .is_some_and(|e| e.contains("could not start")))
        .await
    );
    assert!(binder.binds().len() >= 2);
    let status = svc.status();
    assert!(!status.reconnecting, "{status:?}");
    assert!(!status.listening);
}
