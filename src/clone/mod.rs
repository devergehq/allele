use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

use crate::errors::AlleleError;
use crate::git;

/// Number of days a trashed clone may sit before being purged on startup.
/// Single source of truth — do not scatter copies of this value.
pub const TRASH_TTL_DAYS: u64 = 14;

/// Absolute path to the workspaces root (`~/.allele/workspaces`) — the parent
/// directory of every session clone. Single source of truth for the base that
/// session-sync path normalization strips to / rebases from (see `crate::sync`).
pub fn clones_root() -> Option<PathBuf> {
    crate::paths::workspaces_root()
}

/// Create a clone for a session: uses a short unique session ID as the workspace name.
/// Entries whose top-level name matches an `exclude` path are skipped entirely,
/// avoiding the cost of cloning directories that would be deleted immediately after.
/// Returns the clone path.
pub fn create_session_clone(
    source: &Path,
    project_name: &str,
    session_id: &str,
    exclude: &[String],
) -> crate::errors::Result<PathBuf> {
    let clone_dir = crate::paths::workspaces_root()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?
        .join(project_name);
    fs::create_dir_all(&clone_dir)?;

    let short_id: String = session_id.chars().take(8).collect();
    let clone_path = clone_dir.join(&short_id);

    let dest = if clone_path.exists() {
        let alt = clone_dir.join(format!("{short_id}-alt"));
        if alt.exists() {
            return Err(AlleleError::Clone(format!(
                "Clone destination already exists: {}",
                alt.display()
            )));
        }
        alt
    } else {
        clone_path
    };

    selective_clone(source, &dest, exclude)?;

    if let Err(e) = crate::trust::trust_workspace(&dest) {
        warn!("trust_workspace({}) failed: {e}", dest.display());
    }

    Ok(dest)
}

/// Most entries a single `clonefile(2)` call may cover.
///
/// APFS blocks every other process's metadata operations on the volume
/// (create, rename, unlink — and so any app's database save) for the whole
/// of one `clonefile` call. One call over tc-portal's `node_modules`
/// (271k entries) took 15.4s and stalled an unrelated `rename()` for all
/// 15.4s of it (DEV-755). At ~57µs per entry, 2,000 entries caps each stall
/// near 115ms.
const CLONE_CHUNK_ENTRIES: usize = 2_000;

/// Pause taken after each chunk's worth of work, so the operations other
/// processes queued behind it run before the next call takes the volume.
const CHUNK_PAUSE: Duration = Duration::from_millis(5);

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`; not exported by the libc
/// crate. Clone a symlink as a link rather than cloning what it points at.
const CLONE_NOFOLLOW: u32 = 0x0001;

/// Serialises session clones process-wide. Chunking bounds each stall;
/// running one clone at a time stops a dispatch burst stacking them.
static CLONE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Counts work done since the last pause and sleeps once it reaches a
/// chunk's worth.
struct Pacer {
    since_pause: usize,
}

impl Pacer {
    fn new() -> Self {
        Self { since_pause: 0 }
    }

    fn account(&mut self, entries: usize) {
        self.since_pause += entries;
        if self.since_pause >= CLONE_CHUNK_ENTRIES {
            std::thread::sleep(CHUNK_PAUSE);
            self.since_pause = 0;
        }
    }
}

/// Clone `source` into `dest`, skipping top-level entries whose name
/// appears in `exclude`. The tree is cloned in chunks of at most
/// [`CLONE_CHUNK_ENTRIES`] entries so no single `clonefile(2)` call holds
/// the volume for long. A failure removes the partial destination.
fn selective_clone(source: &Path, dest: &Path, exclude: &[String]) -> crate::errors::Result<()> {
    let _guard = CLONE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let result = clone_top_level(source, dest, exclude);
    if result.is_err() && dest.exists() {
        warn!(
            "selective_clone: cleaning up partial clone at {}",
            dest.display()
        );
        let _ = remove_tree_paced(dest);
    }
    result
}

fn clone_top_level(source: &Path, dest: &Path, exclude: &[String]) -> crate::errors::Result<()> {
    let skip: HashSet<&str> = exclude
        .iter()
        .filter_map(|p| {
            let rel = Path::new(p.trim());
            // Only top-level entries can be skipped at clone time.
            if rel.components().count() == 1 {
                rel.file_name().and_then(|n| n.to_str())
            } else {
                None
            }
        })
        .collect();

    let mut pacer = Pacer::new();

    // The source itself is followed if it is a link; everything beneath it
    // is cloned without following.
    if skip.is_empty() && count_entries_up_to(source, CLONE_CHUNK_ENTRIES) <= CLONE_CHUNK_ENTRIES {
        return clonefile_path(source, dest, 0);
    }

    fs::create_dir(dest)?;

    let entries = fs::read_dir(source)
        .map_err(|e| AlleleError::Clone(format!("cannot read source {}: {e}", source.display())))?;

    let mut skipped = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| AlleleError::Clone(format!("read_dir error: {e}")))?;
        let name = entry.file_name();

        if skip.contains(name.to_string_lossy().as_ref()) {
            skipped += 1;
            continue;
        }

        // `.git` stays one call: split into chunks, a commit or fetch in the
        // source mid-clone could leave refs naming objects the clone never
        // got. Its stall is the price of a consistent repository.
        if name == ".git" {
            clonefile_path(&entry.path(), &dest.join(&name), CLONE_NOFOLLOW)?;
            continue;
        }

        clone_chunked(&entry.path(), &dest.join(&name), &mut pacer)?;
    }

    if skipped > 0 {
        info!("selective_clone: skipped {skipped} excluded entries");
    }

    copy_permissions(source, dest)
}

/// Clone `src` to `dst` without following links. A subtree within the
/// chunk budget is one `clonefile(2)` call; a larger directory is
/// recreated and its children cloned one by one.
fn clone_chunked(src: &Path, dst: &Path, pacer: &mut Pacer) -> crate::errors::Result<()> {
    let is_dir = fs::symlink_metadata(src)
        .map_err(|e| AlleleError::Clone(format!("cannot stat {}: {e}", src.display())))?
        .is_dir();

    let count = if is_dir {
        count_entries_up_to(src, CLONE_CHUNK_ENTRIES)
    } else {
        0
    };
    if count <= CLONE_CHUNK_ENTRIES {
        clonefile_path(src, dst, CLONE_NOFOLLOW)?;
        pacer.account(count + 1);
        return Ok(());
    }

    // Created with default permissions and given the source's only once
    // filled: a read-only source directory (e.g. Go's module cache) would
    // otherwise refuse its own children.
    fs::create_dir(dst)?;
    pacer.account(1);
    let entries = fs::read_dir(src)
        .map_err(|e| AlleleError::Clone(format!("cannot read {}: {e}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| AlleleError::Clone(format!("read_dir error: {e}")))?;
        clone_chunked(&entry.path(), &dst.join(entry.file_name()), pacer)?;
    }
    copy_permissions(src, dst)
}

/// Number of entries beneath `dir`, not following links. Stops counting
/// once past `limit`, so the walk costs at most `limit` entries; an
/// unreadable directory counts as over the limit, which sends the caller
/// down the per-entry path where the real error surfaces.
fn count_entries_up_to(dir: &Path, limit: usize) -> usize {
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            return limit + 1;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return limit + 1;
            };
            count += 1;
            if count > limit {
                return count;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    count
}

/// Give `dst` the permission bits of `src`.
fn copy_permissions(src: &Path, dst: &Path) -> crate::errors::Result<()> {
    let meta = fs::metadata(src)
        .map_err(|e| AlleleError::Clone(format!("cannot stat source {}: {e}", src.display())))?;
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()))?;
    Ok(())
}

/// Remove a tree the way `fs::remove_dir_all` does, pausing every chunk's
/// worth of unlinks. Deleting a 271k-entry clone in one go stalled other
/// processes' file operations for up to 8.4s (DEV-755).
pub fn remove_tree_paced(path: &Path) -> std::io::Result<()> {
    let mut pacer = Pacer::new();
    remove_paced(path, &mut pacer)
}

fn remove_paced(path: &Path, pacer: &mut Pacer) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        fs::remove_file(path)?;
        pacer.account(1);
        return Ok(());
    }
    // A read-only directory (e.g. Go's module cache) refuses unlinks of its
    // children until its owner can write to it.
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_paced(&entry.path(), pacer)?;
        } else {
            fs::remove_file(entry.path())?;
            pacer.account(1);
        }
    }
    fs::remove_dir(path)?;
    pacer.account(1);
    Ok(())
}

/// `clonefile(2)` wrapper for a single path (file or directory).
fn clonefile_path(src: &Path, dst: &Path, flags: u32) -> crate::errors::Result<()> {
    let src_cstr = CString::new(src.to_string_lossy().as_bytes())
        .map_err(|e| AlleleError::Clone(format!("source path contains NUL: {e}")))?;
    let dst_cstr = CString::new(dst.to_string_lossy().as_bytes())
        .map_err(|e| AlleleError::Clone(format!("destination path contains NUL: {e}")))?;

    let result = unsafe { libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), flags) };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EXDEV) {
            return Err(AlleleError::Clone(format!(
                "Cannot clone: source ({}) and destination ({}) are on different \
                 volumes. Both must be on the same APFS volume for clonefile(2) to work. \
                 Move your project or ~/.allele/ so they share a volume.",
                src.display(),
                dst.display(),
            )));
        }
        return Err(AlleleError::Clone(format!(
            "clonefile({} → {}) failed: {err}",
            src.display(),
            dst.display(),
        )));
    }

    Ok(())
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
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
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
                warn!(
                    "cleanup_stale_runtime: stat {} failed: {e}",
                    target.display()
                );
                continue;
            }
        };

        let ft = meta.file_type();
        // Symlinks and regular files use remove_file; directories use
        // remove_dir_all. Sockets/FIFOs are treated as files (remove_file
        // handles them on Unix).
        let result = if ft.is_dir() {
            remove_tree_paced(&target)
        } else {
            fs::remove_file(&target)
        };

        if let Err(e) = result {
            warn!(
                "cleanup_stale_runtime: remove {} failed: {e}",
                target.display()
            );
        }
    }
}

/// Post-clone file work every session-create path does: sweep stale
/// runtime files, write the `.allele-session` marker, and keep it out of
/// git. Blocking I/O — call it from a background task, never the UI
/// thread, where it would stall behind another clone holding the volume.
pub fn prepare_session_workspace(clone_path: &Path, session_id: &str, cleanup_paths: &[String]) {
    // Purge stale runtime files (Overmind/Foreman sockets, server pid files,
    // etc.) that clonefile(2) faithfully copied. Must happen before any
    // drawer tab spawns its command.
    cleanup_stale_runtime(clone_path, cleanup_paths);

    // Marker file for orphan cleanup identification.
    if let Err(e) = fs::write(clone_path.join(".allele-session"), session_id) {
        warn!("failed to write .allele-session marker: {e}");
    }

    // Exclude the marker from git so auto-commit never captures it.
    git::exclude_pattern_in_clone(clone_path, ".allele-session");
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
    let workspace_base = crate::paths::workspaces_root()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;

    if !clone_path.starts_with(&workspace_base) {
        return Err(AlleleError::Clone(format!(
            "Refusing to delete path outside workspace directory: {}",
            clone_path.display()
        )));
    }

    remove_tree_paced(clone_path)?;
    Ok(())
}

/// Return the trash base directory, creating it if necessary.
pub fn trash_base() -> crate::errors::Result<PathBuf> {
    let path = crate::paths::trash_root()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;
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

    let workspace_base = crate::paths::workspaces_root()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;

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
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };

        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age < ttl {
            continue;
        }

        if path.is_dir() {
            if let Err(e) = remove_tree_paced(&path) {
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
/// Resolve the session ID from an orphaned clone using multiple strategies:
/// 1. `.allele-session` marker file (new sessions)
/// 2. Legacy branch prefix `allele/session/<id>`
/// 3. Clone directory name (8-hex short ID — partial, best-effort)
fn resolve_session_id_for_orphan(clone_path: &Path) -> Option<String> {
    // Strategy 1: marker file contains the full UUID
    let marker = clone_path.join(".allele-session");
    if let Ok(content) = fs::read_to_string(&marker) {
        let id = content.trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }

    // Strategy 2: legacy branch prefix
    if let Some(branch) = git::current_branch(clone_path) {
        if let Some(id) = git::session_id_from_branch(&branch) {
            return Some(id.to_string());
        }
    }

    // Strategy 3: directory name is the 8-char short ID — usable for archive
    // ref naming but not a full UUID. Still better than nothing.
    clone_path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.len() == 8 && n.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|s| s.to_string())
}

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
    let workspace_base = crate::paths::workspaces_root()
        .ok_or_else(|| AlleleError::Clone("Could not determine home directory".to_string()))?;

    if !workspace_base.exists() {
        return Ok(0);
    }

    let mut trashed = 0usize;

    for proj_entry in fs::read_dir(&workspace_base)? {
        let Ok(proj_entry) = proj_entry else {
            continue;
        };
        let Ok(ft) = proj_entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }

        let proj_dir = proj_entry.path();
        let proj_name = proj_entry.file_name().to_string_lossy().to_string();

        let Ok(iter) = fs::read_dir(&proj_dir) else {
            continue;
        };

        for clone_entry in iter {
            let Ok(clone_entry) = clone_entry else {
                continue;
            };
            let Ok(ft) = clone_entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }

            let clone_path = clone_entry.path();
            let canonical = fs::canonicalize(&clone_path).unwrap_or_else(|_| clone_path.clone());

            if referenced.contains(&canonical) || referenced.contains(&clone_path) {
                continue;
            }

            // Archive the orphan's session work into canonical before
            // trashing. Resolve session ID from: (1) .allele-session marker
            // file, (2) legacy branch prefix, (3) clone directory name.
            if let Some(source_path) = project_sources.get(&proj_name) {
                let session_id = resolve_session_id_for_orphan(&clone_path);
                if let Some(session_id) = session_id.as_deref() {
                    if let Err(e) = git::archive_session(source_path, &clone_path, session_id) {
                        warn!("Orphan sweep: archive_session failed for {session_id}: {e}");
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

    /// Every entry beneath `root` as (relative path, kind, mode, link target),
    /// sorted, without following links.
    fn snapshot(root: &Path) -> Vec<(PathBuf, &'static str, u32, Option<PathBuf>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let mode = meta.permissions().mode() & 0o7777;
                if meta.file_type().is_symlink() {
                    out.push((rel, "link", 0, Some(fs::read_link(&path).unwrap())));
                } else if meta.is_dir() {
                    out.push((rel, "dir", mode, None));
                    stack.push(path);
                } else {
                    out.push((rel, "file", mode, None));
                }
            }
        }
        out.sort();
        out
    }

    /// A tree large enough to force the chunked path at every level that
    /// matters: a directory over the budget holding one directory that is
    /// itself over the budget, plus a small one, a symlink to a directory,
    /// and a directory with non-default permissions.
    fn build_large_tree(root: &Path) {
        let big = root.join("node_modules");
        let bigger = big.join(".pnpm");
        fs::create_dir_all(&bigger).unwrap();
        for i in 0..(CLONE_CHUNK_ENTRIES + 50) {
            fs::write(bigger.join(format!("f{i}")), b"x").unwrap();
        }
        fs::create_dir_all(big.join("small")).unwrap();
        fs::write(big.join("small/index.js"), b"module.exports = 1").unwrap();
        std::os::unix::fs::symlink(".pnpm", big.join("linked")).unwrap();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("a"), b"a").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o750)).unwrap();
        fs::set_permissions(&big, fs::Permissions::from_mode(0o700)).unwrap();
        // Read-only and over the budget, like Go's module cache.
        let ro = root.join("go-mod");
        fs::create_dir(&ro).unwrap();
        for i in 0..(CLONE_CHUNK_ENTRIES + 10) {
            fs::write(ro.join(format!("m{i}")), b"m").unwrap();
        }
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        fs::write(root.join("README"), b"hi").unwrap();
        let objects = root.join(".git/objects");
        fs::create_dir_all(&objects).unwrap();
        for i in 0..(CLONE_CHUNK_ENTRIES + 5) {
            fs::write(objects.join(format!("o{i}")), b"o").unwrap();
        }
    }

    /// Undo the read-only directory [`build_large_tree`] creates, so the
    /// test can clean up after itself.
    fn unlock(root: &Path) {
        let ro = root.join("go-mod");
        if ro.exists() {
            fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn chunked_clone_reproduces_the_tree_exactly() {
        let base = unique_tmp("chunked-clone");
        let src = base.join("src");
        fs::create_dir(&src).unwrap();
        build_large_tree(&src);
        let dst = base.join("dst");

        selective_clone(&src, &dst, &[]).unwrap();

        assert_eq!(snapshot(&src), snapshot(&dst));
        // The link was cloned as a link, not followed into a copy.
        assert!(fs::symlink_metadata(dst.join("node_modules/linked"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read(dst.join("node_modules/small/index.js")).unwrap(),
            b"module.exports = 1"
        );

        unlock(&src);
        unlock(&dst);
        remove_tree_paced(&base).unwrap();
        assert!(!base.exists());
    }

    #[test]
    fn chunked_clone_still_skips_excluded_top_level_entries() {
        let base = unique_tmp("chunked-exclude");
        let src = base.join("src");
        fs::create_dir(&src).unwrap();
        build_large_tree(&src);
        fs::create_dir(src.join("target")).unwrap();
        fs::write(src.join("target/out"), b"o").unwrap();
        let dst = base.join("dst");

        selective_clone(&src, &dst, &["target".to_string()]).unwrap();

        assert!(!dst.join("target").exists());
        assert!(dst.join("node_modules/.pnpm/f0").exists());

        unlock(&src);
        unlock(&dst);
        remove_tree_paced(&base).unwrap();
    }

    #[test]
    fn failed_clone_removes_partial_destination() {
        let base = unique_tmp("chunked-fail");
        let src = base.join("src");
        fs::create_dir(&src).unwrap();
        build_large_tree(&src);
        // Unreadable directory past a large sibling: the clone fails midway.
        let blocked = src.join("zz-blocked");
        fs::create_dir(&blocked).unwrap();
        for i in 0..(CLONE_CHUNK_ENTRIES + 1) {
            fs::write(blocked.join(format!("b{i}")), b"b").unwrap();
        }
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let dst = base.join("dst");

        let result = selective_clone(&src, &dst, &[]);

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert!(!dst.exists(), "partial clone must be removed");
        unlock(&src);
        remove_tree_paced(&base).unwrap();
    }

    /// Clone and delete a real tree, for measuring stalls with an external
    /// probe (DEV-755). `ALLELE_CLONE_BENCH_SRC` is the tree to clone and
    /// `ALLELE_CLONE_BENCH_DST` a path on the same volume that does not exist.
    #[test]
    #[ignore = "benchmark: needs ALLELE_CLONE_BENCH_SRC and ALLELE_CLONE_BENCH_DST"]
    fn bench_chunked_clone_of_real_tree() {
        let src = PathBuf::from(std::env::var("ALLELE_CLONE_BENCH_SRC").unwrap());
        let dst = PathBuf::from(std::env::var("ALLELE_CLONE_BENCH_DST").unwrap());

        let started = std::time::Instant::now();
        selective_clone(&src, &dst, &[]).unwrap();
        let cloned_in = started.elapsed();

        let (src_n, dst_n) = (
            count_entries_up_to(&src, usize::MAX - 1),
            count_entries_up_to(&dst, usize::MAX - 1),
        );
        println!("clone: {cloned_in:?}, entries src={src_n} dst={dst_n}");
        assert_eq!(src_n, dst_n);

        let started = std::time::Instant::now();
        remove_tree_paced(&dst).unwrap();
        println!("paced delete: {:?}", started.elapsed());
    }

    #[test]
    fn paced_removal_deletes_links_without_following_them() {
        let base = unique_tmp("paced-remove");
        let keep = base.join("keep");
        fs::create_dir(&keep).unwrap();
        fs::write(keep.join("precious"), b"p").unwrap();
        let doomed = base.join("doomed");
        fs::create_dir_all(doomed.join("a/b")).unwrap();
        fs::write(doomed.join("a/b/c"), b"c").unwrap();
        std::os::unix::fs::symlink(&keep, doomed.join("to-keep")).unwrap();

        remove_tree_paced(&doomed).unwrap();

        assert!(!doomed.exists());
        assert!(keep.join("precious").exists());
        remove_tree_paced(&base).unwrap();
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

        assert!(
            sibling.exists(),
            "parent-dir escape must not delete sibling files"
        );

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
