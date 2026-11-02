//! The `PeerTransport` seam: dial a peer, accept a peer, open a stream.
//!
//! Everything above the transport speaks these traits and never a concrete
//! network library, so `IrohTransport` can be swapped or faked. The traits are
//! object-safe: callers hold an `Arc<dyn PeerTransport>`.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::errors::{AlleleError, Result};
use crate::settings::RemoteSettings;

/// The one application protocol spoken between Allele instances. Bump the
/// number on any breaking change to the wire protocol (proposal §3.1).
pub const ALPN: &[u8] = b"allele/remote/1";

/// A peer's address on the network: its Ed25519 public key.
///
/// The transport authenticates it during the handshake, so a connection's
/// `remote_id()` is proven, not claimed. Displays as 64 lowercase hex digits
/// (the same text iroh prints for an `EndpointId`) and parses from that or
/// base32. Construction always goes through validation — any `PeerId` value is
/// a real public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId([u8; 32]);

impl PeerId {
    /// Wrap bytes the transport has already validated as a public key.
    pub(super) fn from_validated(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw public-key bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The first ten hex digits — enough to tell machines apart in a log line
    /// or a UI label, not enough to dial.
    pub fn short(&self) -> String {
        self.to_string()[..10].to_string()
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({})", self.short())
    }
}

impl FromStr for PeerId {
    type Err = AlleleError;

    fn from_str(s: &str) -> Result<Self> {
        super::iroh_transport::parse_peer_id(s.trim())
    }
}

impl serde::Serialize for PeerId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for PeerId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One bidirectional, ordered, reliable byte stream inside a connection.
///
/// A stream opened with [`PeerConnection::open_stream`] becomes visible to the
/// peer's [`PeerConnection::accept_stream`] only once the opener writes to it.
pub struct PeerStream {
    pub send: Box<dyn AsyncWrite + Send + Unpin>,
    pub recv: Box<dyn AsyncRead + Send + Unpin>,
}

/// An authenticated, encrypted connection to one peer.
#[async_trait]
pub trait PeerConnection: Send + Sync {
    /// The peer's id, proven by the handshake.
    fn remote_id(&self) -> PeerId;

    /// Open a new stream to the peer.
    async fn open_stream(&self) -> Result<PeerStream>;

    /// Wait for the peer to open a stream.
    async fn accept_stream(&self) -> Result<PeerStream>;

    /// Resolve once the connection is closed — by us, by the peer, or by the
    /// heartbeat giving up on a silent peer — with a human-readable reason.
    async fn closed(&self) -> String;

    /// True once the connection is closed for any reason.
    #[allow(dead_code)] // first caller: the client link (DEV-1162)
    fn is_closed(&self) -> bool;

    /// Close the connection now. Idempotent.
    fn close(&self, reason: &str);
}

/// A bound local endpoint that can dial and accept peers.
#[async_trait]
pub trait PeerTransport: Send + Sync {
    /// This endpoint's own id — what a peer dials to reach us.
    fn local_id(&self) -> PeerId;

    /// Connect to `peer`, by id alone; the transport finds a path.
    async fn dial(&self, peer: PeerId) -> Result<Arc<dyn PeerConnection>>;

    /// The next incoming connection whose handshake completed, or `None` once
    /// the transport is shut down. Handshakes run concurrently, and one that
    /// fails is dropped rather than returned. The peer is **authenticated, not
    /// authorised**: check `remote_id()` against the trusted peers before
    /// serving it (DEV-1160).
    async fn accept(&self) -> Option<Arc<dyn PeerConnection>>;

    /// Tell the transport the network changed (interface switch, wake from
    /// sleep) so it re-probes paths now instead of waiting to notice.
    #[allow(dead_code)] // first caller: the client link (DEV-1162)
    async fn network_changed(&self);

    /// Close every connection and release the endpoint.
    async fn shutdown(&self);
}

/// Which relay servers the endpoint uses when no direct path exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayChoice {
    /// n0's public relays (the default).
    N0,
    /// A self-hosted `iroh-relay`, by URL. Always validated.
    Custom(String),
}

impl RelayChoice {
    /// Read the relay setting: blank or absent means n0's relays; anything
    /// else must be a valid relay URL, or this is an error rather than a
    /// silent fall-back to n0.
    pub fn from_setting(url: Option<&str>) -> Result<Self> {
        match url.map(str::trim).filter(|u| !u.is_empty()) {
            None => Ok(Self::N0),
            Some(u) => {
                super::iroh_transport::validate_relay_url(u)?;
                Ok(Self::Custom(u.to_string()))
            }
        }
    }
}

/// The heartbeat: how often an idle connection pings, and how long silence
/// lasts before the connection is declared dead.
///
/// Deliberately short. iroh's defaults (5 s ping, ~30 s idle) left a peer
/// that restarted unreachable for 34 s in the DEV-1158 spike. With these, a
/// dead peer is noticed within [`Heartbeat::idle_timeout`], and [`PeerLink`]
/// redials as soon as it is.
///
/// [`PeerLink`]: super::link::PeerLink
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    /// Ping an otherwise idle connection this often.
    pub interval: Duration,
    /// Declare the connection dead after this long with no reply. Must exceed
    /// `interval` by a few pings so one lost packet does not drop the link.
    pub idle_timeout: Duration,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(6),
        }
    }
}

/// Everything needed to bind a transport, other than the identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportConfig {
    pub relay: RelayChoice,
    pub heartbeat: Heartbeat,
}

impl TransportConfig {
    /// Build from the user's settings. Errors on an invalid relay URL.
    pub fn from_settings(settings: &RemoteSettings) -> Result<Self> {
        Ok(Self {
            relay: RelayChoice::from_setting(settings.relay_url.as_deref())?,
            heartbeat: Heartbeat::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";

    #[test]
    fn peer_id_round_trips_through_text() {
        let id: PeerId = ID.parse().unwrap();
        assert_eq!(id.to_string(), ID);
        assert_eq!(id.short(), &ID[..10]);
    }

    #[test]
    fn peer_id_rejects_garbage() {
        assert!("not-a-key".parse::<PeerId>().is_err());
        assert!(ID[..62].parse::<PeerId>().is_err());
    }

    #[test]
    fn peer_id_serialises_as_a_string() {
        let id: PeerId = ID.parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{ID}\""));
        assert_eq!(serde_json::from_str::<PeerId>(&json).unwrap(), id);
    }

    #[test]
    fn blank_relay_setting_means_n0() {
        assert_eq!(RelayChoice::from_setting(None).unwrap(), RelayChoice::N0);
        assert_eq!(
            RelayChoice::from_setting(Some("  ")).unwrap(),
            RelayChoice::N0
        );
    }

    #[test]
    fn host_port_without_a_scheme_is_rejected() {
        assert!(RelayChoice::from_setting(Some("relay.example.com:443")).is_err());
    }

    #[test]
    fn custom_relay_setting_is_kept() {
        let choice = RelayChoice::from_setting(Some(" https://relay.example.com ")).unwrap();
        assert_eq!(
            choice,
            RelayChoice::Custom("https://relay.example.com".into())
        );
    }

    #[test]
    fn invalid_relay_setting_is_an_error_not_a_fallback() {
        let err = RelayChoice::from_setting(Some("not a url")).unwrap_err();
        assert!(matches!(err, AlleleError::Remote(_)), "{err:?}");
    }

    #[test]
    fn config_reads_the_relay_from_settings() {
        let settings = RemoteSettings {
            relay_url: Some("https://relay.example.com".into()),
            ..Default::default()
        };
        let cfg = TransportConfig::from_settings(&settings).unwrap();
        assert_eq!(
            cfg.relay,
            RelayChoice::Custom("https://relay.example.com".into())
        );
        assert_eq!(cfg.heartbeat, Heartbeat::default());
    }

    #[test]
    fn heartbeat_detects_a_dead_peer_well_inside_fifteen_seconds() {
        let hb = Heartbeat::default();
        assert!(hb.idle_timeout <= Duration::from_secs(10));
        // At least two pings fit in the idle window, so one lost ping is survivable.
        assert!(hb.idle_timeout >= hb.interval * 3);
    }
}
