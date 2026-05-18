//! Memory watchdog — monitors process RSS and force-quits on runaway leaks.
//!
//! Spawns a background async task that checks RSS every 30 seconds. Logs a
//! warning at the soft limit (4 GB) and writes a crash report + exits at
//! the hard limit (8 GB). The crash report is written to
//! `~/.allele/crash/` so the user can inspect it after restart.

use std::fs;
use std::path::PathBuf;
use tracing::{error, info, warn};

const SOFT_LIMIT_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GB
const HARD_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024; // 8 GB
const CHECK_INTERVAL_SECS: u64 = 30;
const LOG_INTERVAL_SECS: u64 = 300; // 5 minutes

/// Read the current process's resident set size in bytes (macOS only).
/// Returns `None` on non-macOS or if the syscall fails.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // mach_task_self — avoiding mach2 crate dependency
pub fn process_rss_bytes() -> Option<u64> {
    use libc::{mach_task_self, task_info, MACH_TASK_BASIC_INFO, mach_task_basic_info};
    use std::mem;

    unsafe {
        let mut info: mach_task_basic_info = mem::zeroed();
        let mut count = (mem::size_of::<mach_task_basic_info>() / mem::size_of::<libc::natural_t>())
            as libc::mach_msg_type_number_t;
        let kr = task_info(
            mach_task_self(),
            MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as *mut libc::integer_t,
            &mut count,
        );
        if kr == libc::KERN_SUCCESS {
            Some(info.resident_size as u64)
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn process_rss_bytes() -> Option<u64> {
    None
}

fn crash_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".allele").join("crash"))
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn write_crash_report(rss: u64) {
    let Some(dir) = crash_dir() else { return };
    if let Err(e) = fs::create_dir_all(&dir) {
        error!("Failed to create crash directory: {e}");
        return;
    }

    let ts = chrono_timestamp();
    let filename = format!("memory-limit-{ts}.txt");
    let path = dir.join(&filename);

    let report = format!(
        "Allele Memory Limit Crash Report\n\
         =================================\n\
         Timestamp: {ts}\n\
         Process RSS: {rss_fmt}\n\
         Hard limit: {limit_fmt}\n\
         \n\
         The Allele process exceeded its memory safety limit and was\n\
         force-terminated to prevent system instability.\n\
         \n\
         This indicates a memory leak. Please report this at:\n\
         https://github.com/devergehq/allele/issues\n\
         \n\
         Diagnostic hints:\n\
         - Number of sessions that were running at the time\n\
         - How long Allele had been running\n\
         - Whether sessions were actively working or idle\n\
         - macOS Activity Monitor memory breakdown if available\n",
        rss_fmt = format_bytes(rss),
        limit_fmt = format_bytes(HARD_LIMIT_BYTES),
    );

    match fs::write(&path, &report) {
        Ok(()) => error!("Crash report written to {}", path.display()),
        Err(e) => error!("Failed to write crash report: {e}"),
    }
}

fn chrono_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple ISO-ish format without pulling in chrono
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Approximate date from epoch days (good enough for filenames)
    let y = 1970 + days / 365;
    let d = days % 365;
    format!("{y:04}-d{d:03}T{h:02}{m:02}{s:02}")
}

/// Spawn the memory watchdog on GPUI's background executor. Runs for the
/// lifetime of the app. Checks RSS every 30 seconds and force-quits if
/// the hard limit is exceeded.
pub fn spawn(cx: &gpui::App) {
    let executor = cx.background_executor().clone();
    let timer_executor = executor.clone();

    executor
        .spawn(async move {
            let mut last_rss: u64 = 0;
            let mut last_log_time = std::time::Instant::now();
            let mut soft_warned = false;

            loop {
                timer_executor
                    .timer(std::time::Duration::from_secs(CHECK_INTERVAL_SECS))
                    .await;

                let Some(rss) = process_rss_bytes() else {
                    continue;
                };

                let delta = if last_rss > 0 {
                    rss as i64 - last_rss as i64
                } else {
                    0
                };

                // Periodic info-level log every 5 minutes
                if last_log_time.elapsed().as_secs() >= LOG_INTERVAL_SECS {
                    let delta_str = if delta >= 0 {
                        format!("+{}", format_bytes(delta as u64))
                    } else {
                        format!("-{}", format_bytes((-delta) as u64))
                    };
                    info!(
                        "memory: RSS={} (delta={delta_str})",
                        format_bytes(rss),
                    );
                    last_log_time = std::time::Instant::now();
                }

                last_rss = rss;

                // Soft limit warning
                if rss >= SOFT_LIMIT_BYTES && !soft_warned {
                    warn!(
                        "MEMORY WARNING: RSS={} exceeds soft limit of {}",
                        format_bytes(rss),
                        format_bytes(SOFT_LIMIT_BYTES),
                    );
                    soft_warned = true;
                }

                // Hard limit — write crash report and exit
                if rss >= HARD_LIMIT_BYTES {
                    error!(
                        "MEMORY LIMIT EXCEEDED: RSS={} exceeds hard limit of {}. \
                         Writing crash report and exiting.",
                        format_bytes(rss),
                        format_bytes(HARD_LIMIT_BYTES),
                    );

                    write_crash_report(rss);

                    // Show a dialog so the user knows what happened
                    crate::hooks::show_fatal_dialog(
                        "Allele — Memory Safety Limit",
                        &format!(
                            "Allele was using {} of memory and has been stopped \
                             to protect your system.\n\n\
                             A crash report has been saved to ~/.allele/crash/.\n\n\
                             This is a known issue being investigated. \
                             Please restart Allele.",
                            format_bytes(rss),
                        ),
                    );

                    std::process::exit(99);
                }
            }
        })
        .detach();
}
