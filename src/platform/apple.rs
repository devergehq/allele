//! macOS platform implementations.
//!
//! Clone backend uses APFS `clonefile(2)`. Browser integration drives
//! Google Chrome via AppleScript. System shell wraps `open(1)`,
//! `afplay(1)`, and a Cocoa `NSAlert` for fatal dialogs.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::path::Path;
use std::process::Command;

use crate::browser;
use crate::errors::AlleleError;
use crate::hooks;

use super::{BrowserIntegration, CloneBackend, PlatformResult, SystemShell, TabId};

// -----------------------------------------------------------------------
// AppleCloneBackend
// -----------------------------------------------------------------------

pub(crate) struct AppleCloneBackend;

impl CloneBackend for AppleCloneBackend {
    fn clone(&self, source: &Path, dest: &Path) -> PlatformResult<()> {
        let src_cstr = CString::new(source.to_string_lossy().as_bytes())
            .map_err(|e| AlleleError::Clone(format!("nul in source path: {e}")))?;
        let dst_cstr = CString::new(dest.to_string_lossy().as_bytes())
            .map_err(|e| AlleleError::Clone(format!("nul in dest path: {e}")))?;

        // SAFETY: both CStrings outlive the FFI call; clonefile is a
        // standard POSIX-ish syscall on macOS and takes raw char
        // pointers. Flag 0 means default behaviour (follow symlinks).
        let result =
            unsafe { libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0) };

        if result != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EXDEV) {
                return Err(AlleleError::Clone(format!(
                    "cross-volume clone: source ({}) and destination ({}) \
                     must share a volume for APFS clonefile(2). Move your \
                     project or ~/.allele/ so they share a volume.",
                    source.display(),
                    dest.display()
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
        "APFS clonefile"
    }
}

// -----------------------------------------------------------------------
// AppleBrowserIntegration
// -----------------------------------------------------------------------

pub(crate) struct AppleBrowserIntegration;

impl BrowserIntegration for AppleBrowserIntegration {
    fn is_running(&self) -> bool {
        browser::chrome_running()
    }

    fn create_tab(&self, url: &str) -> Option<TabId> {
        browser::create_tab(url).map(TabId)
    }

    fn activate_tab(&self, id: TabId) -> bool {
        browser::activate_tab(id.0)
    }

    fn navigate_tab(&self, id: TabId, url: &str) -> bool {
        browser::navigate_tab(id.0, url)
    }

    fn close_tab(&self, id: TabId) -> bool {
        browser::close_tab(id.0)
    }
}

// -----------------------------------------------------------------------
// AppleSystemShell
// -----------------------------------------------------------------------

pub(crate) struct AppleSystemShell;

impl SystemShell for AppleSystemShell {
    fn open_url(&self, url: &str) {
        if let Err(e) = Command::new("open").arg(url).spawn() {
            tracing::warn!("open({url}) failed: {e}");
        }
    }

    fn reveal_in_files(&self, path: &Path) {
        if let Err(e) = Command::new("open").arg("-R").arg(path).spawn() {
            tracing::warn!("open -R {} failed: {e}", path.display());
        }
    }

    fn play_sound(&self, path: &Path) {
        // Existing hooks::play_sound already handles path → afplay and
        // gracefully handles missing files. Defer to it so we keep the
        // existing behaviour.
        hooks::play_sound(&path.to_string_lossy());
    }

    fn show_fatal_dialog(&self, title: &str, message: &str) {
        hooks::show_fatal_dialog(title, message);
    }
}
