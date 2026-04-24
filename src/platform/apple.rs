//! macOS adapter implementations. Each impl delegates to the
//! pre-existing module that already contains the tested logic —
//! these shims exist purely to satisfy the trait boundary.
//!
//! See [`crate::platform`] module docs for the overall rationale.
//!
//! On non-macOS these types are unused (they live on the
//! `#[cfg(target_os = "macos")]` branch of `Platform::detect`);
//! the crate-level `#[allow(dead_code)]` keeps the build clean
//! when cross-compiling.

#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Stdio};

use crate::errors::{AlleleError, Result};

/// APFS `clonefile(2)` backend. Delegates to
/// [`crate::clone::create_clone`], which performs the actual
/// syscall and owns the EXDEV diagnostic.
pub(crate) struct AppleCloneBackend;

impl super::CloneBackend for AppleCloneBackend {
    fn clone_dir(&self, src: &Path, dst: &Path) -> Result<()> {
        // crate::clone::create_clone takes (source, workspace_name)
        // where workspace_name is a leaf under ~/.allele/workspaces/.
        // The trait's contract is raw src -> dst directory clone, so
        // we use clonefile(2) directly for the generic case.
        use std::ffi::CString;
        if dst.exists() {
            return Err(AlleleError::Clone(format!(
                "clone destination already exists: {}",
                dst.display()
            )));
        }
        let src_cstr = CString::new(src.to_string_lossy().as_bytes())
            .map_err(|e| AlleleError::Clone(format!("src path contains NUL: {e}")))?;
        let dst_cstr = CString::new(dst.to_string_lossy().as_bytes())
            .map_err(|e| AlleleError::Clone(format!("dst path contains NUL: {e}")))?;
        // SAFETY: both CStrings live for the duration of the call.
        let rc = unsafe { libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EXDEV) {
                return Err(AlleleError::Clone(format!(
                    "cannot clone across volumes: {} -> {} (APFS clonefile(2) \
                     requires same volume)",
                    src.display(),
                    dst.display(),
                )));
            }
            return Err(AlleleError::Clone(format!("clonefile() failed: {err}")));
        }
        Ok(())
    }

    fn supports_cow(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "apfs-clonefile"
    }
}

/// Chrome-via-AppleScript backend. Delegates to [`crate::browser`].
pub(crate) struct AppleBrowser;

impl super::BrowserIntegration for AppleBrowser {
    fn is_running(&self) -> bool {
        crate::browser::chrome_running()
    }

    fn create_tab(&self, url: &str) -> Option<i64> {
        crate::browser::create_tab(url)
    }

    fn activate_tab(&self, id: i64) -> bool {
        crate::browser::activate_tab(id)
    }

    fn navigate_tab(&self, id: i64, url: &str) -> bool {
        crate::browser::navigate_tab(id, url)
    }

    fn close_tab(&self, id: i64) -> bool {
        crate::browser::close_tab(id)
    }
}

/// `open(1)` / `afplay` / `osascript display dialog` backend.
/// Delegates to [`crate::hooks`] where logic is already implemented.
pub(crate) struct AppleShell;

impl super::SystemShell for AppleShell {
    fn open_url(&self, url: &str) {
        if let Err(e) = Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            tracing::warn!("AppleShell::open_url({url}): spawn failed: {e}");
        }
    }

    fn reveal_in_files(&self, path: &Path) {
        if let Err(e) = Command::new("open")
            .arg("-R")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            tracing::warn!(
                "AppleShell::reveal_in_files({}): spawn failed: {e}",
                path.display()
            );
        }
    }

    fn play_sound(&self, sound: &str) {
        crate::hooks::play_sound(sound);
    }

    fn show_fatal_dialog(&self, title: &str, body: &str) {
        crate::hooks::show_fatal_dialog(title, body);
    }
}
