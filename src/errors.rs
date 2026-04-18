//! Crate-level error type.
//!
//! Allele is a UI app, so most failures are logged and the app continues.
//! This type exists for the fallible operations where we do propagate:
//! filesystem IO, git invocations, JSON parsing, platform-specific
//! syscalls, and agent command building.
//!
//! Handlers may still call `tracing::error!` directly on convert — the
//! typed error is for callers that need to branch on the failure mode
//! (e.g. "retry on CloneBackend::Unavailable but not on IO::NotFound").

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Top-level error for operations that can fail in controlled ways.
///
/// Adoption is incremental — some variants are used today (`Io`, `Clone`,
/// `Git`, `State`, `Other`), others exist for imminent uses
/// (`PlatformUnsupported`, `Config`, `Agent`, `Json`). Lint is relaxed for
/// the unused ones.
#[derive(Debug, Error)]
pub(crate) enum AlleleError {
    #[error("I/O error at {path:?}: {source}")]
    Io {
        path: Option<PathBuf>,
        #[source]
        source: io::Error,
    },

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("git operation failed: {0}")]
    Git(String),

    #[allow(dead_code)] // reserved for platform::unsupported paths
    #[error("platform operation unsupported on this OS: {0}")]
    PlatformUnsupported(&'static str),

    #[error("clone backend failed: {0}")]
    Clone(String),

    #[allow(dead_code)] // reserved for agent spawn failures
    #[error("agent command build failed: {0}")]
    Agent(String),

    #[allow(dead_code)] // reserved for config parse errors
    #[error("config parse failed at {path:?}: {reason}")]
    Config { path: PathBuf, reason: String },

    #[error("state corruption: {0}")]
    State(String),

    #[error("other: {0}")]
    Other(String),
}

impl From<io::Error> for AlleleError {
    fn from(source: io::Error) -> Self {
        AlleleError::Io { path: None, source }
    }
}

/// Crate-level Result alias. Callers that want anyhow-style coercion
/// can still use `anyhow::Result` ad-hoc; this alias is for functions
/// that want to commit to the typed error surface.
#[allow(dead_code)] // Introduced in step 1; adoption is incremental.
pub(crate) type Result<T> = std::result::Result<T, AlleleError>;
