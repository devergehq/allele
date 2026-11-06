//! Multi-machine transport: who this Allele is on the network, and how it
//! reaches another one (DEV-1159, `Plans/MULTI-MACHINE-PROPOSAL.md` §3.1–3.2).
//!
//! ```text
//!   identity ──► LocalIdentity { PeerId, secret }   secret lives in the Keychain,
//!                                                    one per data root
//!   transport ─► PeerTransport / PeerConnection      the seam everything above uses
//!   link ──────► PeerLink                            keeps one peer connected:
//!                                                    heartbeat, close-stale, redial
//!   iroh_transport ► IrohTransport                   the only implementation, and the
//!                                                    only file that may name `iroh`
//! ```
//!
//! **Nothing outside `iroh_transport.rs` names the `iroh` crate.**
//! `tests/architecture.rs` enforces it, so a later adapter (Tailscale, plain
//! LAN) or a test double can stand in without touching callers.
//!
//! Trust (DEV-1160) sits on top:
//!
//! ```text
//!   peers / registry ► TrustedPeer, PeerRegistry     peers.json (0600) + open
//!                                                    connections, one lock
//!   pairing ─────────► code, window, host, client    one-time code + host approval
//!   listener ────────► Gate                          who gets in, decided before
//!                                                    anything is read
//!   service ─────────► RemoteService                 binds the endpoint only while
//!                                                    something needs it
//! ```
//!
//! `accept` hands back *authenticated* connections — the peer proved it holds
//! the key behind `remote_id()`. [`listener::Gate`] is what *authorises* them,
//! against the trusted list. What an admitted peer may do is DEV-1161.

// The link and parts of the transport API have no caller in the app until the
// dispatch listener lands (DEV-1161). Remove this allow when DEV-1161 wires it.
#![allow(dead_code, unused_imports)]

pub mod identity;
pub mod iroh_transport;
pub mod link;
pub mod listener;
pub mod pairing;
pub mod peers;
pub mod registry;
pub mod service;
// Test-only: the file gates itself with `#![cfg(test)]`.
pub(crate) mod testing;
pub mod transport;

pub use identity::{resolve_machine_name, LocalIdentity};
pub use iroh_transport::IrohTransport;
pub use link::{LinkState, PeerLink, ReconnectPolicy};
pub use peers::{PeerList, PeerPermissions, TrustedPeer};
pub use registry::PeerRegistry;
pub use service::{ClientView, PeerRow, RemoteService, RemoteStatus};
pub use transport::{
    Heartbeat, PeerConnection, PeerId, PeerStream, PeerTransport, RelayChoice, TransportConfig,
    ALPN,
};
