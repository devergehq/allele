//! The remote listener's front door (DEV-1160): who gets in, decided before
//! anything a peer sends is read.
//!
//! | peer | switch on | code live | result |
//! | -- | -- | -- | -- |
//! | trusted | yes | — | admitted, registered for revocation |
//! | any | no / not trusted | yes | may send one pairing hello (capped, timed) |
//! | any | otherwise | no | closed at once — no stream is accepted |
//!
//! What an admitted peer may *do* is DEV-1161's dispatch listener. Until it
//! lands, [`serve_trusted`] holds the connection open (so revocation has
//! something to close) and answers only a pairing hello — a paired machine
//! re-pairing after losing its own list.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::time::{timeout, Instant};

use super::pairing::host::{serve_hello, serve_unpaired, HELLO_TIMEOUT};
use super::pairing::wire::{read_frame, Frame};
use super::pairing::HostPairing;
use super::peers::TrustedPeer;
use super::registry::PeerRegistry;
use super::transport::{PeerConnection, PeerTransport};

/// Unpaired connections served at once. Past this, more are closed on arrival,
/// so strangers cannot pile up connections while a code is showing.
pub const MAX_UNPAIRED: usize = 4;

pub const NOT_PAIRED_REASON: &str = "this machine is not paired";
pub const SWITCHED_OFF_REASON: &str = "remote connections are off";
pub const BUSY_REASON: &str = "too many pairing attempts";

/// What the gate did with a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Trusted,
    Pairing,
    Refused,
}

/// The front door's inputs. Cheap to clone.
#[derive(Clone)]
pub struct Gate {
    pub registry: PeerRegistry,
    pub pairing: Arc<HostPairing>,
    /// The *Allow remote connections* switch.
    pub allow: Arc<AtomicBool>,
    unpaired: Arc<Semaphore>,
}

impl Gate {
    pub fn new(registry: PeerRegistry, pairing: Arc<HostPairing>, allow: Arc<AtomicBool>) -> Self {
        Self {
            registry,
            pairing,
            allow,
            unpaired: Arc::new(Semaphore::new(MAX_UNPAIRED)),
        }
    }

    /// Decide what to do with a freshly accepted connection and start serving
    /// it. Synchronous on purpose: the decision reads nothing from the peer.
    pub fn route(&self, conn: Arc<dyn PeerConnection>) -> Route {
        let id = conn.remote_id();
        if self.allow.load(Ordering::SeqCst) {
            if let Some(peer) = self.registry.admit(&conn) {
                tracing::info!(
                    "\u{201c}{}\u{201d} ({}) connected",
                    peer.machine_name,
                    id.short()
                );
                tokio::spawn(serve_trusted(conn, peer, self.clone()));
                return Route::Trusted;
            }
        }
        if self.pairing.accepts_presentations(Instant::now()) {
            match Arc::clone(&self.unpaired).try_acquire_owned() {
                Ok(permit) => {
                    let (pairing, registry) = (Arc::clone(&self.pairing), self.registry.clone());
                    tokio::spawn(async move {
                        serve_unpaired(conn, pairing, registry).await;
                        drop(permit);
                    });
                    return Route::Pairing;
                }
                Err(_) => {
                    tracing::warn!("refused {}: {BUSY_REASON}", id.short());
                    conn.close(BUSY_REASON);
                    return Route::Refused;
                }
            }
        }
        let reason = if self.registry.get(id).is_some() {
            SWITCHED_OFF_REASON
        } else {
            NOT_PAIRED_REASON
        };
        tracing::debug!("refused {}: {reason}", id.short());
        conn.close(reason);
        Route::Refused
    }
}

/// Accept connections until the transport shuts down.
pub async fn run(transport: Arc<dyn PeerTransport>, gate: Gate) {
    while let Some(conn) = transport.accept().await {
        gate.route(conn);
    }
}

/// Hold a trusted peer's connection until it closes, then forget it.
///
/// DEV-1161 replaces the body with the dispatch listener. Until then the only
/// request answered is a pairing hello.
async fn serve_trusted(conn: Arc<dyn PeerConnection>, peer: TrustedPeer, gate: Gate) {
    loop {
        tokio::select! {
            stream = conn.accept_stream() => {
                let Ok(mut stream) = stream else { break };
                match timeout(HELLO_TIMEOUT, read_frame(&mut stream.recv)).await {
                    Ok(Ok(frame @ Frame::PairHello { .. })) => {
                        serve_hello(&conn, stream, frame, &gate.pairing, &gate.registry).await;
                    }
                    _ => tracing::debug!(
                        "request from {} ignored: remote requests arrive with DEV-1161",
                        peer.id.short()
                    ),
                }
            }
            _ = conn.closed() => break,
        }
    }
    gate.registry.release(&conn);
    tracing::info!(
        "\u{201c}{}\u{201d} ({}) disconnected",
        peer.machine_name,
        peer.id.short()
    );
}
