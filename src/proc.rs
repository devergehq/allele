//! Spawning fire-and-forget child processes without leaking zombies.
//!
//! Allele shells out for side effects it never reads back: a notification
//! sound, a `display notification`, handing a URL or a path to `open`. The
//! obvious way to write that is `Command::new(..).spawn()` and drop the
//! handle — and it leaks, because **Rust does not reap on drop**. `Child`'s
//! documentation says so plainly: the child becomes a zombie.
//!
//! On a machine running twenty-five sessions this is not theoretical. Measured
//! on a live instance on 19 September 2026: 3,351 zombies in the process tree,
//! 1,869 of them direct children of Allele, all from hook sounds and
//! notifications (DEV-685). Nothing reclaims them until the app exits.
//!
//! ## Why not `SIGCHLD` / `SIG_IGN`
//!
//! The one-line Unix answer is to ignore `SIGCHLD` so the kernel auto-reaps.
//! It was measured rather than assumed, and it is wrong here: under `SIG_IGN`
//! an explicit `waitpid` returns `ECHILD`, so it would break every call site
//! that legitimately waits — `shell_env`'s PATH probe, the startup command's
//! `child.wait()`, and, fatally, alacritty's `EventLoop` reaping the PTY
//! child. That last one is the terminal, which is the app.
//!
//! ```text
//! SIGCHLD = SIG_DFL   waitpid OK    -> (82403, 0)
//! SIGCHLD = SIG_IGN   waitpid FAILS -> ECHILD (No child processes)
//! ```
//!
//! So the reaping is per-child and local: one short-lived thread that does
//! nothing but `wait()`. It costs a thread for the lifetime of a notification
//! sound, holds no shared state, and leaves every other `wait()` caller in the
//! process working exactly as before.

use std::process::Command;

use tracing::warn;

/// Spawn `command` and reap it on a detached thread.
///
/// Returns the child's pid on success. The caller gets no handle and no exit
/// status by design — this is for side effects whose result nobody reads. Use
/// `Command::spawn` directly when the status matters, and wait on it.
///
/// The thread blocks in `wait()` until the child exits, so it lives exactly as
/// long as the child does.
pub(crate) fn spawn_and_reap(mut command: Command) -> std::io::Result<u32> {
    let child = command.spawn()?;
    let pid = child.id();

    let reaper = std::thread::Builder::new()
        .name(format!("reap-{pid}"))
        .spawn(move || {
            let mut child = child;
            if let Err(e) = child.wait() {
                warn!("reaping pid {pid} failed: {e}");
            }
        });

    if let Err(e) = reaper {
        // Thread creation failed — the child is spawned and now unreapable
        // from here. Say so rather than dropping it silently, which is the
        // bug this module exists to fix.
        warn!("could not start reaper thread for pid {pid}: {e} (process will linger)");
    }

    Ok(pid)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// True while `pid` is still in the process table. A **zombie answers
    /// this with success** — it has exited but has not been reaped, so its
    /// entry survives. Once reaped, the pid is gone and this is false.
    fn pid_exists(pid: u32) -> bool {
        // Signal 0 performs error checking without sending anything.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn wait_until_gone(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !pid_exists(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn quiet(program: &str) -> Command {
        let mut c = Command::new(program);
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }

    /// The control. Without this module, dropping the handle leaves a zombie —
    /// and this is what proves the check below can actually detect one. If
    /// Rust ever started reaping on drop this test would fail, and the one
    /// after it would become meaningless without anyone noticing.
    #[test]
    fn dropping_the_handle_leaves_a_zombie() {
        let child = quiet("/usr/bin/true").spawn().expect("spawn true");
        let pid = child.id();
        drop(child);

        // `true` exits immediately; give it room on a loaded machine.
        std::thread::sleep(Duration::from_millis(300));

        assert!(
            pid_exists(pid),
            "pid {pid} vanished without being waited on — either the platform \
             reaped it for us or pid_exists() is not detecting zombies, and \
             either way spawn_and_reap_leaves_no_zombie is no longer proving \
             anything"
        );
    }

    #[test]
    fn spawn_and_reap_leaves_no_zombie() {
        let pid = spawn_and_reap(quiet("/usr/bin/true")).expect("spawn true");

        assert!(
            wait_until_gone(pid, Duration::from_secs(5)),
            "pid {pid} was still in the process table after 5s — it was never \
             reaped"
        );
    }

    #[test]
    fn many_spawns_leave_no_zombies() {
        // The leak shape that produced 1,869 zombies was volume, not a single
        // missed wait. One sound per hook event, thousands of events.
        let pids: Vec<u32> = (0..32)
            .map(|_| spawn_and_reap(quiet("/usr/bin/true")).expect("spawn true"))
            .collect();

        let leaked: Vec<u32> = pids
            .into_iter()
            .filter(|pid| !wait_until_gone(*pid, Duration::from_secs(5)))
            .collect();

        assert!(leaked.is_empty(), "unreaped pids: {leaked:?}");
    }

    #[test]
    fn spawn_failure_is_reported_to_the_caller() {
        let err = spawn_and_reap(quiet("/nonexistent/allele-test-binary"));
        assert!(
            err.is_err(),
            "a command that cannot be spawned must not report success"
        );
    }
}
