//! Login-shell PATH resolution (DEV-16, corrected in DEV-424).
//!
//! When Allele is launched from Finder/Dock/Spotlight it inherits launchd's
//! minimal GUI environment, so every embedded `claude` session — and
//! everything those sessions spawn (hooks, `gh`, `cargo`, `locus`) — can't
//! find user-installed tools. The fix is the same one VS Code and Zed use:
//! ask the user's login shell for its PATH once at startup and adopt it
//! process-wide before anything spawns.
//!
//! The subtlety is telling the two launch styles apart. launchd's PATH is not
//! the bare `/usr/bin:/bin:/usr/sbin:/sbin` you might expect — `path_helper`
//! builds it from `/etc/paths` and `/etc/paths.d/*`, and `/usr/local/bin` is
//! line 1 of stock `/etc/paths`. So the presence of any particular system
//! directory says nothing about how we were launched. What distinguishes a
//! terminal launch is an entry `path_helper` could never have produced: those
//! can only come from a shell rc.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// The files `path_helper` reads to assemble a stock macOS PATH.
const PATH_HELPER_FILE: &str = "/etc/paths";
const PATH_HELPER_DIR: &str = "/etc/paths.d";

/// Stock `/etc/paths` contents, used only when the real files are unreadable.
/// Deliberately generous: over-listing risks one needless probe, while
/// under-listing would misread a launchd PATH as a terminal one.
const FALLBACK_STOCK_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/System/Cryptexes/App/usr/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/Library/Apple/usr/bin",
];

/// Collect the newline-separated directories in a `path_helper` config file.
fn absorb_path_file(contents: &str, dirs: &mut BTreeSet<String>) {
    dirs.extend(
        contents
            .lines()
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned),
    );
}

/// Every directory macOS supplies on its own — the union of `/etc/paths` and
/// `/etc/paths.d/*`, which is exactly what launchd hands a GUI app.
fn stock_system_dirs() -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();

    if let Ok(contents) = std::fs::read_to_string(PATH_HELPER_FILE) {
        absorb_path_file(&contents, &mut dirs);
    }
    if let Ok(entries) = std::fs::read_dir(PATH_HELPER_DIR) {
        for entry in entries.flatten() {
            if let Ok(contents) = std::fs::read_to_string(entry.path()) {
                absorb_path_file(&contents, &mut dirs);
            }
        }
    }

    if dirs.is_empty() {
        warn!("could not read {PATH_HELPER_FILE}; using the built-in stock PATH list");
        dirs.extend(FALLBACK_STOCK_DIRS.iter().map(|dir| (*dir).to_owned()));
    }
    dirs
}

/// True when every entry on `path` is a directory macOS itself supplies, so
/// nothing on it came from a shell rc. Entries are matched whole:
/// `/usr/bin-extra` is not `/usr/bin`. An empty PATH counts as bare.
fn is_stock_only(path: &str, stock: &BTreeSet<String>) -> bool {
    path.split(':')
        .filter(|entry| !entry.is_empty())
        .all(|entry| stock.contains(entry))
}

/// True when PATH looks like launchd's GUI default rather than a shell's.
fn looks_launchd_bare(path: &str) -> bool {
    is_stock_only(path, &stock_system_dirs())
}

/// Ask the user's login shell for its PATH, waiting at most `timeout`.
/// Returns None on spawn failure, timeout, or empty output.
fn query_login_shell_path(timeout: Duration) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut child = std::process::Command::new(&shell)
        // -i so ~/.zshrc-style rc files run, -l for the login profile
        // chain (path_helper lives there on macOS).
        .args(["-ilc", r#"command printf "%s" "$PATH""#])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| warn!("login-shell PATH probe failed to spawn {shell}: {e}"))
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => {
                warn!("login-shell PATH probe exited with {status}");
                return None;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    warn!("login-shell PATH probe timed out after {timeout:?}; killing");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                warn!("login-shell PATH probe wait error: {e}");
                return None;
            }
        }
    }

    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    // Interactive shells can print banners; PATH is the last line.
    let path = out.lines().last()?.trim().to_string();
    (!path.is_empty() && path.contains(':')).then_some(path)
}

/// Adopt the login shell's PATH when the inherited environment is
/// launchd-bare. Must run before anything spawns (PTYs, git checks,
/// agent detection) so every child inherits the fixed value.
pub fn fix_launchd_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    if !looks_launchd_bare(&current) {
        return; // terminal launch — leave the environment alone
    }
    match query_login_shell_path(Duration::from_secs(3)) {
        Some(resolved) if resolved != current => {
            info!(
                "launchd-bare PATH detected; adopting login-shell PATH \
                 ({} entries -> {})",
                current.split(':').count(),
                resolved.split(':').count(),
            );
            std::env::set_var("PATH", resolved);
        }
        Some(_) => info!("launchd-bare PATH detected but login shell agrees; leaving as-is"),
        None => warn!("launchd-bare PATH detected but login-shell probe failed; PATH unchanged"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stock() -> BTreeSet<String> {
        FALLBACK_STOCK_DIRS
            .iter()
            .map(|dir| (*dir).to_owned())
            .collect()
    }

    #[test]
    fn bare_launchd_path_is_detected() {
        assert!(is_stock_only("/usr/bin:/bin:/usr/sbin:/sbin", &stock()));
        assert!(is_stock_only("", &stock()));
    }

    #[test]
    fn stock_usr_local_bin_is_not_a_terminal_marker() {
        // The DEV-424 regression: /usr/local/bin is line 1 of stock
        // /etc/paths, so launchd hands it to every GUI app. A PATH built
        // only from stock directories is bare no matter how long it looks.
        assert!(is_stock_only(
            "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            &stock()
        ));
    }

    #[test]
    fn terminal_paths_are_left_alone() {
        // Anything path_helper could not have produced means a shell rc ran.
        assert!(!is_stock_only(
            "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            &stock()
        ));
        assert!(!is_stock_only(
            "/usr/local/bin:/Users/someone/.cargo/bin:/usr/bin:/bin",
            &stock()
        ));
    }

    #[test]
    fn stock_dirs_match_whole_entries() {
        // A near-miss like /usr/bin-extra is user-supplied, not stock.
        assert!(!is_stock_only("/usr/bin-extra:/usr/bin:/bin", &stock()));
    }

    #[test]
    fn path_files_parse_into_directories() {
        let mut dirs = BTreeSet::new();
        absorb_path_file("/usr/local/bin\n\n  /usr/bin  \n", &mut dirs);
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains("/usr/local/bin"));
        assert!(dirs.contains("/usr/bin"));
    }

    #[test]
    fn stock_dirs_include_the_system_defaults() {
        // Reads the real /etc/paths on macOS; falls back if unreadable.
        let dirs = stock_system_dirs();
        assert!(dirs.contains("/usr/bin"));
        assert!(dirs.contains("/usr/local/bin"));
    }

    #[test]
    fn a_path_of_only_real_stock_dirs_is_bare() {
        // End-to-end over this machine's actual /etc/paths + /etc/paths.d:
        // reassembling PATH from nothing but those directories must read as
        // launchd-bare, which is what a Finder launch actually hands us.
        let dirs = stock_system_dirs();
        let path = dirs.iter().cloned().collect::<Vec<_>>().join(":");
        assert!(looks_launchd_bare(&path));
    }

    #[test]
    fn probe_returns_a_plausible_path() {
        // Runs the real login shell; generous timeout for CI-ish envs.
        if let Some(p) = query_login_shell_path(Duration::from_secs(5)) {
            assert!(p.contains(':'));
            assert!(p.split(':').any(|e| e == "/usr/bin"));
        }
    }
}
