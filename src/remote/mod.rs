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
//! What is deliberately not here: pairing and the trusted-peers list (DEV-1160)
//! and the remote listener with its wire protocol (DEV-1161). `accept` hands
//! back *authenticated* connections — the peer proved it holds the key behind
//! `remote_id()` — but nothing here *authorises* them. That is the caller's job.

// The transport, the link and most of the identity API have no caller in the
// app until the remote listener lands (DEV-1161); this ticket ships the seam
// and the Settings → Machines pane. Remove this allow when DEV-1161 wires it.
#![allow(dead_code, unused_imports)]

pub mod identity;
pub mod iroh_transport;
pub mod link;
pub mod transport;

pub use identity::{resolve_machine_name, LocalIdentity};
pub use iroh_transport::IrohTransport;
pub use link::{LinkState, PeerLink, ReconnectPolicy};
pub use transport::{
    Heartbeat, PeerConnection, PeerId, PeerStream, PeerTransport, RelayChoice, TransportConfig,
    ALPN,
};
