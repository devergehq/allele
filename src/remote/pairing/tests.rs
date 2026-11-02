//! End-to-end pairing over the in-memory network: a host running the real
//! listener gate, and a client running the real `pair_with`.
//!
//! These are the DEV-1160 acceptance criteria that need two machines; the
//! single-machine ones are in `window`, `registry` and `repositories`.

#![cfg(test)]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::{timeout, Instant};

use super::code::{confirmation_number, PairingCode, PairingSecret};
use super::host::HostView;
use super::window::{Outcome, PairingRequest, CODE_TTL};
use super::{pair_with, HostPairing};
use crate::errors::Result;
use crate::remote::listener::{self, Gate, MAX_UNPAIRED};
use crate::remote::peers::TrustedPeer;
use crate::remote::registry::PeerRegistry;
use crate::remote::service::{Binder, RemoteService};
use crate::remote::testing::{MemNet, MemTransport};
use crate::remote::transport::{PeerConnection, PeerTransport, TransportConfig};
use crate::repositories::fakes::InMemoryPeersRepository;
use crate::settings::RemoteSettings;

struct Host {
    t: Arc<MemTransport>,
    repo: InMemoryPeersRepository,
    registry: PeerRegistry,
    pairing: Arc<HostPairing>,
    allow: Arc<AtomicBool>,
}

impl Host {
    fn start(net: &Arc<MemNet>) -> Self {
        let t = net.endpoint();
        let repo = InMemoryPeersRepository::default();
        let registry = PeerRegistry::load(Arc::new(repo.clone()));
        let pairing = Arc::new(HostPairing::default());
        let allow = Arc::new(AtomicBool::new(false));
        let gate = Gate::new(registry.clone(), Arc::clone(&pairing), Arc::clone(&allow));
        tokio::spawn(listener::run(t.clone() as Arc<dyn PeerTransport>, gate));
        Self {
            t,
            repo,
            registry,
            pairing,
            allow,
        }
    }

    /// Show a code, as *Pair a new machine* does.
    fn show_code(&self) -> PairingCode {
        let code = PairingCode {
            host: self.t.local_id(),
            secret: PairingSecret::generate().unwrap(),
        };
        self.pairing
            .open(code.clone(), "Desk".into(), Instant::now());
        code
    }

    async fn awaiting(&self) -> PairingRequest {
        until(|| {
            matches!(
                self.pairing.view(Instant::now()),
                HostView::AwaitingApproval { .. }
            )
        })
        .await;
        match self.pairing.view(Instant::now()) {
            HostView::AwaitingApproval { request, .. } => request,
            other => panic!("{other:?}"),
        }
    }

    fn outcome(&self) -> Option<Outcome> {
        match self.pairing.view(Instant::now()) {
            HostView::Ended(o) => Some(o),
            _ => None,
        }
    }
}

struct Client {
    t: Arc<MemTransport>,
    repo: InMemoryPeersRepository,
    registry: PeerRegistry,
}

impl Client {
    fn new(net: &Arc<MemNet>) -> Self {
        let repo = InMemoryPeersRepository::default();
        Self {
            t: net.endpoint(),
            registry: PeerRegistry::load(Arc::new(repo.clone())),
            repo,
        }
    }

    /// Run *Add machine* in the background.
    fn pair(&self, code: &PairingCode, name: &str) -> tokio::task::JoinHandle<Result<TrustedPeer>> {
        let (t, code, name, registry) = (
            self.t.clone(),
            code.clone(),
            name.to_string(),
            self.registry.clone(),
        );
        tokio::spawn(async move { pair_with(&*t, &code, &name, &registry, |_| {}).await })
    }
}

async fn until(cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition never became true");
}

/// AC: a valid code plus host approval pairs the two machines.
/// AC: a new peer has `view` and not `control` (on both sides).
#[tokio::test]
async fn a_valid_code_plus_approval_pairs_both_machines() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let client = Client::new(&net);
    let code = host.show_code();

    let pairing = client.pair(&code, "Laptop");
    let request = host.awaiting().await;
    assert_eq!(request.machine_name, "Laptop");
    assert_eq!(request.client, client.t.local_id());
    // Both screens show the same confirmation number.
    assert_eq!(
        request.confirmation,
        confirmation_number(code.host, client.t.local_id(), &code.secret)
    );
    // Nothing is trusted before the click.
    assert!(host.repo.snapshot().peers.is_empty());
    assert!(client.repo.snapshot().peers.is_empty());

    assert!(host.pairing.answer(request.id, true));
    let paired_host = pairing.await.unwrap().unwrap();
    assert_eq!(paired_host.machine_name, "Desk");

    until(|| host.outcome().is_some()).await;
    assert_eq!(
        host.outcome(),
        Some(Outcome::Paired {
            machine_name: "Laptop".into()
        })
    );
    let on_host = host.repo.snapshot().peers;
    assert_eq!(on_host.len(), 1);
    assert_eq!(on_host[0].id, client.t.local_id());
    assert!(on_host[0].permissions.view && !on_host[0].permissions.control);
    let on_client = client.repo.snapshot().peers;
    assert_eq!(on_client.len(), 1);
    assert_eq!(on_client[0].id, host.t.local_id());
    assert!(on_client[0].permissions.view && !on_client[0].permissions.control);
}

/// AC: declining on the host leaves both trusted lists unchanged.
#[tokio::test]
async fn declining_leaves_both_trusted_lists_unchanged() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let client = Client::new(&net);
    let code = host.show_code();

    let pairing = client.pair(&code, "Laptop");
    let request = host.awaiting().await;
    assert!(host.pairing.answer(request.id, false));
    let err = pairing.await.unwrap().unwrap_err().to_string();
    assert!(err.contains("declined"), "{err}");
    assert!(host.repo.snapshot().peers.is_empty());
    assert!(client.repo.snapshot().peers.is_empty());
    assert_eq!(host.outcome(), Some(Outcome::Declined));
}

/// A leaked code, used by someone else, gets nowhere without the click: the
/// request waits, then times out as a decline.
#[tokio::test(start_paused = true)]
async fn a_leaked_code_cannot_pair_without_the_approval_click() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let attacker = Client::new(&net);
    let code = host.show_code();

    let pairing = attacker.pair(&code, "Laptop");
    host.awaiting().await;
    // Nobody clicks. Paused time skips the two-minute wait.
    let err = pairing.await.unwrap().unwrap_err().to_string();
    assert!(err.contains("in time"), "{err}");
    assert_eq!(host.outcome(), Some(Outcome::TimedOut));
    assert!(host.repo.snapshot().peers.is_empty());
    assert!(attacker.repo.snapshot().peers.is_empty());
}

/// A guessed secret never even reaches the prompt.
#[tokio::test]
async fn a_guessed_code_never_reaches_the_approval_prompt() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let guesser = Client::new(&net);
    let real = host.show_code();
    let guess = PairingCode {
        host: real.host,
        secret: PairingSecret::generate().unwrap(),
    };
    let err = guesser.pair(&guess, "Guesser").await.unwrap().unwrap_err();
    assert!(err.to_string().contains("not valid"), "{err}");
    match host.pairing.view(Instant::now()) {
        HostView::Showing {
            failed_attempts, ..
        } => assert_eq!(failed_attempts, 1),
        other => panic!("expected the code to still be showing, got {other:?}"),
    }
    assert!(host.repo.snapshot().peers.is_empty());
}

/// AC: a code is rejected after 5 minutes — and the host does not even read
/// the late hello, because the gate closes for strangers when the code dies.
#[tokio::test(start_paused = true)]
async fn a_code_is_rejected_after_five_minutes() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let client = Client::new(&net);
    let code = host.show_code();
    tokio::time::advance(CODE_TTL + Duration::from_secs(1)).await;

    let result = client.pair(&code, "Laptop").await.unwrap();
    assert!(result.is_err());
    assert_eq!(host.outcome(), Some(Outcome::Expired));
    assert!(host.repo.snapshot().peers.is_empty());
    assert!(client.repo.snapshot().peers.is_empty());
    assert_eq!(host.t.accepted()[0].accept_stream_calls(), 0);
}

/// AC: a code is rejected on its second use.
#[tokio::test]
async fn a_code_is_rejected_on_its_second_use() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let first = Client::new(&net);
    let code = host.show_code();

    let pairing = first.pair(&code, "Laptop");
    let request = host.awaiting().await;
    host.pairing.answer(request.id, true);
    pairing.await.unwrap().unwrap();
    until(|| host.outcome().is_some()).await;

    let second = Client::new(&net);
    let result = second.pair(&code, "Other").await.unwrap();
    assert!(result.is_err(), "a used code must not pair again");
    assert_eq!(host.repo.snapshot().peers.len(), 1);
    assert!(second.repo.snapshot().peers.is_empty());
}

/// A leaked code raced by its owner: the second presentation aborts the
/// pairing, so whoever got in first cannot be approved by mistake.
#[tokio::test]
async fn two_machines_presenting_one_code_abort_the_pairing() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let attacker = Client::new(&net);
    let owner = Client::new(&net);
    let code = host.show_code();

    let a = attacker.pair(&code, "Laptop");
    let first = host.awaiting().await;
    let o = owner.pair(&code, "Laptop");
    let owner_err = o.await.unwrap().unwrap_err().to_string();
    assert!(owner_err.contains("already used"), "{owner_err}");
    // The stale prompt can no longer approve the attacker.
    assert!(!host.pairing.answer(first.id, true));
    assert!(a.await.unwrap().is_err());
    assert_eq!(host.outcome(), Some(Outcome::Contested));
    assert!(host.repo.snapshot().peers.is_empty());
    assert!(attacker.repo.snapshot().peers.is_empty());
    assert!(owner.repo.snapshot().peers.is_empty());
}

/// If the client never acknowledges, the host saves nothing.
#[tokio::test(start_paused = true)]
async fn the_host_saves_the_client_only_after_its_acknowledgement() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let code = host.show_code();
    // A hand-rolled client that stops after the approval.
    let rogue = net.endpoint();
    let conn = rogue.dial_mem(code.host).await;
    let mut stream = conn.open_stream().await.unwrap();
    super::wire::write_frame(
        &mut stream.send,
        &super::wire::Frame::PairHello {
            v: super::wire::PAIRING_VERSION,
            secret: code.secret.to_text(),
            machine_name: "Laptop".into(),
        },
    )
    .await
    .unwrap();
    let request = host.awaiting().await;
    host.pairing.answer(request.id, true);
    let reply = super::wire::read_frame(&mut stream.recv).await.unwrap();
    assert!(matches!(reply, super::wire::Frame::PairApproved { .. }));
    conn.close("gone"); // no ack
    until(|| host.outcome().is_some()).await;
    assert!(matches!(host.outcome(), Some(Outcome::Failed(_))));
    assert!(host.repo.snapshot().peers.is_empty());
}

/// AC: a connection from an id not in peers.json is refused before any
/// request is read.
#[tokio::test]
async fn an_unknown_peer_is_refused_before_anything_is_read() {
    let net = MemNet::new();
    let host = Host::start(&net);
    host.allow.store(true, std::sync::atomic::Ordering::SeqCst);
    let stranger = net.endpoint();
    let conn = stranger.dial_mem(host.t.local_id()).await;
    let reason = timeout(Duration::from_secs(2), conn.closed())
        .await
        .unwrap();
    assert_eq!(reason, listener::NOT_PAIRED_REASON);
    assert_eq!(host.t.accepted()[0].accept_stream_calls(), 0);
}

/// The switch is a real off switch: with it off, even a trusted peer is
/// turned away unread.
#[tokio::test]
async fn a_trusted_peer_is_refused_while_remote_connections_are_off() {
    let net = MemNet::new();
    let host = Host::start(&net);
    let laptop = net.endpoint();
    host.registry
        .add_paired(TrustedPeer::newly_paired(laptop.local_id(), "Laptop"))
        .unwrap();
    let conn = laptop.dial_mem(host.t.local_id()).await;
    let reason = timeout(Duration::from_secs(2), conn.closed())
        .await
        .unwrap();
    assert_eq!(reason, listener::SWITCHED_OFF_REASON);
    assert_eq!(host.t.accepted()[0].accept_stream_calls(), 0);
}

/// AC: removing a peer closes its open connection within 2 seconds.
#[tokio::test]
async fn removing_a_peer_closes_its_open_connection_within_two_seconds() {
    let net = MemNet::new();
    let host = Host::start(&net);
    host.allow.store(true, std::sync::atomic::Ordering::SeqCst);
    let laptop = net.endpoint();
    host.registry
        .add_paired(TrustedPeer::newly_paired(laptop.local_id(), "Laptop"))
        .unwrap();
    let conn = laptop.dial_mem(host.t.local_id()).await;
    until(|| host.registry.live_count(laptop.local_id()) == 1).await;
    assert!(!conn.is_closed());

    let removed_at = Instant::now();
    assert!(host.registry.remove(laptop.local_id()).unwrap());
    let reason = timeout(Duration::from_secs(2), conn.closed())
        .await
        .expect("closed within two seconds");
    assert!(removed_at.elapsed() < Duration::from_secs(2));
    assert_eq!(reason, crate::remote::registry::REVOKED_REASON);
    // And it cannot reconnect.
    let again = laptop.dial_mem(host.t.local_id()).await;
    let reason = timeout(Duration::from_secs(2), again.closed())
        .await
        .unwrap();
    assert_eq!(reason, listener::NOT_PAIRED_REASON);
}

/// Strangers cannot pile up connections while a code is showing.
#[tokio::test]
async fn unpaired_connections_past_the_cap_are_closed_at_once() {
    let net = MemNet::new();
    let host = Host::start(&net);
    host.show_code();
    let mut held = Vec::new();
    for _ in 0..MAX_UNPAIRED {
        // Connect, never send a hello.
        held.push(net.endpoint().dial_mem(host.t.local_id()).await);
    }
    until(|| host.t.accepted().len() == MAX_UNPAIRED).await;
    let extra = net.endpoint().dial_mem(host.t.local_id()).await;
    let reason = timeout(Duration::from_secs(2), extra.closed())
        .await
        .unwrap();
    assert_eq!(reason, listener::BUSY_REASON);
    assert!(held.iter().all(|c| !c.is_closed()));
}

/// An unpaired peer that connects and says nothing is dropped after the
/// hello timeout rather than held for the life of the code.
#[tokio::test(start_paused = true)]
async fn a_silent_unpaired_peer_is_dropped_after_the_hello_timeout() {
    let net = MemNet::new();
    let host = Host::start(&net);
    host.show_code();
    let conn = net.endpoint().dial_mem(host.t.local_id()).await;
    let reason = timeout(Duration::from_secs(30), conn.closed())
        .await
        .unwrap();
    assert_eq!(reason, "pairing finished");
    assert!(matches!(
        host.pairing.view(Instant::now()),
        HostView::Showing { .. }
    ));
}

// --- RemoteService: the switch and the endpoint's lifecycle ----------------

struct MemBinder(Arc<MemNet>);

#[async_trait]
impl Binder for MemBinder {
    async fn bind(&self, _: &TransportConfig) -> Result<Arc<dyn PeerTransport>> {
        Ok(self.0.endpoint())
    }
}

fn service(net: &Arc<MemNet>, allow: bool) -> (RemoteService, InMemoryPeersRepository) {
    let repo = InMemoryPeersRepository::default();
    let settings = RemoteSettings {
        allow_connections: allow,
        ..Default::default()
    };
    let svc = RemoteService::new(
        tokio::runtime::Handle::current(),
        Arc::new(repo.clone()),
        Arc::new(MemBinder(Arc::clone(net))),
        &settings,
    );
    (svc, repo)
}

/// AC (switch): turning *Allow remote connections* off closes the listener.
#[tokio::test]
async fn turning_the_switch_off_stops_the_listener() {
    let net = MemNet::new();
    let (svc, _) = service(&net, true);
    svc.reconcile_now().await;
    assert!(svc.status().listening);
    let id = svc.transport().await.unwrap().local_id();

    svc.set_allow(false);
    svc.reconcile_now().await;
    assert!(!svc.status().listening);
    // The endpoint is gone from the network.
    assert!(net.endpoint().dial(id).await.is_err());
}

#[tokio::test]
async fn the_switch_is_off_by_default_and_nothing_listens() {
    let net = MemNet::new();
    let (svc, _) = service(&net, false);
    assert!(!RemoteSettings::default().allow_connections);
    svc.reconcile_now().await;
    assert!(!svc.status().listening);
}

/// The first pairing turns the switch on; later ones leave it as the user set
/// it, so pairing a new Mac cannot quietly let a cut-off one back in.
#[tokio::test]
async fn only_the_first_pairing_turns_the_switch_on() {
    let net = MemNet::new();
    let (svc, repo) = service(&net, false);
    let pair_one = |name: &'static str| {
        let svc = svc.clone();
        let net = Arc::clone(&net);
        async move {
            svc.start_pairing("Desk".into());
            until(|| matches!(svc.status().host, HostView::Showing { .. })).await;
            let code = match svc.status().host {
                HostView::Showing { code, .. } => PairingCode::parse(&code).unwrap(),
                _ => unreachable!(),
            };
            let client = Client::new(&net);
            let task = client.pair(&code, name);
            until(|| matches!(svc.status().host, HostView::AwaitingApproval { .. })).await;
            let HostView::AwaitingApproval { request, .. } = svc.status().host else {
                unreachable!()
            };
            let answer = svc.answer(request.id, true);
            task.await.unwrap().unwrap();
            until(|| matches!(svc.status().host, HostView::Ended(_))).await;
            answer
        }
    };

    let first = pair_one("Laptop").await;
    assert!(first.answered && first.switch_turned_on);
    assert!(svc.allows_connections());

    // The user turns it off, then pairs another Mac.
    svc.set_allow(false);
    let second = pair_one("Mini").await;
    assert!(second.answered && !second.switch_turned_on);
    assert!(!svc.allows_connections());
    assert_eq!(repo.snapshot().peers.len(), 2);
}

/// Pairing works with the switch off: showing a code brings the endpoint up,
/// and it goes down again once the code is cancelled.
#[tokio::test]
async fn a_code_brings_the_endpoint_up_and_cancelling_takes_it_down() {
    let net = MemNet::new();
    let (svc, _) = service(&net, false);
    svc.start_pairing("Desk".into());
    until(|| matches!(svc.status().host, HostView::Showing { .. })).await;
    svc.reconcile_now().await;
    assert!(svc.status().listening);
    svc.cancel_pairing();
    svc.reconcile_now().await;
    assert!(!svc.status().listening);
    assert!(svc.pairing().view(Instant::now()) == HostView::Ended(Outcome::Cancelled));
}
