//! The host's side of pairing: the shared window the UI reads and answers, and
//! the exchange run for each connection that presents a code.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::time::{timeout, Instant};

use super::code::{PairingCode, PairingSecret};
use super::window::{
    Outcome, PairingRequest, PairingWindow, Phase, Refusal, RequestId, APPROVAL_TIMEOUT,
};
use super::wire::{read_frame, write_frame, Frame, RefusalCode, PAIRING_VERSION};
use crate::remote::peers::TrustedPeer;
use crate::remote::registry::PeerRegistry;
use crate::remote::transport::{PeerConnection, PeerId, PeerStream};

/// How long an unpaired peer has to open its stream and send its hello, and
/// how long the host waits for the client's acknowledgement.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the host waits, after its last reply, for the client to close.
const LINGER: Duration = Duration::from_secs(5);

/// What the host's Machines pane shows about pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostView {
    Idle,
    Showing {
        code: String,
        expires_in: Duration,
        failed_attempts: u32,
    },
    AwaitingApproval {
        request: PairingRequest,
        expires_in: Duration,
    },
    Finishing {
        machine_name: String,
    },
    Ended(Outcome),
}

struct Inner {
    window: Option<PairingWindow>,
    /// The host's own name, sent to the client on approval.
    host_name: String,
    /// The open request's answer channel.
    decision: Option<(RequestId, oneshot::Sender<bool>)>,
    /// When the open request stops waiting for an answer.
    approval_deadline: Option<Instant>,
}

/// The host's pairing state, shared by the listener and the UI.
pub struct HostPairing {
    inner: Mutex<Inner>,
    changed: watch::Sender<u64>,
}

impl Default for HostPairing {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                window: None,
                host_name: String::new(),
                decision: None,
                approval_deadline: None,
            }),
            changed: watch::channel(0).0,
        }
    }
}

impl HostPairing {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Show a new code, replacing (and cancelling) any earlier one.
    pub fn open(&self, code: PairingCode, host_name: String, now: Instant) {
        let mut inner = self.lock();
        if let Some(w) = inner.window.as_mut() {
            w.close(Outcome::Cancelled);
        }
        inner.decision = None; // dropping the sender declines a pending request
        inner.approval_deadline = None;
        inner.window = Some(PairingWindow::open(code, now));
        inner.host_name = host_name;
        drop(inner);
        tracing::info!("pairing code shown; valid for five minutes");
        self.bump();
    }

    /// Stop showing the code. A pending request is declined.
    pub fn cancel(&self) {
        let mut inner = self.lock();
        if let Some(w) = inner.window.as_mut() {
            w.close(Outcome::Cancelled);
        }
        inner.decision = None;
        drop(inner);
        self.bump();
    }

    /// Clear an ended window from the UI.
    pub fn dismiss(&self) {
        let mut inner = self.lock();
        if matches!(
            inner.window.as_ref().map(PairingWindow::phase),
            Some(Phase::Closed(_))
        ) {
            inner.window = None;
        }
        drop(inner);
        self.bump();
    }

    /// Whether an unpaired peer may present a code right now.
    pub fn accepts_presentations(&self, now: Instant) -> bool {
        let mut inner = self.lock();
        let accepts = inner
            .window
            .as_mut()
            .is_some_and(|w| w.accepts_presentations(now));
        drop(inner);
        accepts
    }

    /// Whether a window is in progress (not yet ended) — the listener must run.
    pub fn in_progress(&self, now: Instant) -> bool {
        let mut inner = self.lock();
        let Some(w) = inner.window.as_mut() else {
            return false;
        };
        let expired = w.expire_if_due(now);
        let live = !matches!(w.phase(), Phase::Closed(_));
        drop(inner);
        if expired {
            self.bump();
        }
        live
    }

    /// The next moment the window changes on its own: code expiry or the
    /// approval deadline. The service sleeps until then.
    pub fn next_deadline(&self) -> Option<Instant> {
        let inner = self.lock();
        match inner.window.as_ref().map(PairingWindow::phase) {
            Some(Phase::Open) => inner.window.as_ref().map(PairingWindow::expires_at),
            Some(Phase::AwaitingApproval(_)) => inner.approval_deadline,
            _ => None,
        }
    }

    pub fn host_name(&self) -> String {
        self.lock().host_name.clone()
    }

    /// Present a code. A correct one opens a request and returns the channel
    /// the host user's answer arrives on.
    pub fn present(
        &self,
        client: PeerId,
        secret: &[u8],
        claimed_name: &str,
        now: Instant,
    ) -> Result<(PairingRequest, oneshot::Receiver<bool>), Refusal> {
        let mut inner = self.lock();
        let Some(window) = inner.window.as_mut() else {
            return Err(Refusal::Expired);
        };
        let result = window.present(client, secret, claimed_name, now);
        let out = match result {
            Ok(request) => {
                let (tx, rx) = oneshot::channel();
                inner.decision = Some((request.id, tx));
                inner.approval_deadline = Some(now + APPROVAL_TIMEOUT);
                tracing::info!(
                    "pairing request from \u{201c}{}\u{201d} ({}), confirmation {}",
                    request.machine_name,
                    request.client.short(),
                    request.confirmation
                );
                Ok((request, rx))
            }
            Err(Refusal::Contested) => {
                inner.decision = None; // the first request now reads as declined
                Err(Refusal::Contested)
            }
            Err(r) => Err(r),
        };
        drop(inner);
        self.bump();
        out
    }

    /// The host user's click. Returns whether it answered a live request.
    pub fn answer(&self, id: RequestId, approve: bool) -> bool {
        let mut inner = self.lock();
        let Some(window) = inner.window.as_mut() else {
            return false;
        };
        let Some(request) = window.decide(id, approve) else {
            return false;
        };
        tracing::info!(
            "pairing with \u{201c}{}\u{201d} ({}) {}",
            request.machine_name,
            request.client.short(),
            if approve { "approved" } else { "declined" }
        );
        if let Some((pending, tx)) = inner.decision.take() {
            if pending == id {
                let _ = tx.send(approve);
            }
        }
        drop(inner);
        self.bump();
        true
    }

    /// Record how request `id` ended, if it is still the current one.
    pub fn finish(&self, id: RequestId, outcome: Outcome) {
        let mut inner = self.lock();
        if let Some(w) = inner.window.as_mut() {
            if w.is_current(id) {
                w.close(outcome);
            }
        }
        drop(inner);
        self.bump();
    }

    pub fn view(&self, now: Instant) -> HostView {
        let mut inner = self.lock();
        let deadline = inner.approval_deadline;
        let Some(w) = inner.window.as_mut() else {
            return HostView::Idle;
        };
        w.expire_if_due(now);
        match w.phase() {
            Phase::Open => HostView::Showing {
                code: w.code().to_text(),
                expires_in: w.expires_at().saturating_duration_since(now),
                failed_attempts: w.failed_attempts(),
            },
            Phase::AwaitingApproval(r) => HostView::AwaitingApproval {
                request: r.clone(),
                expires_in: deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(now)),
            },
            Phase::Approved(r) => HostView::Finishing {
                machine_name: r.machine_name.clone(),
            },
            Phase::Closed(o) => HostView::Ended(o.clone()),
        }
    }
}

/// Serve a connection from an **unpaired** peer that the listener let through
/// because a code is live: read one hello, bounded in size and time, and run
/// the exchange. Anything else closes the connection.
pub(crate) async fn serve_unpaired(
    conn: Arc<dyn PeerConnection>,
    pairing: Arc<HostPairing>,
    registry: PeerRegistry,
) {
    let peer = conn.remote_id();
    let hello = async {
        let mut stream = conn.accept_stream().await.ok()?;
        let frame = read_frame(&mut stream.recv).await.ok()?;
        Some((stream, frame))
    };
    match timeout(HELLO_TIMEOUT, hello).await {
        Ok(Some((stream, frame))) => {
            serve_hello(&conn, stream, frame, &pairing, &registry).await;
            // Closing a QUIC connection can discard what is still in flight,
            // so after the last reply let the client hang up first.
            let _ = timeout(LINGER, conn.closed()).await;
        }
        _ => tracing::debug!("unpaired {} sent no pairing hello", peer.short()),
    }
    conn.close("pairing finished");
}

/// Run the host side of the exchange for one hello.
pub(crate) async fn serve_hello(
    conn: &Arc<dyn PeerConnection>,
    mut stream: PeerStream,
    frame: Frame,
    pairing: &HostPairing,
    registry: &PeerRegistry,
) {
    let client = conn.remote_id();
    let Frame::PairHello {
        v,
        secret,
        machine_name,
    } = frame
    else {
        tracing::debug!("{} sent a non-pairing frame first", client.short());
        return;
    };
    let refuse = |reason| Frame::PairRefused { reason };
    if v != PAIRING_VERSION {
        let _ = write_frame(&mut stream.send, &refuse(RefusalCode::Invalid)).await;
        return;
    }
    let presented = PairingSecret::decode_wire(&secret);
    let (request, decision) =
        match pairing.present(client, &presented, &machine_name, Instant::now()) {
            Ok(ok) => ok,
            Err(refusal) => {
                tracing::info!(
                    "pairing attempt from {} refused: {refusal:?}",
                    client.short()
                );
                let code = match refusal {
                    Refusal::Expired => RefusalCode::Expired,
                    Refusal::Used | Refusal::Contested => RefusalCode::Used,
                    Refusal::WrongSecret => RefusalCode::Invalid,
                };
                let _ = write_frame(&mut stream.send, &refuse(code)).await;
                return;
            }
        };

    // Wait for the human. The client going away, the timeout, and the request
    // being superseded (contested, cancelled) all count as "no".
    let approved = tokio::select! {
        answer = timeout(APPROVAL_TIMEOUT, decision) => match answer {
            Ok(Ok(yes)) => yes,
            Ok(Err(_)) => {
                let _ = write_frame(&mut stream.send, &refuse(RefusalCode::Used)).await;
                return;
            }
            Err(_) => {
                pairing.finish(request.id, Outcome::TimedOut);
                let _ = write_frame(&mut stream.send, &refuse(RefusalCode::TimedOut)).await;
                return;
            }
        },
        reason = conn.closed() => {
            pairing.finish(request.id, Outcome::Failed(format!("the other Mac disconnected ({reason})")));
            return;
        }
    };
    if !approved {
        let _ = write_frame(&mut stream.send, &refuse(RefusalCode::Declined)).await;
        return;
    }

    let approved = Frame::PairApproved {
        machine_name: pairing.host_name(),
    };
    if let Err(e) = write_frame(&mut stream.send, &approved).await {
        pairing.finish(request.id, Outcome::Failed(e.to_string()));
        return;
    }
    // Save the client only once it says it saved us.
    let ack = tokio::select! {
        ack = timeout(HELLO_TIMEOUT, read_frame(&mut stream.recv)) => matches!(ack, Ok(Ok(Frame::PairAck))),
        _ = conn.closed() => false,
    };
    match ack {
        true => {}
        false => {
            pairing.finish(
                request.id,
                Outcome::Failed("the other Mac did not confirm the pairing".into()),
            );
            return;
        }
    }
    let peer = TrustedPeer::newly_paired(client, &request.machine_name);
    let saved = registry.add_paired(peer);
    // Nothing more to say; closing tells the client its ack arrived.
    conn.close("paired");
    match saved {
        Ok(()) => {
            tracing::info!(
                "paired with \u{201c}{}\u{201d} ({}), view only",
                request.machine_name,
                client.short()
            );
            pairing.finish(
                request.id,
                Outcome::Paired {
                    machine_name: request.machine_name.clone(),
                },
            );
        }
        Err(e) => pairing.finish(request.id, Outcome::Failed(e.to_string())),
    }
}
