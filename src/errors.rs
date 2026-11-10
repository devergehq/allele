//! Typed errors for Allele. Replaces ad-hoc String errors at module
//! boundaries. See ARCHITECTURE.md §3.6.
//!
//! Boundary adoption (converting git::, clone::, config::, agents::,
//! hooks:: to return `Result<T, AlleleError>` instead of
//! `anyhow::Result<T>`) is tracked as phase 17 in
//! docs/RE-DECOMPOSITION-PLAN.md and will be done incrementally.

use thiserror::Error;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
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

    /// Multi-machine transport and identity (`crate::remote`, DEV-1159).
    #[error("remote error: {0}")]
    Remote(String),

    #[error("screen mirror error: {0}")]
    Mirror(String),

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
///
/// Writes to stdout and to `~/.allele/logs/allele.log` (rotated, see
/// [`crate::log_file`]): stdout is discarded when the app is launched with
/// `open` or from Finder, and the file is what a bug report can quote
/// (DEV-1243). Call after `paths::init`. If the file cannot be opened, logs
/// go to stdout only, and the first line says why.
pub(crate) fn init_tracing() {
    let file = crate::paths::logs_dir().map(|dir| {
        crate::log_file::RotatingLog::open(
            &dir,
            crate::log_file::FILE_NAME,
            crate::log_file::MAX_BYTES,
            crate::log_file::KEEP,
        )
    });
    let (sink, problem) = match file {
        Some(Ok(log)) => (Some(log), None),
        Some(Err(e)) => (None, Some(format!("could not open the log file: {e}"))),
        None => (
            None,
            Some("no data root; not logging to a file".to_string()),
        ),
    };
    let path = sink.as_ref().map(|l| l.path().display().to_string());
    let file_layer = sink.map(|log| {
        fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || log.clone())
    });
    let _ = tracing_subscriber::registry()
        .with(filter())
        .with(fmt::layer().with_target(false))
        .with(file_layer)
        .try_init();
    match (path, problem) {
        (Some(path), _) => tracing::info!("logging to {path}"),
        (None, Some(problem)) => tracing::warn!("{problem}"),
        (None, None) => {}
    }
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_env("ALLELE_LOG").unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Log to **stderr** instead of stdout.
///
/// Required by `--mcp-serve`: stdout there carries newline-delimited JSON-RPC
/// frames, and a single log line written into that stream desynchronises the
/// client's parser. MCP servers conventionally log to stderr for exactly this
/// reason.
///
/// No file sink here: an MCP server is a short-lived client process, and a
/// second writer rotating the app's file would race the app's own rotation.
pub(crate) fn init_tracing_stderr() {
    let _ = fmt()
        .with_env_filter(filter())
        .with_target(false)
        .with_level(true)
        .with_writer(std::io::stderr)
        .try_init();
}
