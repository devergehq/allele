//! NDJSON stream parser for Claude Code's `--output-format stream-json`.
//!
//! Two-layer design:
//!   Layer 1 — `StreamLine` / `StreamEvent`: 1:1 with the wire format (stable, serde).
//!   Layer 2 — `RichEvent`: Allele's internal representation (can evolve independently).
//!
//! The `StreamParser` transforms Layer 1 → Layer 2, accumulating partial JSON
//! for tool inputs and extracting semantic events like `EditDiff`.

mod types;
mod parser;

pub use types::*;
pub use parser::*;
