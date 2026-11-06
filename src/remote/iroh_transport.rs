//! `PeerTransport` over iroh 1.4.1 — QUIC dialed by public key, with
//! hole-punching and relay fallback.
//!
//! **This is the only file in the crate that may name `iroh`**
//! (`tests/architecture.rs`). Everything iroh-shaped — key derivation, id
//! parsing, relay URLs, QUIC tuning — is translated to crate types here.
//!
//! Facts from the DEV-1158 spike that shaped this file:
//! - Dialing by a bare id needs n0's address lookup (DNS/pkarr), which the
//!   `N0` preset provides. A relay-less mode is not just a config switch.
//! - A redial after the peer restarts waits behind the dead connection until
//!   QUIC's idle timeout; hence the short [`Heartbeat`] and `PeerLink`'s
//!   close-before-redial.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use iroh::endpoint::{presets, Builder, Connection, IdleTimeout, QuicTransportConfig, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};
use tokio::sync::mpsc;

use crate::errors::{AlleleError, Result};

use super::identity::LocalIdentity;
use super::transport::{
    Heartbeat, PeerConnection, PeerId, PeerStream, PeerTransport, RelayChoice, TransportConfig,
    ALPN,
};

fn remote_err(context: &str, e: impl std::fmt::Display) -> AlleleError {
    AlleleError::Remote(format!("{context}: {e}"))
}

// ── key and id helpers, used by `identity` and `transport` ────────────────

/// 32 fresh secret-key bytes from the OS RNG.
pub(super) fn generate_secret() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// The public id for a secret key.
pub(super) fn public_id(secret: &[u8; 32]) -> PeerId {
    peer_id(SecretKey::from_bytes(secret).public())
}

/// Parse a peer id from hex (or base32), checking it is a real public key.
pub(super) fn parse_peer_id(s: &str) -> Result<PeerId> {
    s.parse::<EndpointId>()
        .map(peer_id)
        .map_err(|e| remote_err("not a machine id", e))
}

/// Check a relay URL is one iroh can use: `http(s)://host[:port]`.
///
/// Parsing alone is not enough — `relay.example.com:443` parses, with
/// `relay.example.com` as the *scheme*, and would bind an endpoint that never
/// comes online.
pub(super) fn validate_relay_url(url: &str) -> Result<()> {
    parse_relay_url(url).map(|_| ())
}

fn parse_relay_url(url: &str) -> Result<RelayUrl> {
    let parsed = url
        .parse::<RelayUrl>()
        .map_err(|e| remote_err(&format!("invalid relay URL {url:?}"), e))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(AlleleError::Remote(format!(
            "invalid relay URL {url:?}: expected http(s)://host[:port]"
        )));
    }
    Ok(parsed)
}

fn peer_id(id: EndpointId) -> PeerId {
    PeerId::from_validated(*id.as_bytes())
}

fn endpoint_id(peer: PeerId) -> Result<EndpointId> {
    EndpointId::from_bytes(peer.as_bytes()).map_err(|e| remote_err("invalid machine id", e))
}

fn relay_mode(choice: &RelayChoice) -> Result<RelayMode> {
    Ok(match choice {
        RelayChoice::N0 => RelayMode::Default,
        RelayChoice::Custom(url) => RelayMode::custom([parse_relay_url(url)?]),
    })
}

fn quic_config(heartbeat: &Heartbeat) -> Result<QuicTransportConfig> {
    let idle: IdleTimeout = heartbeat
        .idle_timeout
        .try_into()
        .map_err(|e| remote_err("idle timeout out of range", e))?;
    // Starts from iroh's own tuned defaults (multipath, NAT traversal) and
    // overrides only the heartbeat.
    Ok(QuicTransportConfig::builder()
        .keep_alive_interval(heartbeat.interval)
        .max_idle_timeout(Some(idle))
        .build())
}

// ── transport ─────────────────────────────────────────────────────────────

/// A bound iroh endpoint. Cheap to share: wrap it in an `Arc` and hand it out
/// as `Arc<dyn PeerTransport>`.
pub struct IrohTransport {
    endpoint: Endpoint,
    /// Finished handshakes, fed by a task started on the first `accept`. Each
    /// handshake runs in its own task, so one peer that stalls mid-handshake
    /// cannot hold up everyone else. Never started on a dial-only endpoint.
    incoming: tokio::sync::OnceCell<tokio::sync::Mutex<mpsc::Receiver<Connection>>>,
}

/// Handshakes finished but not yet taken by `accept`. Past this, the accept
/// loop waits — backpressure, rather than an unbounded queue of strangers.
const ACCEPT_BACKLOG: usize = 16;

impl IrohTransport {
    /// Bind an endpoint for `identity`, using n0's address lookup and the
    /// relay and heartbeat in `config`. Must run inside a tokio runtime, and
    /// the endpoint lives on that runtime.
    ///
    /// The relay is read here, once. A changed relay setting takes effect the
    /// next time the transport is bound (DEV-1161 rebinds on change).
    pub async fn bind(identity: &LocalIdentity, config: &TransportConfig) -> Result<Self> {
        Self::bind_from(Endpoint::builder(presets::N0), identity, config).await
    }

    async fn bind_from(
        builder: Builder,
        identity: &LocalIdentity,
        config: &TransportConfig,
    ) -> Result<Self> {
        let endpoint = builder
            .secret_key(SecretKey::from_bytes(identity.secret().expose()))
            .alpns(vec![ALPN.to_vec()])
            .relay_mode(relay_mode(&config.relay)?)
            .transport_config(quic_config(&config.heartbeat)?)
            .bind()
            .await
            .map_err(|e| remote_err("could not start the network endpoint", e))?;
        tracing::info!(
            "remote: endpoint {} bound ({:?})",
            peer_id(endpoint.id()).short(),
            config.relay
        );
        Ok(Self {
            endpoint,
            incoming: tokio::sync::OnceCell::new(),
        })
    }

    /// Wait until the endpoint is reachable (connected to its home relay),
    /// for at most `within`. `false` if it is not online by then.
    pub async fn wait_online(&self, within: Duration) -> bool {
        tokio::time::timeout(within, self.endpoint.online())
            .await
            .is_ok()
    }

    /// The relay URLs this endpoint currently advertises — its home relay
    /// once [`Self::wait_online`] succeeds.
    pub fn relay_urls(&self) -> Vec<String> {
        self.endpoint
            .addr()
            .relay_urls()
            .map(|u| u.to_string())
            .collect()
    }
}

#[async_trait]
impl PeerTransport for IrohTransport {
    fn local_id(&self) -> PeerId {
        peer_id(self.endpoint.id())
    }

    async fn dial(&self, peer: PeerId) -> Result<Arc<dyn PeerConnection>> {
        let conn = self
            .endpoint
            .connect(EndpointAddr::new(endpoint_id(peer)?), ALPN)
            .await
            .map_err(|e| remote_err(&format!("could not reach {}", peer.short()), e))?;
        Ok(Arc::new(IrohConnection::new(conn)))
    }

    async fn accept(&self) -> Option<Arc<dyn PeerConnection>> {
        let rx = self
            .incoming
            .get_or_init(|| async {
                tokio::sync::Mutex::new(spawn_accept_loop(self.endpoint.clone()))
            })
            .await;
        let conn = rx.lock().await.recv().await?;
        Some(Arc::new(IrohConnection::new(conn)))
    }

    async fn network_changed(&self) {
        self.endpoint.network_change().await;
    }

    async fn shutdown(&self) {
        self.endpoint.close().await;
    }
}

/// Accept every incoming connection and finish each handshake in its own
/// task, forwarding the ones that complete. A failed handshake is logged and
/// dropped; it never ends the loop. Stops when the endpoint closes.
fn spawn_accept_loop(endpoint: Endpoint) -> mpsc::Receiver<Connection> {
    let (tx, rx) = mpsc::channel(ACCEPT_BACKLOG);
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        let _ = tx.send(conn).await;
                    }
                    Err(e) => tracing::debug!("remote: incoming handshake failed: {e}"),
                }
            });
        }
    });
    rx
}

// ── connection ────────────────────────────────────────────────────────────

struct IrohConnection {
    conn: Connection,
    remote: PeerId,
}

impl IrohConnection {
    fn new(conn: Connection) -> Self {
        let remote = peer_id(conn.remote_id());
        Self { conn, remote }
    }
}

#[async_trait]
impl PeerConnection for IrohConnection {
    fn remote_id(&self) -> PeerId {
        self.remote
    }

    async fn open_stream(&self) -> Result<PeerStream> {
        let (send, recv) = self
            .conn
            .open_bi()
            .await
            .map_err(|e| remote_err("could not open a stream", e))?;
        Ok(PeerStream {
            send: Box::new(send),
            recv: Box::new(recv),
        })
    }

    async fn accept_stream(&self) -> Result<PeerStream> {
        let (send, recv) = self
            .conn
            .accept_bi()
            .await
            .map_err(|e| remote_err("could not accept a stream", e))?;
        Ok(PeerStream {
            send: Box::new(send),
            recv: Box::new(recv),
        })
    }

    async fn closed(&self) -> String {
        self.conn.closed().await.to_string()
    }

    fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    fn close(&self, reason: &str) {
        self.conn.close(VarInt::from_u32(0), reason.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end over real iroh endpoints on loopback, with a local relay
    //! server and an in-memory address book standing in for n0's — so these
    //! run offline, in CI, in a few seconds each.

    use super::*;
    use crate::remote::link::{LinkState, PeerLink, ReconnectPolicy};
    use iroh::address_lookup::MemoryLookup;
    use iroh::tls::CaTlsConfig;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::runtime::Runtime;

    fn rt() -> Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn config(relay_url: &RelayUrl) -> TransportConfig {
        TransportConfig {
            relay: RelayChoice::Custom(relay_url.to_string()),
            heartbeat: Heartbeat::default(),
        }
    }

    /// Bind like production, but trusting the test relay's self-signed
    /// certificate and finding peers in `book` instead of n0's DNS.
    async fn bind(
        identity: &LocalIdentity,
        relay_url: &RelayUrl,
        book: &Arc<MemoryLookup>,
    ) -> IrohTransport {
        let builder = Endpoint::builder(presets::Minimal)
            .ca_tls_config(CaTlsConfig::insecure_skip_verify())
            .address_lookup(Arc::clone(book));
        let t = IrohTransport::bind_from(builder, identity, &config(relay_url))
            .await
            .unwrap();
        assert!(t.wait_online(Duration::from_secs(10)).await, "never online");
        book.add_endpoint_info(t.endpoint.addr());
        t
    }

    /// Accept connections forever, echoing every stream.
    fn serve_echo(t: Arc<IrohTransport>) {
        tokio::spawn(async move {
            while let Some(conn) = t.accept().await {
                tokio::spawn(async move {
                    while let Ok(mut s) = conn.accept_stream().await {
                        tokio::spawn(async move {
                            let _ = tokio::io::copy(&mut s.recv, &mut s.send).await;
                        });
                    }
                });
            }
        });
    }

    async fn echo(conn: &dyn PeerConnection, msg: &[u8]) -> Vec<u8> {
        let mut s = conn.open_stream().await.unwrap();
        s.send.write_all(msg).await.unwrap();
        let mut buf = vec![0u8; msg.len()];
        tokio::time::timeout(Duration::from_secs(5), s.recv.read_exact(&mut buf))
            .await
            .expect("echo timed out")
            .unwrap();
        buf
    }

    #[test]
    fn ids_round_trip_between_crate_and_iroh_types() {
        let secret = generate_secret();
        let id = public_id(&secret);
        assert_eq!(
            id.to_string(),
            SecretKey::from_bytes(&secret).public().to_string()
        );
        assert_eq!(parse_peer_id(&id.to_string()).unwrap(), id);
        assert_eq!(peer_id(endpoint_id(id).unwrap()), id);
    }

    #[test]
    fn n0_relay_choice_maps_to_n0_relays() {
        assert_eq!(relay_mode(&RelayChoice::N0).unwrap(), RelayMode::Default);
    }

    #[test]
    fn relay_urls_need_an_http_scheme_and_a_host() {
        assert!(validate_relay_url("https://relay.example.com").is_ok());
        assert!(validate_relay_url("http://localhost:3340").is_ok());
        // These parse as URLs, with the host read as the scheme.
        assert!(validate_relay_url("relay.example.com:443").is_err());
        assert!(validate_relay_url("localhost:3340").is_err());
        assert!(validate_relay_url("ftp://relay.example.com").is_err());
    }

    #[test]
    fn custom_relay_choice_maps_to_exactly_that_relay() {
        let url = "https://relay.example.com";
        let mode = relay_mode(&RelayChoice::Custom(url.into())).unwrap();
        assert_eq!(mode, RelayMode::custom([url.parse::<RelayUrl>().unwrap()]));
    }

    #[test]
    fn heartbeat_is_applied_to_the_quic_config() {
        // Builds without error for the defaults; an absurd idle timeout is refused.
        quic_config(&Heartbeat::default()).unwrap();
        let absurd = Heartbeat {
            interval: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(u64::MAX / 2),
        };
        assert!(quic_config(&absurd).is_err());
    }

    #[test]
    fn same_identity_binds_to_the_same_id() {
        let rt = rt();
        rt.block_on(async {
            let (_map, relay_url, _relay) = iroh::test_utils::run_relay_server().await.unwrap();
            let book = Arc::new(MemoryLookup::new());
            let me = LocalIdentity::ephemeral();
            let a = bind(&me, &relay_url, &book).await;
            assert_eq!(a.local_id(), me.id());
            a.shutdown().await;
            // "Restart": a fresh endpoint from the same identity.
            let b = bind(&me, &relay_url, &book).await;
            assert_eq!(b.local_id(), me.id());
            b.shutdown().await;
        });
    }

    #[test]
    fn the_relay_setting_is_the_relay_the_endpoint_uses() {
        let rt = rt();
        rt.block_on(async {
            let (_map, relay_url, _relay) = iroh::test_utils::run_relay_server().await.unwrap();
            let book = Arc::new(MemoryLookup::new());
            let t = bind(&LocalIdentity::ephemeral(), &relay_url, &book).await;
            assert_eq!(t.relay_urls(), vec![relay_url.to_string()]);
            t.shutdown().await;
        });
    }

    #[test]
    fn dial_by_id_and_echo_a_stream() {
        let rt = rt();
        rt.block_on(async {
            let (_map, relay_url, _relay) = iroh::test_utils::run_relay_server().await.unwrap();
            let book = Arc::new(MemoryLookup::new());
            let server = Arc::new(bind(&LocalIdentity::ephemeral(), &relay_url, &book).await);
            let client = bind(&LocalIdentity::ephemeral(), &relay_url, &book).await;
            serve_echo(Arc::clone(&server));

            let conn = client.dial(server.local_id()).await.unwrap();
            assert_eq!(conn.remote_id(), server.local_id());
            assert_eq!(echo(&*conn, b"hello").await, b"hello");

            conn.close("done");
            assert!(conn.is_closed());
            client.shutdown().await;
            server.shutdown().await;
        });
    }

    /// The peer process dies without a goodbye: its runtime is torn down, so
    /// it sends nothing more. Only the heartbeat can notice.
    fn kill(server_rt: Runtime, server: Arc<IrohTransport>) {
        server_rt.shutdown_background();
        // Its endpoint must not run Drop on a runtime that no longer exists.
        std::mem::forget(server);
    }

    fn start_server(
        identity: &LocalIdentity,
        relay_url: &RelayUrl,
        book: &Arc<MemoryLookup>,
    ) -> (Runtime, Arc<IrohTransport>) {
        let server_rt = rt();
        let server = server_rt.block_on(async {
            let s = Arc::new(bind(identity, relay_url, book).await);
            serve_echo(Arc::clone(&s));
            s
        });
        (server_rt, server)
    }

    #[test]
    fn a_dead_peer_is_detected_within_fifteen_seconds() {
        let client_rt = rt();
        let (_map, relay_url, _relay) = client_rt
            .block_on(iroh::test_utils::run_relay_server())
            .unwrap();
        let book = Arc::new(MemoryLookup::new());
        let server_identity = LocalIdentity::ephemeral();
        let (server_rt, server) = start_server(&server_identity, &relay_url, &book);

        let client = client_rt.block_on(bind(&LocalIdentity::ephemeral(), &relay_url, &book));
        let link = PeerLink::spawn(
            client_rt.handle(),
            Arc::new(client),
            server_identity.id(),
            ReconnectPolicy::default(),
        );
        client_rt.block_on(async {
            let conn = link.wait_connected().await.unwrap();
            assert_eq!(echo(&*conn, b"up").await, b"up");
        });

        let died = Instant::now();
        kill(server_rt, server);
        client_rt.block_on(async {
            let mut rx = link.subscribe();
            tokio::time::timeout(Duration::from_secs(15), async {
                while rx.borrow_and_update().is_connected() {
                    rx.changed().await.unwrap();
                }
            })
            .await
            .expect("dead peer still looked connected after 15 s");
        });
        let detected = died.elapsed();
        assert!(detected < Duration::from_secs(15), "took {detected:?}");
        eprintln!("dead peer detected in {detected:?}");
    }

    #[test]
    fn a_restarted_peer_is_redialed_within_ten_seconds() {
        let client_rt = rt();
        let (_map, relay_url, _relay) = client_rt
            .block_on(iroh::test_utils::run_relay_server())
            .unwrap();
        let book = Arc::new(MemoryLookup::new());
        let server_identity = LocalIdentity::ephemeral();
        let (server_rt, server) = start_server(&server_identity, &relay_url, &book);

        let client = client_rt.block_on(bind(&LocalIdentity::ephemeral(), &relay_url, &book));
        let link = PeerLink::spawn(
            client_rt.handle(),
            Arc::new(client),
            server_identity.id(),
            ReconnectPolicy::default(),
        );
        let first = client_rt.block_on(link.wait_connected()).unwrap();
        client_rt.block_on(async { assert_eq!(echo(&*first, b"one").await, b"one") });

        // Kill and restart with the same identity: same id, new port.
        kill(server_rt, server);
        let (server_rt, server) = start_server(&server_identity, &relay_url, &book);
        let restarted = Instant::now();

        client_rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut rx = link.subscribe();
                loop {
                    if let LinkState::Connected(c) = &*rx.borrow_and_update() {
                        if !Arc::ptr_eq(c, &first) {
                            break;
                        }
                    }
                    rx.changed().await.unwrap();
                }
            })
            .await
            .expect("not redialed within 10 s of the restart");
            let conn = link.connection().unwrap();
            assert_eq!(echo(&*conn, b"two").await, b"two");
        });
        let took = restarted.elapsed();
        assert!(took < Duration::from_secs(10), "took {took:?}");
        eprintln!("restarted peer redialed in {took:?}");

        client_rt.block_on(link.shutdown());
        server_rt.block_on(server.shutdown());
    }
}
