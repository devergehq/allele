//! Mirroring a terminal screen as state, not bytes (DEV-1165).
//!
//! alacritty's `EventLoop` reads the PTY and feeds `Term` directly, so Allele
//! never sees the output stream. Instead the host copies the `Term` grid
//! ([`capture`]), turns successive copies into a frame stream
//! ([`DiffProducer`]): one [`ScreenSnapshot`] then [`RowDiff`]s, merged while
//! the consumer is behind. The client rebuilds the screen in a
//! [`MirroredGrid`] and draws it with the same grid element as a local
//! terminal (see `super::grid_source`).
//!
//! This module is transport-free on purpose: it knows nothing of
//! `crate::remote`. The remote protocol carries [`ScreenFrame`]s; it does not
//! shape them.

// Nothing outside the tests consumes the mirror until the remote protocol
// wires attach/detach in (DEV-1165 stage B). Remove this then.
#![allow(dead_code, unused_imports)]

pub mod capture;
pub mod grid;
pub mod model;
pub mod producer;
pub mod source;

#[cfg(test)]
mod tests;

pub use capture::{capture, capture_history, Capture};
pub use grid::MirroredGrid;
pub use model::{RowDiff, ScreenFrame, ScreenSnapshot, ScrollbackPage, WireRow};
pub use producer::{DiffProducer, Poll, MIN_FRAME_INTERVAL};
pub use source::{MirrorTerm, SharedTerm};
