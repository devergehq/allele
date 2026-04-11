use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
pub fn create_session_clone(source: &Path, project_name: &str, session_id: &str) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let clone_dir = home.join(CLONE_BASE).join(project_name);
    fs::create_dir_all(&clone_dir)?;

    // Short session ID — first 8 chars of UUID
    let short_id: String = session_id.chars().take(8).collect();
    let clone_path = clone_dir.join(&short_id);

    if clone_path.exists() {
        // Unlikely with UUIDs but handle by appending a random suffix
        return create_clone(source, &format!("{short_id}-alt"));
    }

    let src_cstr = CString::new(source.to_string_lossy().as_bytes())?;
    let dst_cstr = CString::new(clone_path.to_string_lossy().as_bytes())?;

    let result = unsafe {
        libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0)
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("clonefile() failed: {err}");
    }

    Ok(clone_path)
}

/// Create an APFS copy-on-write clone of a directory.
///
/// Uses the macOS `clonefile(2)` syscall — near-instant, zero disk cost
/// until files are modified. The clone is a perfect snapshot including
/// untracked files, node_modules, .env, everything.
///
/// Returns the path to the clone.
pub fn create_clone(source: &Path, workspace_name: &str) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

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
        anyhow::bail!(
            "Clone destination already exists: {}",
            clone_path.display()
        );
    }

    // Call clonefile(2) — macOS APFS copy-on-write clone
    let src_cstr = CString::new(source.to_string_lossy().as_bytes())?;
    let dst_cstr = CString::new(clone_path.to_string_lossy().as_bytes())?;

    let result = unsafe {
        libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0)
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("clonefile() failed: {err}");
    }

    Ok(clone_path)
}

/// Delete a workspace clone outright.
///
/// This is the destructive path — only used via the explicit "Discard"
/// action. Normal session closure trashes the clone instead (see
/// [`trash_clone`]).
pub fn delete_clone(clone_path: &Path) -> anyhow::Result<()> {
    if !clone_path.exists() {
        return Ok(());
    }

    // Safety check — only delete paths under our workspace directory
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let workspace_base = home.join(CLONE_BASE);

    if !clone_path.starts_with(&workspace_base) {
        anyhow::bail!(
            "Refusing to delete path outside workspace directory: {}",
            clone_path.display()
        );
    }

    fs::remove_dir_all(clone_path)?;
    Ok(())
}

/// List all workspace clones for a project
pub fn list_clones(project_name: &str) -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let project_dir = home.join(CLONE_BASE).join(project_name);

    if !project_dir.exists() {
        return Ok(Vec::new());
    }

    let mut clones = Vec::new();
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            clones.push(entry.path());
        }
    }

    Ok(clones)
}

/// Return the trash base directory, creating it if necessary.
pub fn trash_base() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
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
pub fn trash_clone(clone_path: &Path) -> anyhow::Result<PathBuf> {
    if !clone_path.exists() {
        anyhow::bail!("trash_clone: path does not exist: {}", clone_path.display());
    }

    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let workspace_base = home.join(CLONE_BASE);

    if !clone_path.starts_with(&workspace_base) {
        anyhow::bail!(
            "Refusing to trash path outside workspace directory: {}",
            clone_path.display()
        );
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

    fs::rename(clone_path, &dest)?;
    Ok(dest)
}

/// Delete trash entries older than `ttl_days`. Returns the number of entries
/// actually purged. Errors on individual entries are logged and swallowed —
/// one corrupt directory shouldn't stop the sweep.
pub fn purge_trash_older_than_days(ttl_days: u64) -> anyhow::Result<usize> {
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
                eprintln!("Failed to purge trash entry {}: {e}", path.display());
                continue;
            }
        } else if let Err(e) = fs::remove_file(&path) {
            eprintln!("Failed to purge trash file {}: {e}", path.display());
            continue;
        }

        purged += 1;
    }

    Ok(purged)
}

/// Walk `~/.allele/workspaces/<project>/*` and move any clone not
/// present in `referenced` into the trash. Conservative — never deletes.
///
/// Returns the number of clones that were trashed.
pub fn sweep_orphans(referenced: &HashSet<PathBuf>) -> anyhow::Result<usize> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
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

            match trash_clone(&clone_path) {
                Ok(dest) => {
                    eprintln!(
                        "Orphan sweep: trashed {} → {}",
                        clone_path.display(),
                        dest.display()
                    );
                    trashed += 1;
                }
                Err(e) => {
                    eprintln!(
                        "Orphan sweep: failed to trash {}: {e}",
                        clone_path.display()
                    );
                }
            }
        }
    }

    Ok(trashed)
}
