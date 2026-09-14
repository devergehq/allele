//! Application startup wiring (DEV-624).
//!
//! `fn main()` had accumulated 1,088 lines of setup: window creation, polling
//! tasks, menu construction and action registration. None of it is logic, all
//! of it is wiring, and it crowded out the file it lived in — which had regrown
//! to 4,922 lines against a 5,000-line ratchet after a decomposition that was
//! already recorded as finished.

pub(crate) mod actions;
pub(crate) mod pollers;
