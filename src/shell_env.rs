//! Login-shell PATH resolution (DEV-16).
//!
//! When Allele is launched from Finder/Dock/Spotlight it inherits launchd's
//! minimal GUI environment (`/usr/bin:/bin:/usr/sbin:/sbin`), so every
//! embedded `claude` session — and everything those sessions spawn (hooks,
//! `gh`, `cargo`, `locus`) — can't find user-installed tools. The fix is
//! the same one VS Code and Zed use: ask the user's login shell for its
//! PATH once at startup and adopt it process-wide before anything spawns.

use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Directories whose absence from PATH marks the environment as
/// launchd-bare. A terminal-launched Allele has at least one of these.
const MARKER_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

/// True when PATH looks like launchd's stripped GUI default.
fn looks_launchd_bare(path: &str) -> bool {
    !MARKER_DIRS
        .iter()
        .any(|dir| path.split(':').any(|entry| entry == *dir))
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

    #[test]
    fn bare_launchd_path_is_detected() {
        assert!(looks_launchd_bare("/usr/bin:/bin:/usr/sbin:/sbin"));
        assert!(looks_launchd_bare(""));
    }

    #[test]
    fn terminal_paths_are_left_alone() {
        assert!(!looks_launchd_bare(
            "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        ));
        assert!(!looks_launchd_bare("/usr/local/bin:/usr/bin:/bin"));
    }

    #[test]
    fn marker_must_match_whole_entry() {
        // A substring like /usr/local/bin-extra must not count.
        assert!(looks_launchd_bare("/usr/local/bin-extra:/usr/bin:/bin"));
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
