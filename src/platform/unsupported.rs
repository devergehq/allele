//! Non-macOS fallback adapter implementations.
//!
//! The goal per ARCHITECTURE.md §3.2 is that missing functionality
//! is *discoverable* — call sites that route through these stubs
//! emit a `tracing::warn!` so operators running on unsupported
//! platforms see something in the logs rather than silent no-ops.
//!
//! [`FullCopyCloneBackend`] is the one non-stub adapter: it
//! implements directory cloning via recursive `std::fs::copy`. It
//! is substantially slower and uses real disk space, but it is
//! *correct* — a session clone still works, just without APFS'
//! copy-on-write cost model.
//!
//! On macOS these types are unused (they live on the `#[cfg(not(
//! target_os = "macos"))]` branch of `Platform::detect`); the
//! `#[allow(dead_code)]` annotations keep them from tripping the
//! macOS build's dead-code lint.

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::Path;

use crate::errors::{AlleleError, Result};

/// Recursive `std::fs::copy` fallback. Used on Linux/Windows where
/// `clonefile(2)` is macOS-only. Correct but slow and non-CoW —
/// every byte of the source is copied.
pub(crate) struct FullCopyCloneBackend;

impl super::CloneBackend for FullCopyCloneBackend {
    fn clone_dir(&self, src: &Path, dst: &Path) -> Result<()> {
        if dst.exists() {
            return Err(AlleleError::Clone(format!(
                "clone destination already exists: {}",
                dst.display()
            )));
        }
        copy_recursive(src, dst).map_err(|e| {
            AlleleError::Clone(format!(
                "recursive copy {} -> {} failed: {e}",
                src.display(),
                dst.display()
            ))
        })
    }

    fn supports_cow(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "recursive-copy"
    }
}

/// Recursively copy `src` to `dst`. Both regular files and
/// directories are handled; symlinks are followed (matching the
/// semantics of APFS `clonefile(2)` on macOS, which dereferences).
fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let child_src = entry.path();
            let child_dst = dst.join(entry.file_name());
            copy_recursive(&child_src, &child_dst)?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }
}

/// No-op browser integration with discoverable warnings. Each method
/// logs via `tracing::warn!` before returning the failure value so
/// callers can see what they tried to do in an unsupported environment.
pub(crate) struct UnsupportedBrowser;

impl super::BrowserIntegration for UnsupportedBrowser {
    fn is_running(&self) -> bool {
        false
    }

    fn create_tab(&self, url: &str) -> Option<i64> {
        tracing::warn!(
            "UnsupportedBrowser::create_tab({url}): no browser adapter for this OS"
        );
        None
    }

    fn activate_tab(&self, id: i64) -> bool {
        tracing::warn!(
            "UnsupportedBrowser::activate_tab({id}): no browser adapter for this OS"
        );
        false
    }

    fn navigate_tab(&self, id: i64, url: &str) -> bool {
        tracing::warn!(
            "UnsupportedBrowser::navigate_tab({id}, {url}): no browser adapter for this OS"
        );
        false
    }

    fn close_tab(&self, id: i64) -> bool {
        tracing::warn!(
            "UnsupportedBrowser::close_tab({id}): no browser adapter for this OS"
        );
        false
    }
}

/// Portable shell integration: `xdg-open` on Linux-like systems
/// when available, `tracing::warn!` stubs otherwise.
pub(crate) struct PortableSystemShell;

impl super::SystemShell for PortableSystemShell {
    fn open_url(&self, url: &str) {
        use std::process::{Command, Stdio};
        let spawn = Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Err(e) = spawn {
            tracing::warn!("PortableSystemShell::open_url({url}): xdg-open failed: {e}");
        }
    }

    fn reveal_in_files(&self, path: &Path) {
        tracing::warn!(
            "PortableSystemShell::reveal_in_files({}): no file-manager adapter for this OS",
            path.display()
        );
    }

    fn play_sound(&self, sound: &str) {
        tracing::warn!(
            "PortableSystemShell::play_sound({sound}): no audio adapter for this OS"
        );
    }

    fn show_fatal_dialog(&self, title: &str, body: &str) {
        // Last-ditch: log to stderr via tracing at ERROR level.
        // Without a GUI toolkit we can't block on user confirmation.
        tracing::error!(
            "FATAL ({title}): {body} — no dialog adapter for this OS, continuing"
        );
    }
}
