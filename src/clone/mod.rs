use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

use crate::errors::AlleleError;
use crate::git;

/// Base directory for all workspace clones
const CLONE_BASE: &str = ".allele/workspaces";

/// Base directory for the trash — orphaned clones are moved here rather
/// than deleted outright, so accidental sweeps are recoverable.
const TRASH_BASE: &str = ".allele/trash";

/// Number of days a trashed clone may sit before being purged on startup.
/// Single source of truth — do not scatter copies of this value.
pub const TRASH_TTL_DAYS: u64 = 14;

/// Create a clone for a session: uses a short unique session ID as the workspace name.
/// Returns the clone path.
pub fn create_session_clone(source: &Path, project_name: &str, session_id: &str) -> crate::errors::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;
    let clone_dir = home.join(CLONE_BASE).join(project_name);
    fs::create_dir_all(&clone_dir)?;

    // Short session ID — first 8 chars of UUID
    let short_id: String = session_id.chars().take(8).collect();
    let clone_path = clone_dir.join(&short_id);

    let final_path = if clone_path.exists() {
        // Unlikely with UUIDs but handle by appending a random suffix
        create_clone(source, &format!("{short_id}-alt"))?
    } else {
        let src_cstr = CString::new(source.to_string_lossy().as_bytes())
            .map_err(|e| AlleleError::Clone(format!("source path contains NUL: {e}")))?;
        let dst_cstr = CString::new(clone_path.to_string_lossy().as_bytes())
            .map_err(|e| AlleleError::Clone(format!("destination path contains NUL: {e}")))?;

        let result = unsafe {
            libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0)
        };

        if result != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EXDEV) {
                return Err(AlleleError::Clone(format!(
                    "Cannot clone: source ({}) and destination ({}) are on different \
                     volumes. Both must be on the same APFS volume for clonefile(2) to work. \
                     Move your project or ~/.allele/ so they share a volume.",
                    source.display(),
                    clone_path.display(),
                )));
            }
            return Err(AlleleError::Clone(format!("clonefile() failed: {err}")));
        }

        clone_path
    };

    // Pre-register the clone as a trusted workspace in ~/.claude.json so
    // Claude Code does not prompt on first entry. Non-fatal: a failure
    // here just means the user sees the trust dialog once, which is the
    // current baseline behaviour.
    if let Err(e) = crate::trust::trust_workspace(&final_path) {
        warn!("trust_workspace({}) failed: {e}", final_path.display());
    }

    Ok(final_path)
}

/// Create an APFS copy-on-write clone of a directory.
///
/// Uses the macOS `clonefile(2)` syscall — near-instant, zero disk cost
/// until files are modified. The clone is a perfect snapshot including
/// untracked files, node_modules, .env, everything.
///
/// Returns the path to the clone.
pub fn create_clone(source: &Path, workspace_name: &str) -> crate::errors::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;

    // Derive a project name from the source path
    let project_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let clone_dir = home
        .join(CLONE_BASE)
        .join(project_name);

    // Ensure parent directory exists
    fs::create_dir_all(&clone_dir)?;

    let clone_path = clone_dir.join(workspace_name);

    // clonefile requires destination does NOT exist
    if clone_path.exists() {
        return Err(AlleleError::Clone(format!(
            "Clone destination already exists: {}",
            clone_path.display()
        )));
    }

    // Call clonefile(2) — macOS APFS copy-on-write clone
    let src_cstr = CString::new(source.to_string_lossy().as_bytes())
        .map_err(|e| AlleleError::Clone(format!("source path contains NUL: {e}")))?;
    let dst_cstr = CString::new(clone_path.to_string_lossy().as_bytes())
        .map_err(|e| AlleleError::Clone(format!("destination path contains NUL: {e}")))?;

    let result = unsafe {
        libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0)
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EXDEV) {
            return Err(AlleleError::Clone(format!(
                "Cannot clone: source ({}) and destination ({}) are on different \
                 volumes. Both must be on the same APFS volume for clonefile(2) to work. \
                 Move your project or ~/.allele/ so they share a volume.",
                source.display(),
                clone_path.display(),
            )));
        }
        return Err(AlleleError::Clone(format!("clonefile() failed: {err}")));
    }

    Ok(clone_path)
}

/// Delete stale runtime artifacts left behind in a fresh session clone.
///
/// APFS `clonefile(2)` is faithful — it copies `.overmind.sock`, Puma pid
/// files and similar per-process state from the parent working copy. Those
/// files make the new session's drawer tabs refuse to start their
/// processes ("Overmind is already running…", "a server is already
/// running…"). This sweep runs immediately after a clone, before any
/// drawer terminal is spawned.
///
/// `paths` are interpreted as relative to `clone_path`. Entries that would
/// escape the clone (via `..` or an absolute component) are skipped with a
/// warning — protects users from a footgun if they edit the config by
/// hand. Missing entries are silently ignored; any other per-entry error
/// is logged but does not abort the sweep.
pub fn cleanup_stale_runtime(clone_path: &Path, paths: &[String]) {
    for entry in paths {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }

        let rel = Path::new(trimmed);
        if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            warn!(
                "cleanup_stale_runtime: refusing entry '{trimmed}' — must be a \
                 relative path with no '..' segments"
            );
            continue;
        }

        let target = clone_path.join(rel);
        let meta = match fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                warn!("cleanup_stale_runtime: stat {} failed: {e}", target.display());
                continue;
            }
        };

        let ft = meta.file_type();
        // Symlinks and regular files use remove_file; directories use
        // remove_dir_all. Sockets/FIFOs are treated as files (remove_file
        // handles them on Unix).
        let result = if ft.is_dir() {
            fs::remove_dir_all(&target)
        } else {
            fs::remove_file(&target)
        };

        if let Err(e) = result {
            warn!("cleanup_stale_runtime: remove {} failed: {e}", target.display());
        }
    }
}

/// Delete a workspace clone outright.
///
/// This is the destructive path — only used via the explicit "Discard"
/// action. Normal session closure trashes the clone instead (see
/// [`trash_clone`]).
pub fn delete_clone(clone_path: &Path) -> crate::errors::Result<()> {
    if !clone_path.exists() {
        return Ok(());
    }

    // Safety check — only delete paths under our workspace directory
    let home = dirs::home_dir()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;
    let workspace_base = home.join(CLONE_BASE);

    if !clone_path.starts_with(&workspace_base) {
        return Err(AlleleError::Clone(format!(
            "Refusing to delete path outside workspace directory: {}",
            clone_path.display()
        )));
    }

    fs::remove_dir_all(clone_path)?;
    Ok(())
}

/// Return the trash base directory, creating it if necessary.
pub fn trash_base() -> crate::errors::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;
    let path = home.join(TRASH_BASE);
    fs::create_dir_all(&path)?;
    Ok(path)
}

/// Move a clone into the trash directory.
///
/// The trash entry is named `<project>-<basename>-<epoch-seconds>` so
/// that collisions are impossible and the original provenance is legible
/// when a user pokes around in `~/.allele/trash/`.
///
/// Safety: refuses to operate on any path outside
/// `~/.allele/workspaces/`.
pub fn trash_clone(clone_path: &Path) -> crate::errors::Result<PathBuf> {
    if !clone_path.exists() {
        return Err(AlleleError::Clone(format!(
            "trash_clone: path does not exist: {}",
            clone_path.display()
        )));
    }

    let home = dirs::home_dir()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;
    let workspace_base = home.join(CLONE_BASE);

    if !clone_path.starts_with(&workspace_base) {
        return Err(AlleleError::Clone(format!(
            "Refusing to trash path outside workspace directory: {}",
            clone_path.display()
        )));
    }

    let trash_dir = trash_base()?;

    let project_name = clone_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let clone_name = clone_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut dest = trash_dir.join(format!("{project_name}-{clone_name}-{epoch}"));
    // Extremely unlikely, but if two sweeps run in the same second, append a counter.
    let mut suffix = 1u32;
    while dest.exists() {
        dest = trash_dir.join(format!("{project_name}-{clone_name}-{epoch}-{suffix}"));
        suffix += 1;
    }

    fs::rename(clone_path, &dest).map_err(|e| {
        if e.raw_os_error() == Some(libc::EXDEV) {
            AlleleError::Clone(format!(
                "Cannot trash clone: source ({}) and trash directory ({}) are on different \
                 volumes. Both must be on the same APFS volume. \
                 Move ~/.allele/ so workspaces and trash share a volume.",
                clone_path.display(),
                dest.display(),
            ))
        } else {
            AlleleError::Io(e)
        }
    })?;
    Ok(dest)
}

/// Delete trash entries older than `ttl_days`. Returns the number of entries
/// actually purged. Errors on individual entries are logged and swallowed —
/// one corrupt directory shouldn't stop the sweep.
pub fn purge_trash_older_than_days(ttl_days: u64) -> crate::errors::Result<usize> {
    let trash_dir = trash_base()?;
    if !trash_dir.exists() {
        return Ok(0);
    }

    let ttl = Duration::from_secs(ttl_days * 24 * 60 * 60);
    let now = SystemTime::now();
    let mut purged = 0usize;

    for entry in fs::read_dir(&trash_dir)? {
        let Ok(entry) = entry else { continue; };
        let path = entry.path();

        let Ok(meta) = entry.metadata() else { continue; };
        let Ok(modified) = meta.modified() else { continue; };

        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < ttl {
            continue;
        }

        if path.is_dir() {
            if let Err(e) = fs::remove_dir_all(&path) {
                warn!("Failed to purge trash entry {}: {e}", path.display());
                continue;
            }
        } else if let Err(e) = fs::remove_file(&path) {
            warn!("Failed to purge trash file {}: {e}", path.display());
            continue;
        }

        purged += 1;
    }

    Ok(purged)
}

/// Walk `~/.allele/workspaces/<project>/*` and move any clone not
/// present in `referenced` into the trash. Conservative — never deletes.
///
/// `project_sources` maps project names to their canonical source paths.
/// If the clone has an `allele/session/<id>` branch and the owning
/// project is in the map, `git::archive_session` runs before trashing
/// to preserve the orphan's session work in canonical. Archive failure
/// is logged and non-blocking — the clone is trashed regardless.
///
/// Returns the number of clones that were trashed.
pub fn sweep_orphans(
    referenced: &HashSet<PathBuf>,
    project_sources: &HashMap<String, PathBuf>,
) -> crate::errors::Result<usize> {
    let home = dirs::home_dir()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;
    let workspace_base = home.join(CLONE_BASE);

    if !workspace_base.exists() {
        return Ok(0);
    }

    let mut trashed = 0usize;

    for proj_entry in fs::read_dir(&workspace_base)? {
        let Ok(proj_entry) = proj_entry else { continue; };
        let Ok(ft) = proj_entry.file_type() else { continue; };
        if !ft.is_dir() {
            continue;
        }

        let proj_dir = proj_entry.path();
        let proj_name = proj_entry
            .file_name()
            .to_string_lossy()
            .to_string();

        let Ok(iter) = fs::read_dir(&proj_dir) else { continue; };

        for clone_entry in iter {
            let Ok(clone_entry) = clone_entry else { continue; };
            let Ok(ft) = clone_entry.file_type() else { continue; };
            if !ft.is_dir() {
                continue;
            }

            let clone_path = clone_entry.path();
            let canonical = fs::canonicalize(&clone_path).unwrap_or_else(|_| clone_path.clone());

            if referenced.contains(&canonical) || referenced.contains(&clone_path) {
                continue;
            }

            // Archive the orphan's session work into canonical before
            // trashing. Resolve canonical from the project name, and
            // session ID from the clone's current branch. Both must
            // succeed for the archive to run; otherwise skip silently.
            if let Some(source_path) = project_sources.get(&proj_name) {
                if let Some(session_id) = git::current_branch(&clone_path)
                    .as_deref()
                    .and_then(git::session_id_from_branch)
                {
                    if let Err(e) = git::archive_session(source_path, &clone_path, session_id) {
                        warn!(
                            "Orphan sweep: archive_session failed for {session_id}: {e}"
                        );
                    }
                }
            }

            match trash_clone(&clone_path) {
                Ok(dest) => {
                    info!(
                        "Orphan sweep: trashed {} → {}",
                        clone_path.display(),
                        dest.display()
                    );
                    trashed += 1;
                }
                Err(e) => {
                    warn!(
                        "Orphan sweep: failed to trash {}: {e}",
                        clone_path.display()
                    );
                }
            }
        }
    }

    Ok(trashed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_tmp(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("allele-test-{tag}-{pid}-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cleanup_removes_known_files_and_skips_missing() {
        let clone = unique_tmp("cleanup-basic");
        fs::write(clone.join(".overmind.sock"), b"").unwrap();
        fs::create_dir_all(clone.join("tmp/pids")).unwrap();
        fs::write(clone.join("tmp/pids/server.pid"), b"12345").unwrap();

        let paths = vec![
            ".overmind.sock".to_string(),
            ".foreman.sock".to_string(), // missing — must be a no-op
            "tmp/pids/server.pid".to_string(),
        ];
        cleanup_stale_runtime(&clone, &paths);

        assert!(!clone.join(".overmind.sock").exists());
        assert!(!clone.join("tmp/pids/server.pid").exists());
        // Parent dir should be left alone — we only delete the leaf entry.
        assert!(clone.join("tmp/pids").exists());

        fs::remove_dir_all(&clone).ok();
    }

    #[test]
    fn cleanup_refuses_parent_dir_escape() {
        let clone = unique_tmp("cleanup-escape");
        let sibling = clone.parent().unwrap().join("should-survive.txt");
        fs::write(&sibling, b"keep me").unwrap();

        // Relative path with .. that would escape — must be rejected.
        let rel = format!("../{}", sibling.file_name().unwrap().to_string_lossy());
        cleanup_stale_runtime(&clone, &[rel]);

        assert!(sibling.exists(), "parent-dir escape must not delete sibling files");

        fs::remove_file(&sibling).ok();
        fs::remove_dir_all(&clone).ok();
    }

    #[test]
    fn cleanup_refuses_absolute_path() {
        let clone = unique_tmp("cleanup-abs");
        let outside = unique_tmp("cleanup-abs-outside").join("victim.txt");
        fs::write(&outside, b"keep me").unwrap();

        cleanup_stale_runtime(&clone, &[outside.to_string_lossy().to_string()]);

        assert!(outside.exists(), "absolute entries must be rejected");

        fs::remove_file(&outside).ok();
        fs::remove_dir_all(outside.parent().unwrap()).ok();
        fs::remove_dir_all(&clone).ok();
    }

    #[test]
    fn cleanup_handles_directory_entry() {
        let clone = unique_tmp("cleanup-dir");
        let dir = clone.join("tmp/cache");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), b"x").unwrap();
        fs::write(dir.join("b"), b"y").unwrap();

        cleanup_stale_runtime(&clone, &["tmp/cache".to_string()]);

        assert!(!dir.exists());
        assert!(clone.join("tmp").exists());

        fs::remove_dir_all(&clone).ok();
    }
}
