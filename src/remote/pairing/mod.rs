//! Pairing two machines (DEV-1160, proposal §3.2).
//!
//! ```text
//!   host                                   client
//!   ────                                   ──────
//!   Pair a new machine → code + QR
//!   (host id + 80-bit secret, 5 min)  ───► Add machine: enter / scan code
//!                                    ◄───  dial host id (transport proves the
//!                                          host holds that key), send hello
//!   window.present: secret ok? →           {secret, name}
//!   prompt "Approve <name>?"               "Does <number> match?"
//!   with confirmation number               ─── both users check the numbers ───
//!   [Approve]                         ───► PairApproved{host name}
//!                                          [Confirm] (before or after approval)
//!                                          save host (view)
//!                                    ◄───  PairAck
//!   save client (view)
//! ```
//!
//! The properties that matter, and where each is enforced:
//!
//! | property | where |
//! | -- | -- |
//! | nothing is trusted without a click on the host | `window::PairingWindow::decide` is the only way to `Approved` |
//! | nothing is trusted without a click on the client | `client::pair_with` saves and acks only after `confirm` says yes |
//! | a code works once | `window` — the first correct presentation consumes it |
//! | a code expires after five minutes | `window::CODE_TTL` |
//! | a leaked code raced by its owner aborts | `window` — a second correct presentation → `Contested` |
//! | the user can tell their Mac from an impostor | `code::confirmation_number`, shown on both screens |
//! | guessing gets nowhere | 80-bit secret, five minutes, and a correct guess still needs the click |
//! | a broken exchange leaves the host trusting nobody new | the host saves only on `PairAck` |

pub mod client;
pub mod code;
pub mod host;
pub mod window;
pub mod wire;

pub(crate) use client::pair_with;
pub use client::{ClientProgress, CONFIRM_TIMEOUT};
pub use code::{confirmation_number, PairingCode, PairingSecret};
pub use host::{HostPairing, HostView};
pub use window::{Outcome, PairingRequest, RequestId, APPROVAL_TIMEOUT, CODE_TTL};

// Test-only: the file gates itself with `#![cfg(test)]`.
mod tests;
