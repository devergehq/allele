//! Live status feed: what a paired client needs to answer "does anything on
//! that Mac need me?" (DEV-1162, proposal §3.5 / §3.9).
//!
//! Two halves, joined only by the plain types in [`model`]:
//!
//! - **Host** ([`host`]): [`StatusFeed`] holds the last published view of every
//!   session. `AppState` republishes into it whenever state is marked dirty and
//!   after each batch of hook events. The publisher diffs the new view against
//!   the old one and emits a [`StatusDelta`] per change, so no call site has to
//!   know which delta it caused and nothing polls.
//! - **Client** ([`client`]): [`RemoteHostState`] is one paired host as seen
//!   from here. It applies a snapshot (replacing everything), then deltas in
//!   sequence order, and keeps the last-known sessions while the host is
//!   unreachable.
//!
//! Deliberately independent of `crate::remote`: the transport maps these
//! types onto its wire frames; nothing here knows there is a network.

pub(crate) mod client;
pub(crate) mod diff;
pub(crate) mod host;
pub(crate) mod model;

pub(crate) use client::{DeltaOutcome, RemoteHostState, Transition};
pub(crate) use host::StatusFeed;
pub(crate) use model::{SessionEntry, StatusChange, StatusDelta, StatusSnapshot};
