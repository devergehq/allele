//! File-descriptor headroom (DEV-328).
//!
//! macOS hands apps launched from Finder/Dock/Spotlight a soft `RLIMIT_NOFILE`
//! of 256 — `launchctl limit maxfiles` reports `256 unlimited`. Allele spends
//! descriptors quickly: every session holds a Claude PTY plus one per terminal
//! tab, and each of those drags along kqueues, sockets and file watchers. Past
//! a couple of dozen sessions the process runs dry and `tty::new` fails with
//! `EMFILE`, which reaches the user as the unhelpful
//! `Failed to create PTY: Too many open files (os error 24)`.
//!
//! The fix is the one every terminal-hosting app applies (Zed, Alacritty,
//! WezTerm, VS Code): raise the soft limit at startup, before anything spawns.

use tracing::{info, warn};

/// Descriptors to ask for.
///
/// 10240 is the traditional macOS `OPEN_MAX` and the value the `rlimit` crate
/// documents for this exact job. It is ~100x what a busy Allele was measured
/// holding (85 descriptors), and deliberately far below `kern.maxfilesperproc`
/// (245760): high enough that no realistic session count reaches it, low enough
/// that a genuine descriptor leak still trips a limit eventually rather than
/// quietly consuming the machine's global `kern.maxfiles` budget.
#[cfg_attr(not(unix), allow(dead_code))]
const DESIRED_SOFT_LIMIT: u64 = 10_240;

/// Decide which soft limit to request, or `None` to leave the process alone.
///
/// Deliberately pure and separate from the syscalls so it can be tested
/// without mutating process-global state — a real `setrlimit` in a unit test
/// would leak across the rest of the suite and make it order-dependent.
///
/// Two cases matter beyond the obvious one. macOS reports the *hard* limit as
/// `RLIM_INFINITY`, so "raise soft to hard" would yield an effectively
/// unbounded limit that masks leaks forever — `desired` has to win. And a
/// hard limit lower than `desired` must cap the request, since asking for more
/// than the kernel allows fails outright rather than clamping.
#[cfg_attr(not(unix), allow(dead_code))]
fn target_soft_limit(current_soft: u64, hard: u64, desired: u64) -> Option<u64> {
    let target = desired.min(hard);
    (target > current_soft).then_some(target)
}

/// Raise this process's soft `RLIMIT_NOFILE` toward [`DESIRED_SOFT_LIMIT`].
///
/// Must run before anything spawns — children inherit the limit in force at
/// fork time, so a late call leaves early PTYs and subprocesses on the old
/// ceiling. Never fatal: if the limit can't be read or raised, Allele carries
/// on with whatever it was given and logs why.
#[cfg(unix)]
pub fn raise_open_file_limit() {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // SAFETY: `getrlimit` only writes the `rlimit` struct we hand it, which is
    // fully initialised and outlives the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) } != 0 {
        warn!(
            "could not read RLIMIT_NOFILE ({}); leaving the descriptor limit alone",
            std::io::Error::last_os_error()
        );
        return;
    }

    let current = limits.rlim_cur;
    let hard = limits.rlim_max;

    let Some(target) = target_soft_limit(current, hard, DESIRED_SOFT_LIMIT) else {
        info!("RLIMIT_NOFILE soft limit is already {current}; leaving it alone");
        return;
    };

    limits.rlim_cur = target as libc::rlim_t;

    // SAFETY: `setrlimit` only reads the `rlimit` struct we hand it.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) } != 0 {
        warn!(
            "could not raise RLIMIT_NOFILE from {current} to {target} ({}); \
             sessions may fail with \"Too many open files\"",
            std::io::Error::last_os_error()
        );
        return;
    }

    info!("raised RLIMIT_NOFILE soft limit {current} -> {target}");
}

#[cfg(not(unix))]
pub fn raise_open_file_limit() {
    // Descriptor limits are a POSIX concern; nothing to do elsewhere.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_default_is_raised() {
        // The case that motivated this: GUI launch, soft 256, hard unlimited.
        assert_eq!(target_soft_limit(256, u64::MAX, 10_240), Some(10_240));
    }

    #[test]
    fn infinite_hard_limit_never_becomes_the_target() {
        // macOS reports hard as RLIM_INFINITY. Adopting it wholesale would
        // hide any descriptor leak indefinitely.
        assert_eq!(target_soft_limit(256, u64::MAX, 10_240), Some(10_240));
        assert_eq!(target_soft_limit(256, 245_760, 10_240), Some(10_240));
    }

    #[test]
    fn low_hard_limit_caps_the_request() {
        // Asking above the hard limit fails rather than clamping, so clamp here.
        assert_eq!(target_soft_limit(256, 512, 10_240), Some(512));
    }

    #[test]
    fn sufficient_soft_limit_is_left_alone() {
        assert_eq!(target_soft_limit(10_240, u64::MAX, 10_240), None);
        // A terminal launch inherits the shell's already-generous limit.
        assert_eq!(target_soft_limit(1_048_576, u64::MAX, 10_240), None);
    }

    #[test]
    fn a_low_hard_limit_never_lowers_the_current_soft_limit() {
        // Pathological, but shrinking the limit we already hold would be worse
        // than doing nothing.
        assert_eq!(target_soft_limit(10_240, 512, 10_240), None);
    }
}
