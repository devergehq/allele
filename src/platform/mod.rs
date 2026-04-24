//! OS-abstraction layer. Three traits carve the OS boundary:
//!
//! - [`CloneBackend`] — APFS `clonefile(2)` (macOS) vs recursive
//!   `std::fs::copy` (everywhere else).
//! - [`BrowserIntegration`] — per-session Chrome tab via AppleScript
//!   (macOS) vs a tracing-warning stub.
//! - [`SystemShell`] — `open(1)`, `afplay`, `osascript` display dialog
//!   (macOS) vs portable `xdg-open`/no-op fallbacks.
//!
//! Adapters DELEGATE to existing `crate::clone`, `crate::browser`,
//! `crate::hooks` modules rather than duplicating logic. This is
//! deliberate — phase 17 will migrate individual call sites through
//! the trait object; the module boundary stays intact.
//!
//! See ARCHITECTURE.md §3.2 (trait boundary) and §4.1 (global
//! accessor invariant: install before first use).

use std::path::Path;
use std::sync::{Arc, OnceLock};

pub(crate) mod apple;
pub(crate) mod unsupported;

/// APFS-style directory clone. macOS uses `clonefile(2)` for
/// near-instant, zero-disk-cost copy-on-write snapshots; fallback
/// impls fall back to `std::fs::copy`.
#[allow(dead_code)] // methods adopted incrementally in phase 17 call-site migration
pub(crate) trait CloneBackend: Send + Sync {
    /// Clone `src` to `dst` (which must not exist). Errors are
    /// returned as `AlleleError::Clone` regardless of backend.
    fn clone_dir(&self, src: &Path, dst: &Path) -> crate::errors::Result<()>;

    /// True if this backend offers copy-on-write semantics (i.e. the
    /// clone is structurally cheap and shares storage until mutation).
    fn supports_cow(&self) -> bool;

    /// Short identifier used in diagnostics ("apfs-clonefile",
    /// "recursive-copy", etc.).
    fn name(&self) -> &'static str;
}

/// External browser (Chrome) tab management. On macOS this drives
/// the user's real Chrome via AppleScript; elsewhere the trait
/// emits a tracing warning so missing functionality is discoverable.
#[allow(dead_code)] // methods adopted incrementally in phase 17 call-site migration
pub(crate) trait BrowserIntegration: Send + Sync {
    /// True if the supported browser process is currently running.
    fn is_running(&self) -> bool;

    /// Create a new tab at `url`. Returns the new tab's stable id, or
    /// `None` if the browser is unavailable or scripting failed.
    fn create_tab(&self, url: &str) -> Option<i64>;

    /// Activate the tab with `id`. Returns true on success.
    fn activate_tab(&self, id: i64) -> bool;

    /// Navigate the tab with `id` to `url`. Returns true on success.
    fn navigate_tab(&self, id: i64, url: &str) -> bool;

    /// Close the tab with `id`. Returns true on success.
    fn close_tab(&self, id: i64) -> bool;
}

/// Shell-level OS interactions: opening URLs in the user's default
/// browser, revealing files in the file manager, playing sounds, and
/// showing fatal startup dialogs.
#[allow(dead_code)] // methods adopted incrementally in phase 17 call-site migration
pub(crate) trait SystemShell: Send + Sync {
    /// Open `url` in the user's configured default browser.
    fn open_url(&self, url: &str);

    /// Reveal `path` in the OS file manager (Finder on macOS).
    fn reveal_in_files(&self, path: &Path);

    /// Play a system sound given its name or path. On macOS this
    /// expects a filesystem path understood by `afplay`.
    fn play_sound(&self, sound: &str);

    /// Show a blocking fatal-error dialog with a stop icon. Returns
    /// once the user dismisses it. Used for startup errors that must
    /// block before the caller exits the process.
    fn show_fatal_dialog(&self, title: &str, body: &str);
}

/// Bundle of trait objects representing the current platform's
/// adapters. Stored once globally via [`Platform::install_global`];
/// subsystems get cheap `Arc` clones to pass into background tasks.
pub(crate) struct Platform {
    #[allow(dead_code)] // held for phase-17 migration
    pub(crate) clone: Arc<dyn CloneBackend>,
    #[allow(dead_code)] // held for phase-17 migration
    pub(crate) browser: Arc<dyn BrowserIntegration>,
    pub(crate) shell: Arc<dyn SystemShell>,
}

impl Platform {
    /// Construct the platform bundle for the current `target_os`.
    /// Pure — no globals touched. Call [`Self::install_global`] to
    /// make it discoverable via [`global`].
    pub(crate) fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                clone: Arc::new(apple::AppleCloneBackend),
                browser: Arc::new(apple::AppleBrowser),
                shell: Arc::new(apple::AppleShell),
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

    /// Store this bundle in the process-wide [`OnceLock`] so call
    /// sites that don't have an `AppState` handle (panic hooks,
    /// early startup error paths) can still reach the platform via
    /// [`global`]. Returns a freshly Arc-cloned bundle so the caller
    /// can also keep its own handle — callers typically stash the
    /// returned value on `AppState.platform`.
    ///
    /// Idempotent: subsequent calls silently drop — the first win.
    pub(crate) fn install_global(self) -> Self {
        let copy = self.clone_arcs();
        let _ = PLATFORM.set(copy);
        self
    }

    /// Clone the contained `Arc`s — not `Clone` on `Platform` itself
    /// because trait-object field wrappers don't auto-derive.
    #[allow(dead_code)] // used by install_global + AppState construction
    pub(crate) fn clone_arcs(&self) -> Self {
        Self {
            clone: Arc::clone(&self.clone),
            browser: Arc::clone(&self.browser),
            shell: Arc::clone(&self.shell),
        }
    }
}

static PLATFORM: OnceLock<Platform> = OnceLock::new();

/// Access the process-wide platform bundle installed by
/// [`Platform::install_global`]. Panics with a clear message if
/// called before installation — see ARCHITECTURE.md §4.1.
#[allow(dead_code)] // used by phase-17 migration of panic hook / early-error paths
pub(crate) fn global() -> &'static Platform {
    PLATFORM.get().expect(
        "platform::global() called before Platform::detect().install_global() — \
         see ARCHITECTURE.md §4.1",
    )
}
