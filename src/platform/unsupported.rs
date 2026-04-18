//! Fallback platform implementations for non-macOS targets.
//!
//! These exist so the crate compiles on Linux / Windows. Most of
//! Allele's higher-level features (preview-browser integration, APFS
//! cloning) still require a real implementation to be useful — see
//! [`FullCopyCloneBackend`] for the one fallback that actually does the
//! work.

#![cfg(not(target_os = "macos"))]

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::errors::AlleleError;

use super::{BrowserIntegration, CloneBackend, PlatformResult, SystemShell, TabId};

// -----------------------------------------------------------------------
// FullCopyCloneBackend
// -----------------------------------------------------------------------

/// Clone backend that falls back to a recursive copy. Correct on any
/// filesystem but proportional to directory size — no COW sharing.
///
/// Linux impls should replace this with a `CloneBackend` that probes
/// for `FICLONE` support on Btrfs/XFS/ZFS before copying. A pure
/// `std::fs::copy` per file is the baseline.
pub(crate) struct FullCopyCloneBackend;

impl CloneBackend for FullCopyCloneBackend {
    fn clone(&self, source: &Path, dest: &Path) -> PlatformResult<()> {
        copy_dir_recursive(source, dest).map_err(|e| {
            AlleleError::Clone(format!(
                "recursive copy {} -> {} failed: {e}",
                source.display(),
                dest.display()
            ))
        })
    }

    fn supports_cow(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "full recursive copy"
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else if file_type.is_file() {
            fs::copy(&path, &dest_path)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&path)?;
                std::os::unix::fs::symlink(target, &dest_path)?;
            }
            #[cfg(windows)]
            {
                // Skip symlinks on Windows — creating them needs admin privs
                tracing::warn!("skipping symlink {}: not supported on Windows", path.display());
            }
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------
// UnsupportedBrowser
// -----------------------------------------------------------------------

/// Browser integration stub. Always reports "not running" so
/// preview-URL workflows fall back to `SystemShell::open_url`.
pub(crate) struct UnsupportedBrowser;

impl BrowserIntegration for UnsupportedBrowser {
    fn is_running(&self) -> bool {
        false
    }
    fn create_tab(&self, _url: &str) -> Option<TabId> {
        None
    }
    fn activate_tab(&self, _id: TabId) -> bool {
        false
    }
    fn navigate_tab(&self, _id: TabId, _url: &str) -> bool {
        false
    }
    fn close_tab(&self, _id: TabId) -> bool {
        false
    }
}

// -----------------------------------------------------------------------
// PortableSystemShell
// -----------------------------------------------------------------------

/// Non-macOS system shell. Uses `xdg-open` on Linux and `start` on
/// Windows; sound and file-manager operations log a warning rather
/// than silently no-op so missing platform support is discoverable.
pub(crate) struct PortableSystemShell;

impl SystemShell for PortableSystemShell {
    fn open_url(&self, url: &str) {
        #[cfg(target_os = "linux")]
        let cmd = "xdg-open";
        #[cfg(target_os = "windows")]
        let cmd = "start";
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let cmd: &str = {
            tracing::warn!("open_url({url}): no handler for this platform");
            return;
        };

        if let Err(e) = Command::new(cmd).arg(url).spawn() {
            tracing::warn!("{cmd}({url}) failed: {e}");
        }
    }

    fn reveal_in_files(&self, path: &Path) {
        #[cfg(target_os = "linux")]
        {
            let parent = path.parent().unwrap_or(path);
            if let Err(e) = Command::new("xdg-open").arg(parent).spawn() {
                tracing::warn!("xdg-open {} failed: {e}", parent.display());
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Err(e) = Command::new("explorer").arg("/select,").arg(path).spawn() {
                tracing::warn!("explorer /select {} failed: {e}", path.display());
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            tracing::warn!("reveal_in_files({}): not supported", path.display());
        }
    }

    fn play_sound(&self, path: &Path) {
        // Baseline: log. Linux would use `paplay` / `aplay`; Windows
        // can drive WinMM via powershell. Both require more bindings
        // than is worth shipping before first real user.
        tracing::debug!("play_sound({}) not implemented on this platform", path.display());
    }

    fn show_fatal_dialog(&self, title: &str, message: &str) {
        // Without a native toolkit there's nowhere to pop a modal.
        // Log loudly and let `main()` exit — this matches the bash
        // dev workflow on Linux.
        tracing::error!("FATAL [{title}]: {message}");
    }
}
