//! Typed errors for Allele. Replaces ad-hoc String errors at module
//! boundaries. See ARCHITECTURE.md §3.6.
//!
//! Boundary adoption (converting git::, clone::, config::, agents::,
//! hooks:: to return `Result<T, AlleleError>` instead of
//! `anyhow::Result<T>`) is tracked as phase 17 in
//! docs/RE-DECOMPOSITION-PLAN.md and will be done incrementally.

use thiserror::Error;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Debug, Error)]
#[allow(dead_code)] // variants adopted incrementally in phase 17
pub enum AlleleError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("git error: {0}")]
    Git(String),

    #[error("clone error: {0}")]
    Clone(String),

    #[error("agent error: {0}")]
    Agent(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("hooks error: {0}")]
    Hooks(String),

    #[error("state error: {0}")]
    State(String),

    #[error("platform operation unsupported on this OS: {0}")]
    PlatformUnsupported(String),

    #[error("dispatch error: {0}")]
    Dispatch(String),

    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
pub type Result<T> = std::result::Result<T, AlleleError>;

/// Initialise structured logging. Call exactly once, early in main().
///
/// Reads the filter from `ALLELE_LOG` (default `info`). Examples:
///   ALLELE_LOG=debug               — everything above debug
///   ALLELE_LOG=allele=debug,warn   — allele's spans at debug, everyone else warn
pub(crate) fn init_tracing() {
    init_tracing_to(false);
}

/// Log to **stderr** instead of stdout.
///
/// Required by `--mcp-serve`: stdout there carries newline-delimited JSON-RPC
/// frames, and a single log line written into that stream desynchronises the
/// client's parser. MCP servers conventionally log to stderr for exactly this
/// reason.
pub(crate) fn init_tracing_stderr() {
    init_tracing_to(true);
}

fn init_tracing_to(stderr: bool) {
    let filter = EnvFilter::try_from_env("ALLELE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true);
    if stderr {
        let _ = builder.with_writer(std::io::stderr).try_init();
    } else {
        let _ = builder.try_init();
    }
}
