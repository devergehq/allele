//! The remote listener's front door (DEV-1160): who gets in, decided before
//! anything a peer sends is read.
//!
//! | peer | switch on | code live | result |
//! | -- | -- | -- | -- |
//! | trusted | yes | — | admitted, registered for revocation |
//! | any | no / not trusted | yes | may send one pairing hello (capped, timed) |
//! | any | otherwise | no | closed at once — no stream is accepted |
//!
//! An admitted peer's connection is served by [`serve_trusted`]. Each stream
//! the peer opens starts with one small frame, read under [`HELLO_TIMEOUT`]
//! and the stranger-sized limit, that says what the stream is:
//!
//! | first frame | stream | served by |
//! | -- | -- | -- |
//! | `hello` | the control stream (one per connection) | `control::serve` (DEV-1161) |
//! | `pair_hello` | a paired machine re-pairing after losing its list | `pairing::host::serve_hello` |
//! | anything else | — | the stream is dropped |
//!
//! Host-opened streams (screens, DEV-1165) never pass through here.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{timeout, Instant};

use super::control::{self, ControlContext, Ended};
use super::pairing::host::{serve_hello, serve_unpaired, HELLO_TIMEOUT};
use super::pairing::HostPairing;
use super::peers::TrustedPeer;
use super::registry::PeerRegistry;
use super::transport::{PeerConnection, PeerTransport};
use super::wire::{read_frame, write_frame, Frame, StreamRefusal};

/// Unpaired connections served at once. Past this, more are closed on arrival,
/// so strangers cannot pile up connections while a code is showing.
pub const MAX_UNPAIRED: usize = 4;

pub const NOT_PAIRED_REASON: &str = "this machine is not paired";
pub const SWITCHED_OFF_REASON: &str = "remote connections are off";
pub const BUSY_REASON: &str = "too many pairing attempts";

/// Streams one trusted connection may have open at once: the control stream,
/// a re-pairing, and slack. QUIC bounds this too; this bounds the tasks.
pub const MAX_STREAMS_PER_CONNECTION: usize = 4;

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
    /// What an admitted peer's requests are answered by.
    pub control: ControlContext,
    unpaired: Arc<Semaphore>,
}

impl Gate {
    pub fn new(
        registry: PeerRegistry,
        pairing: Arc<HostPairing>,
        allow: Arc<AtomicBool>,
        control: ControlContext,
    ) -> Self {
        Self {
            registry,
            pairing,
            allow,
            control,
            unpaired: Arc::new(Semaphore::new(MAX_UNPAIRED)),
        }
    }

    /// Decide what to do with a freshly accepted connection and start serving
    /// it. Synchronous on purpose: the decision reads nothing from the peer.
    pub fn route(&self, conn: Arc<dyn PeerConnection>) -> Route {
        let id = conn.remote_id();
        {
            if let Some(peer) = self.registry.admit(&conn, &self.allow) {
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

/// Serve a trusted peer's connection until it closes, then forget it.
async fn serve_trusted(conn: Arc<dyn PeerConnection>, peer: TrustedPeer, gate: Gate) {
    // One control stream per connection; a second is refused, not served.
    let control_open = Arc::new(AtomicBool::new(false));
    // Dropped with this function, which aborts every stream task with it.
    let mut streams = JoinSet::new();
    loop {
        tokio::select! {
            stream = conn.accept_stream() => {
                let Ok(stream) = stream else { break };
                if streams.len() >= MAX_STREAMS_PER_CONNECTION {
                    tracing::debug!("{}: too many streams; one dropped", peer.id.short());
                    continue;
                }
                let (conn, gate, open) = (Arc::clone(&conn), gate.clone(), Arc::clone(&control_open));
                streams.spawn(async move { serve_stream(&conn, stream, &gate, &open).await });
            }
            Some(_) = streams.join_next(), if !streams.is_empty() => {}
            _ = conn.closed() => break,
        }
    }
    drop(streams);
    gate.registry.release(&conn);
    tracing::info!(
        "\u{201c}{}\u{201d} ({}) disconnected",
        peer.machine_name,
        peer.id.short()
    );
}

/// Route one peer-opened stream by its first frame.
async fn serve_stream(
    conn: &Arc<dyn PeerConnection>,
    mut stream: super::transport::PeerStream,
    gate: &Gate,
    control_open: &AtomicBool,
) {
    let peer = conn.remote_id();
    match timeout(HELLO_TIMEOUT, read_frame(&mut stream.recv)).await {
        Ok(Ok(Frame::Hello { protocol, .. })) => {
            let ended = control::serve(
                conn,
                stream,
                protocol,
                &gate.registry,
                &gate.control,
                control_open,
            )
            .await;
            if let Ended::Connection(reason) = ended {
                control::close_after_linger(conn, &reason).await;
            }
        }
        Ok(Ok(frame @ Frame::PairHello { .. })) => {
            serve_hello(conn, stream, frame, &gate.pairing, &gate.registry).await;
        }
        Ok(Ok(other)) => {
            tracing::debug!("{} opened a stream with {other:?}; refused", peer.short());
            let refused = Frame::Refused {
                code: StreamRefusal::BadHello,
                message: "a stream must start with a hello".into(),
            };
            let _ = write_frame(&mut stream.send, &refused).await;
            let _ = stream.send.shutdown().await;
        }
        _ => tracing::debug!("{} opened a stream and sent no hello", peer.short()),
    }
}
