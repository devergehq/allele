// Per-task Chrome tab linking via AppleScript.
//
// Allele runs alongside the user's real Google Chrome (split-screen). Each
// Allele session has one Chrome tab. Switching sessions activates the
// corresponding tab; creating a session lazily creates a tab on first
// Browser-tab use.
//
// All communication goes through `osascript`. The first call that targets
// Chrome will trigger a one-time Automation permission prompt in
// System Settings → Privacy & Security → Automation.

use std::process::{Command, Stdio};

/// True if Google Chrome's main process is running. Used so the Browser
/// tab UI can distinguish "Chrome not running" from "script failed".
pub fn chrome_running() -> bool {
    Command::new("pgrep")
        .arg("-x")
        .arg("Google Chrome")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run an AppleScript snippet via `osascript -e`. Returns the stdout
/// trimmed on success, None on any failure. Errors are logged.
fn run_osascript(script: &str) -> Option<String> {
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        tracing::warn!("browser: osascript failed ({}): {}", out.status, err.trim());
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create a new tab in Chrome's frontmost window at `url`. Returns the
/// new tab's integer id on success.
pub fn create_tab(url: &str) -> Option<i64> {
    // Escape double quotes in the URL so the AppleScript literal is safe.
    let safe_url = url.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        r#"tell application "Google Chrome"
            if (count of windows) = 0 then
                make new window
            end if
            set t to make new tab at end of tabs of window 1 with properties {{URL:"{safe_url}"}}
            set index of window 1 to 1
            activate
            return (id of t) as string
        end tell"#
    );
    let out = run_osascript(&script)?;
    out.parse::<i64>().ok()
}

/// Activate the tab with `id`. Brings Chrome to the foreground. Returns
/// true on success, false if the id is stale or Chrome is unavailable.
///
/// Compares ids as strings — AppleScript silently promotes integer
/// literals above ~1.07 billion to `real`, and `real is integer`
/// returns false, so numeric comparison misses every real tab id.
pub fn activate_tab(id: i64) -> bool {
    let script = format!(
        r#"tell application "Google Chrome"
            set target to "{id}"
            repeat with w in windows
                set i to 0
                repeat with t in tabs of w
                    set i to i + 1
                    if ((id of t) as string) is target then
                        set active tab index of w to i
                        set index of w to 1
                        activate
                        return "ok"
                    end if
                end repeat
            end repeat
            return "missing"
        end tell"#
    );
    matches!(run_osascript(&script).as_deref(), Some("ok"))
}

/// Navigate the tab with `id` to `url`. Does not bring Chrome to the
/// foreground — callers that want focus should call `activate_tab`
/// afterwards. Returns true on success.
pub fn navigate_tab(id: i64, url: &str) -> bool {
    let safe_url = url.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        r#"tell application "Google Chrome"
            set target to "{id}"
            repeat with w in windows
                repeat with t in tabs of w
                    if ((id of t) as string) is target then
                        set URL of t to "{safe_url}"
                        return "ok"
                    end if
                end repeat
            end repeat
            return "missing"
        end tell"#
    );
    matches!(run_osascript(&script).as_deref(), Some("ok"))
}

/// Close the tab with `id`. Best-effort; returns true if the tab was
/// found and closed.
pub fn close_tab(id: i64) -> bool {
    let script = format!(
        r#"tell application "Google Chrome"
            set target to "{id}"
            repeat with w in windows
                repeat with t in tabs of w
                    if ((id of t) as string) is target then
                        close t
                        return "ok"
                    end if
                end repeat
            end repeat
            return "missing"
        end tell"#
    );
    matches!(run_osascript(&script).as_deref(), Some("ok"))
}
