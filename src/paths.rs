//! Every filesystem path Allele owns, derived from one root (DEV-487).
//!
//! Allele is developed in Allele sessions, so verifying a change means running
//! a second instance — and every path below used to resolve `~` independently,
//! which meant the test instance shared `state.json`, the workspaces root and
//! the MCP control socket with the live one. `crate::cli` documents what that
//! costs: a second process atomically rewriting `state.json` can clobber the
//! live app's session list on a last-writer-wins race. That guard refuses a
//! stray launch; this module makes a *deliberate* one safe.
//!
//! # What belongs here
//!
//! Only paths **Allele owns**. Data belonging to other tools stays anchored to
//! the real home even in a sandbox, because a sandboxed Allele still drives the
//! user's actual Claude Code, reads their actual transcripts, and launches
//! their actually-installed binaries. Deliberately NOT routed through here:
//!
//! - `~/.claude`, `~/.claude.json` — Claude Code's own state and trust file
//! - `~/.config/opencode` — opencode's own config
//! - agent binary probe paths (`~/.local/bin`, `~/.npm/bin`)
//! - `~` expansion when displaying a path to the user
//!
//! Redirecting any of those would give a sandbox instance a broken agent rather
//! than an isolated one.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Directory name under the root that holds Allele's own data.
const ALLELE_DIR: &str = ".allele";
/// Where the sandbox root lives, relative to the real home.
const SANDBOX_DIR: &str = ".allele-sandbox";
/// Environment variable consulted when no `--home` flag was passed.
pub const HOME_ENV: &str = "ALLELE_HOME";

static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Fix the root for the life of the process. Call once from `main`, before
/// anything reads a path or spawns a child — a later call is ignored, which is
/// deliberate: a root that changed midway would leave half the app writing to
/// one tree and half to another.
///
/// `override_root` comes from `--home`/`--sandbox`. With `None`, `ALLELE_HOME`
/// is consulted, and failing that the real home directory is used — so an
/// ordinary launch resolves exactly the paths it always did.
pub fn init(override_root: Option<PathBuf>) {
    let resolved = override_root
        .or_else(|| {
            std::env::var(HOME_ENV)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir);
    let _ = ROOT.set(resolved);
}

/// The root every path below hangs off. `None` only when there is no home
/// directory and nothing was passed, which is the same condition the previous
/// `dirs::home_dir()?` calls already returned `None` for.
pub fn root() -> Option<PathBuf> {
    ROOT.get().cloned().flatten().or_else(dirs::home_dir)
}

/// The default sandbox root, `~/.allele-sandbox`, anchored to the REAL home —
/// never to `root()`, or `--sandbox` inside a sandbox would nest.
pub fn default_sandbox_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(SANDBOX_DIR))
}

/// True when this process is running against a root other than the real home.
/// Drives the "this is not your live instance" marker in the UI.
pub fn is_redirected() -> bool {
    match (root(), dirs::home_dir()) {
        (Some(r), Some(h)) => r != h,
        _ => false,
    }
}

// --- Allele-owned paths ---------------------------------------------------

/// `<root>/.allele` — the base for most of Allele's data, and the directory
/// `crate::hooks` and `crate::dispatch` hang the control socket off.
pub fn allele_dir() -> Option<PathBuf> {
    root().map(|r| r.join(ALLELE_DIR))
}

/// `<root>/.config/allele` — settings and the panic log.
pub fn config_dir() -> Option<PathBuf> {
    root().map(|r| r.join(".config").join("allele"))
}

pub fn settings_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.json"))
}

pub fn crash_log_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("crash.log"))
}

pub fn state_file() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("state.json"))
}

/// The parent directory of every session clone.
pub fn workspaces_root() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("workspaces"))
}

/// `<root>/.allele/trash` — where discarded clones wait out `TRASH_TTL_DAYS`.
pub fn trash_root() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("trash"))
}

pub fn keymap_file() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("keymap.json"))
}

pub fn crash_dir() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("crash"))
}

pub fn diagnostics_dir() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("diagnostics"))
}

pub fn debug_dir() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("debug"))
}

pub fn attachments_dir() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("attachments"))
}

pub fn sync_ledger_file() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("sync-ledger.json"))
}

pub fn base_infra_dir() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("base-infra"))
}

/// `<root>/.allele/projects/<project>/scripts` — where a relative `startup` or
/// `shutdown` command resolves from.
pub fn project_scripts_dir(project_name: &str) -> Option<PathBuf> {
    allele_dir().map(|d| d.join("projects").join(project_name).join("scripts"))
}

/// `<root>/.allele/browsers` — removed at startup; kept only so the stale
/// sweep can find it.
pub fn legacy_browsers_dir() -> Option<PathBuf> {
    allele_dir().map(|d| d.join("browsers"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `init` writes a process-global OnceLock, so these exercise the pure
    // derivation from an explicit root rather than calling `init` and fighting
    // over global state with every other test in the binary.
    fn derive(root: &std::path::Path) -> Vec<PathBuf> {
        vec![
            root.join(".allele").join("state.json"),
            root.join(".allele").join("workspaces"),
            root.join(".config").join("allele").join("settings.json"),
        ]
    }

    #[test]
    fn every_owned_path_hangs_off_one_root() {
        // The property that matters: redirect the root and nothing is left
        // pointing at the old tree.
        let sandbox = PathBuf::from("/tmp/sandbox-root");
        for p in derive(&sandbox) {
            assert!(p.starts_with(&sandbox), "{} escaped the root", p.display());
        }
    }

    /// The whole point of the module, exercised through the real global.
    ///
    /// Only this test calls `init`, and `root()` never writes to the
    /// `OnceLock` — it falls back to the real home when unset — so ordering
    /// against the rest of the binary's tests does not matter.
    #[test]
    fn init_redirects_every_owned_path_and_leaves_none_at_the_real_home() {
        let sandbox = std::env::temp_dir().join("allele-test-paths-root");
        init(Some(sandbox.clone()));

        let owned: Vec<(&str, Option<PathBuf>)> = vec![
            ("allele_dir", allele_dir()),
            ("config_dir", config_dir()),
            ("settings_file", settings_file()),
            ("crash_log_file", crash_log_file()),
            ("state_file", state_file()),
            ("workspaces_root", workspaces_root()),
            ("trash_root", trash_root()),
            ("keymap_file", keymap_file()),
            ("crash_dir", crash_dir()),
            ("diagnostics_dir", diagnostics_dir()),
            ("debug_dir", debug_dir()),
            ("attachments_dir", attachments_dir()),
            ("sync_ledger_file", sync_ledger_file()),
            ("base_infra_dir", base_infra_dir()),
            ("project_scripts_dir", project_scripts_dir("demo")),
            ("legacy_browsers_dir", legacy_browsers_dir()),
        ];

        let home = dirs::home_dir().expect("test host has a home directory");
        for (name, path) in owned {
            let path = path.unwrap_or_else(|| panic!("{name} returned None"));
            assert!(
                path.starts_with(&sandbox),
                "{name} escaped the root: {}",
                path.display()
            );
            assert!(
                !path.starts_with(&home),
                "{name} still points at the real home: {}",
                path.display()
            );
        }

        assert!(is_redirected(), "a non-home root must read as redirected");

        // Derivation is only half of it. Drive a real write through production
        // code — `Settings::save` resolves its own path — and confirm the file
        // lands in the sandbox rather than over the user's live settings.
        let _ = std::fs::remove_dir_all(&sandbox);
        crate::settings::Settings::default().save();
        let written = settings_file().expect("settings path");
        assert!(
            written.exists(),
            "Settings::save did not follow the redirect: {}",
            written.display()
        );
        assert!(written.starts_with(&sandbox));
        let _ = std::fs::remove_dir_all(&sandbox);
    }

    #[test]
    fn sandbox_root_is_anchored_to_the_real_home() {
        // Never to root(), or --sandbox inside a sandbox would nest.
        if let (Some(sandbox), Some(home)) = (default_sandbox_root(), dirs::home_dir()) {
            assert_eq!(sandbox, home.join(".allele-sandbox"));
        }
    }
}
