//! Platform-abstraction layer.
//!
//! Allele runs as a native desktop app and currently leans heavily on
//! macOS-specific primitives (APFS `clonefile(2)`, AppleScript for
//! Chrome control, `open(1)`, `afplay(1)`, Cocoa dialogs). Porting to
//! Linux or Windows requires swappable implementations, not
//! conditional-compilation sprinkled through business logic.
//!
//! This module defines three traits that carve the OS boundary:
//!
//! - [`CloneBackend`] — COW filesystem clones. macOS uses `clonefile`,
//!   Linux can use `FICLONE` on Btrfs/XFS/ZFS, fallback is a full copy.
//! - [`BrowserIntegration`] — Chrome tab control. macOS uses AppleScript;
//!   other platforms would use the Chrome DevTools Protocol or return
//!   [`BrowserIntegration::Unsupported`].
//! - [`SystemShell`] — miscellaneous OS primitives: open URL, reveal in
//!   file manager, play sound. Wraps `open` / `xdg-open` / `start`.
//!
//! `Platform` bundles all three behind an `Arc` so handlers can inject
//! a single dependency into [`crate::app_state::AppState`].
//!
//! Selection happens exactly once at startup via [`Platform::detect`].
//! Call sites use the trait objects; the concrete impls live in
//! sibling modules (`apple.rs`, `linux.rs`, `windows.rs`,
//! `unsupported.rs`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub(crate) mod apple;
pub(crate) mod unsupported;

use crate::errors::AlleleError;

/// Result alias for platform operations.
pub(crate) type PlatformResult<T> = std::result::Result<T, AlleleError>;

// -----------------------------------------------------------------------
// CloneBackend
// -----------------------------------------------------------------------

/// Filesystem snapshot / copy-on-write cloning.
///
/// Implementations must be cheap for same-volume sources (that's the
/// whole point of cloning on COW filesystems). Fallback implementations
/// may do a deep copy — callers should assume "O(size) at worst".
pub(crate) trait CloneBackend: Send + Sync {
    /// Clone `source` to `dest`. `dest` must not exist. On success the
    /// clone appears as a regular directory; on COW filesystems the
    /// backing blocks are shared until written.
    fn clone(&self, source: &Path, dest: &Path) -> PlatformResult<()>;

    /// True if this backend uses genuine copy-on-write (same-volume
    /// required). Callers can use this to decide whether to warn the
    /// user about potentially expensive operations.
    fn supports_cow(&self) -> bool;

    /// Human-readable name for diagnostics. "APFS clonefile", "Btrfs
    /// reflink", "full recursive copy", etc.
    fn name(&self) -> &'static str;
}

// `supports_cow` and `TabId` are part of the trait surface even though
// no call site needs them yet — they exist so downstream UI features
// ("clone is expensive on this platform", "persist TabId across
// restart") can land without trait changes.
#[allow(dead_code)]
const _: fn() = || {
    // Silences the "never used" warnings without hiding real deadness:
    // if the trait item is deleted this reference breaks.
    fn _touch(b: &dyn CloneBackend) -> bool { b.supports_cow() }
    let _ = _touch;
    let _tab = TabId(0);
};

// -----------------------------------------------------------------------
// BrowserIntegration
// -----------------------------------------------------------------------

/// Opaque tab identifier. Carries whatever the underlying backend uses
/// to address a tab (AppleScript tab id, CDP target id, etc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TabId(pub i64);

/// Control over the user's browser for preview-URL workflows.
///
/// The macOS implementation drives Google Chrome via AppleScript.
/// Non-macOS platforms can return `unsupported` from every method —
/// [`AppState`] already falls back to `open URL` when browser
/// integration is disabled.
pub(crate) trait BrowserIntegration: Send + Sync {
    /// Is the target browser currently running? If not, callers should
    /// skip `create_tab` and fall back to a plain system URL open.
    fn is_running(&self) -> bool;

    /// Create a new tab at the given URL and return its id. `None` if
    /// the browser is not running or the call failed.
    fn create_tab(&self, url: &str) -> Option<TabId>;

    /// Bring the tab with this id to the foreground. Returns `false`
    /// if the id is stale (tab was closed externally).
    fn activate_tab(&self, id: TabId) -> bool;

    /// Navigate an existing tab to a new URL. Returns `false` if the
    /// id is stale.
    fn navigate_tab(&self, id: TabId, url: &str) -> bool;

    /// Close the tab. Returns `false` if the id is stale or the close
    /// failed.
    fn close_tab(&self, id: TabId) -> bool;
}

// -----------------------------------------------------------------------
// SystemShell
// -----------------------------------------------------------------------

/// Miscellaneous OS primitives: open URL, reveal in file manager, play
/// a sound file, show a fatal modal dialog.
pub(crate) trait SystemShell: Send + Sync {
    /// Open a URL (or any file with a registered handler) in the
    /// system default application. `open` on macOS, `xdg-open` on
    /// Linux, `start` on Windows.
    fn open_url(&self, url: &str);

    /// Reveal a file or directory in the system file manager (Finder,
    /// Nautilus, Explorer). No-op on unsupported platforms.
    fn reveal_in_files(&self, path: &Path);

    /// Play a sound file. macOS uses `afplay`; Linux would use
    /// `aplay` or `paplay`; Windows uses `powershell` / WinMM. No-op
    /// on unsupported platforms.
    fn play_sound(&self, path: &Path);

    /// Show a blocking modal dialog for unrecoverable startup errors.
    /// Used before any window is open (missing git, etc). On
    /// unsupported platforms the message is logged via `tracing`.
    fn show_fatal_dialog(&self, title: &str, message: &str);
}

// -----------------------------------------------------------------------
// Platform bundle
// -----------------------------------------------------------------------

/// All platform adapters bundled behind `Arc`s so `AppState` can hold
/// one concrete value and share it with background tasks.
#[derive(Clone)]
pub(crate) struct Platform {
    pub(crate) clone: Arc<dyn CloneBackend>,
    pub(crate) browser: Arc<dyn BrowserIntegration>,
    pub(crate) shell: Arc<dyn SystemShell>,
}

/// Process-wide platform singleton. Set once in `main()`; leaf
/// components (e.g. [`crate::terminal::TerminalView`]) that are too
/// deeply nested for explicit injection read it via [`global`].
///
/// Tests can seed this with `GLOBAL.set(fake_platform)` before
/// instantiating any component that reads it.
static GLOBAL: OnceLock<Platform> = OnceLock::new();

/// Get the process-wide platform bundle. Panics if called before
/// [`Platform::install_global`]; callers in the main flow are always
/// downstream of `main()` where install happens, so this is a real
/// invariant violation if it fires.
pub(crate) fn global() -> &'static Platform {
    GLOBAL.get().expect(
        "platform::global() called before Platform::install_global — \
         set once in main() before any UI component constructor runs",
    )
}

impl Platform {
    /// Pick the right backends for the compile target. Should be
    /// called exactly once at the top of `main()`.
    pub(crate) fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                clone: Arc::new(apple::AppleCloneBackend),
                browser: Arc::new(apple::AppleBrowserIntegration),
                shell: Arc::new(apple::AppleSystemShell),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                clone: Arc::new(unsupported::FullCopyCloneBackend),
                browser: Arc::new(unsupported::UnsupportedBrowser),
                shell: Arc::new(unsupported::PortableSystemShell),
            }
        }
    }

    /// Install this bundle as the process-wide singleton. Must be
    /// called exactly once at startup; subsequent calls are a no-op so
    /// test scaffolding stays idempotent under cargo test's one-shot
    /// process model.
    pub(crate) fn install_global(self) -> Self {
        let _ = GLOBAL.set(self.clone());
        self
    }
}

/// Helper the clone backend uses to derive the workspace base
/// directory (`~/.allele/workspaces`).
#[allow(dead_code)] // Utility reserved for future non-macOS clone backends.
pub(crate) fn workspaces_base() -> PlatformResult<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".allele").join("workspaces"))
        .ok_or_else(|| AlleleError::Other("home_dir() returned None".into()))
}
