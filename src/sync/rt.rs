//! GPUI ↔ tokio bridge for session sync.
//!
//! The app runs on GPUI's executor, but rust-s3's async I/O (reqwest/hyper)
//! requires a **tokio** runtime. This module owns a shared multi-threaded tokio
//! runtime and a [`block_on`] that drives a sync future to completion.
//!
//! Usage: run the S3 work on a GPUI background task and block on it there —
//! never on the main/UI thread:
//!
//! ```ignore
//! let result = cx.background_executor()
//!     .spawn(async move { rt::block_on(connect::validate_bucket(&cfg)) })
//!     .await;
//! ```

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Runtime;

/// The shared runtime, created on first use.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build the session-sync tokio runtime")
    })
}

/// Drive `future` to completion on the shared sync runtime, blocking the calling
/// thread. Call this only from a GPUI background task (`cx.background_executor()`),
/// never the UI thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    runtime().block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_drives_a_future() {
        assert_eq!(block_on(async { 1 + 2 }), 3);
    }

    #[test]
    fn block_on_works_repeatedly() {
        // The runtime is reused across calls.
        assert_eq!(block_on(async { "a" }), "a");
        assert_eq!(block_on(async { "b" }), "b");
    }
}
