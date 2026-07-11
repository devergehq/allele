//! NDJSON stream parser for Claude Code's `--output-format stream-json`.
//!
//! Two-layer design:
//!   Layer 1 — `StreamLine` / `StreamEvent`: 1:1 with the wire format (stable, serde).
//!   Layer 2 — `RichEvent`: Allele's internal representation (can evolve independently).
//!
//! The `StreamParser` transforms Layer 1 → Layer 2, accumulating partial JSON
//! for tool inputs and extracting semantic events like `EditDiff`.

mod parser;
// The ledger and adapter model are foundational APIs consumed incrementally by
// later work (narrative projection, transcript reader). Allow dead_code so each
// stacked change builds clean before its consumer lands.
#[allow(dead_code)]
mod ledger;
#[allow(dead_code)]
mod adapter;
mod types;

pub use ledger::*;
#[allow(unused_imports)]
pub use adapter::*;
pub use parser::*;
pub use types::*;
