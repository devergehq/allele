//! Typed wrapper around subprocess `git` calls for the clone/session
//! merge-back pipeline.
//!
//! ## Why shell out
//!
//! Allele is macOS-only and targets developer workstations where `git`
//! is universally present (via the Xcode Command Line Tools). Every operation
//! we need is a cheap one-shot — `init`, `fetch`, `merge`, `status`.
//! Shelling out gives 100% correct git semantics with zero crate dependency
//! bloat and follows the existing Allele pattern of shelling to `claude`
//! and FFI'ing to `clonefile(2)`.
//!
//! ## Ref namespace
//!
//! - `refs/heads/allele/session/<session-id>` — session work branch in the
//!   clone, rooted at canonical's HEAD at clone time. Lives in the clone's
//!   own `.git/` until archived back.
//! - `refs/allele/archive/<session-id>` — session work fetched back into
//!   canonical on discard. Pruned after [`TRASH_TTL_DAYS`] to match the
//!   trash bin TTL.
//!

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Build a `git` command with the standard Allele environment — inline
/// identity flags so commits work even when the user has no global git
/// identity configured, `GIT_CONFIG_NOSYSTEM=1` so `/etc/gitconfig` is
/// ignored (system-wide hooks can't interfere with Allele's internal
/// operations), and an optional `-C <repo>` to run as if the CWD were
/// the given path.
fn git_cmd(repo: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(path) = repo {
        cmd.arg("-C").arg(path);
    }
    cmd.arg("-c").arg("user.email=allele@local");
    cmd.arg("-c").arg("user.name=Allele");
    cmd
}

/// Execute a `git` subprocess and return its `Output`, converting non-zero
/// exit into an error that includes stderr.
fn run_git(mut cmd: Command, context: &str) -> anyhow::Result<std::process::Output> {
    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("failed to spawn git ({context}): {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {context} failed: {}", stderr.trim());
    }
    Ok(output)
}

/// Execute a `git` subprocess and return its trimmed stdout as a `String`.
fn run_git_stdout(cmd: Command, context: &str) -> anyhow::Result<String> {
    let output = run_git(cmd, context)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Return true if `git` is on PATH and responds to `git --version`.
///
/// This is the startup check — if it returns false, Allele should refuse
/// to start with a friendly error pointing the user at `xcode-select --install`.
/// The result is cheap (~5ms) but callers should cache it rather than
/// re-running it per operation.
pub fn git_available() -> bool {
    let mut cmd = Command::new("git");
    cmd.arg("--version");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Return true if `path` is inside a git work tree.
pub fn is_git_repo(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let mut cmd = git_cmd(Some(path));
    cmd.arg("rev-parse").arg("--is-inside-work-tree");
    matches!(
        cmd.output().ok().map(|o| (o.status.success(), o.stdout)),
        Some((true, stdout)) if String::from_utf8_lossy(&stdout).trim() == "true"
    )
}

/// Return true if the working tree has uncommitted changes (staged,
/// unstaged, or untracked files). Uses `git status --porcelain` — any
/// non-empty output means dirty.
pub fn is_working_tree_dirty(path: &Path) -> bool {
    if !is_git_repo(path) {
        return false;
    }
    let mut cmd = git_cmd(Some(path));
    cmd.arg("status").arg("--porcelain");
    cmd.output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Initialise `path` as a git repository and create an initial commit
/// capturing the current on-disk state.
///
/// Idempotent — if `path` is already a git repo, returns `Ok(())` without
/// modifying anything. Safe to call unconditionally on any project added
/// to Allele.
///
/// The initial commit uses Allele's inline identity, so no global git
/// config is required. `--no-verify` bypasses any pre-commit hooks the
/// user might have configured via `core.hooksPath`.
pub fn git_init(path: &Path) -> anyhow::Result<()> {
    if is_git_repo(path) {
        return Ok(());
    }

    if !path.exists() {
        anyhow::bail!(
            "git_init: path does not exist: {}",
            path.display()
        );
    }

    // git init
    let mut cmd = git_cmd(Some(path));
    cmd.arg("init").arg("--quiet");
    run_git(cmd, "init")?;

    // git add -A
    let mut cmd = git_cmd(Some(path));
    cmd.arg("add").arg("-A");
    run_git(cmd, "add")?;

    // Check if there's anything to commit — empty dirs need --allow-empty
    let mut cmd = git_cmd(Some(path));
    cmd.arg("diff").arg("--cached").arg("--quiet");
    let has_staged = !cmd.output().map(|o| o.status.success()).unwrap_or(true);

    // git commit
    let mut cmd = git_cmd(Some(path));
    cmd.arg("commit")
        .arg("--no-verify")
        .arg("--quiet")
        .arg("-m")
        .arg("allele: initial state");
    if !has_staged {
        cmd.arg("--allow-empty");
    }
    run_git(cmd, "commit")?;

    Ok(())
}

/// Create `refs/heads/allele/session/<id>` in the clone, rooted at HEAD,
/// and switch to it.
///
/// After this call, HEAD points at the new session branch and any
/// subsequent commits in the clone extend that branch.
pub fn create_session_branch(
    clone: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    if !is_git_repo(clone) {
        anyhow::bail!(
            "create_session_branch: not a git repo: {}",
            clone.display()
        );
    }

    let branch = session_branch_name(session_id);

    // `git checkout -B <branch> HEAD` creates or resets the branch and
    // checks it out. Safer than `checkout -b` because it's idempotent if
    // the branch already exists (e.g. on session resume).
    let mut cmd = git_cmd(Some(clone));
    cmd.arg("checkout")
        .arg("-B")
        .arg(&branch)
        .arg("HEAD");
    run_git(cmd, "checkout -B (session)")?;

    Ok(())
}

/// Fetch the session branch from a clone back into canonical as an archive
/// ref: `refs/allele/archive/<session-id>`.
///
/// Non-destructive on the clone side — fetch is read-only over the clone's
/// object database. Uses a local file-path remote, so no network is
/// involved and no special `protocol.file.allow` flag is required (the
/// CVE-2022-39253 restriction applies to submodule file:// clones, not
/// peer path fetches — verified experimentally on git 2.49.0).
pub fn fetch_session_branch(
    canonical: &Path,
    clone: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    if !is_git_repo(canonical) {
        anyhow::bail!(
            "fetch_session_branch: canonical is not a git repo: {}",
            canonical.display()
        );
    }
    if !is_git_repo(clone) {
        anyhow::bail!(
            "fetch_session_branch: clone is not a git repo: {}",
            clone.display()
        );
    }

    // Use the clone's actual current branch — after auto-naming it may be
    // `allele/session/<uuid>/<slug>` rather than the original `allele/session/<uuid>`.
    let src = current_branch(clone)
        .unwrap_or_else(|| session_branch_name(session_id));
    let dst = archive_ref_name(session_id);

    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("fetch")
        .arg("--no-tags")
        .arg(clone)
        .arg(format!("{src}:{dst}"));
    run_git(cmd, "fetch (session → archive)")?;

    Ok(())
}

/// If the clone's working tree has uncommitted changes, stage everything
/// and create a commit so the work is captured on the session branch
/// before archiving. Returns `true` if a commit was created.
pub fn auto_commit_if_dirty(clone: &Path) -> anyhow::Result<bool> {
    if !is_working_tree_dirty(clone) {
        return Ok(false);
    }
    let mut add = git_cmd(Some(clone));
    add.arg("add").arg("-A");
    run_git(add, "add -A (auto-commit)")?;

    let mut commit = git_cmd(Some(clone));
    commit
        .arg("commit")
        .arg("--no-verify")
        .arg("-m")
        .arg("allele: auto-commit uncommitted work before archive");
    run_git(commit, "commit (auto-commit)")?;
    Ok(true)
}

/// Archive a clone's session work back into canonical by fetching the
/// session branch as `refs/allele/archive/<session-id>`.
///
/// Automatically commits any uncommitted changes in the clone first so
/// they are not lost when the clone is deleted.
///
/// Returns the fetch result — callers typically log it and proceed to
/// delete/trash the clone regardless of outcome.
pub fn archive_session(
    canonical: &Path,
    clone: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    // Capture any uncommitted work before fetching the branch.
    if let Err(e) = auto_commit_if_dirty(clone) {
        eprintln!("auto_commit_if_dirty failed for {session_id}: {e}");
    }
    fetch_session_branch(canonical, clone, session_id)
}

/// Delete a ref. Equivalent to `git update-ref -d <ref>`.
pub fn delete_ref(repo: &Path, ref_name: &str) -> anyhow::Result<()> {
    if !is_git_repo(repo) {
        anyhow::bail!("delete_ref: not a git repo: {}", repo.display());
    }
    let mut cmd = git_cmd(Some(repo));
    cmd.arg("update-ref").arg("-d").arg(ref_name);
    run_git(cmd, "update-ref -d")?;
    Ok(())
}

/// Prune `refs/allele/archive/*` refs whose committer date is older than
/// `ttl_days`. Returns the number of refs pruned.
///
/// Matches the trash bin TTL so archived session work is preserved for
/// the same window as the trash. Intended to be called once at startup
/// alongside [`crate::clone::purge_trash_older_than_days`].
pub fn prune_archive_refs(canonical: &Path, ttl_days: u64) -> anyhow::Result<usize> {
    if !is_git_repo(canonical) {
        anyhow::bail!(
            "prune_archive_refs: not a git repo: {}",
            canonical.display()
        );
    }

    // List archive refs with committer dates as unix timestamps.
    // Tab separator to avoid space-parsing issues in ref names.
    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("for-each-ref")
        .arg("--format=%(refname)%09%(committerdate:unix)")
        .arg("refs/allele/archive/");
    let listing = run_git_stdout(cmd, "for-each-ref (prune)")?;

    if listing.is_empty() {
        return Ok(0);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ttl_secs = ttl_days * 24 * 60 * 60;

    let mut pruned = 0usize;
    for line in listing.lines() {
        let Some((ref_name, ts_str)) = line.split_once('\t') else {
            continue;
        };
        let Ok(ts) = ts_str.trim().parse::<u64>() else {
            continue;
        };

        if now.saturating_sub(ts) < ttl_secs {
            continue;
        }

        // Expired — delete the ref.
        if let Err(e) = delete_ref(canonical, ref_name) {
            eprintln!("prune_archive_refs: failed to delete {ref_name}: {e}");
            continue;
        }
        pruned += 1;
    }

    Ok(pruned)
}

// --- Archive browsing + merging ------------------------------------------

/// An archived session ref in canonical, ready to browse or merge.
pub struct ArchiveEntry {
    pub session_id: String,
    #[allow(dead_code)] // stored for future display (tooltips, detail view)
    pub commit_hash: String,
    pub timestamp: u64, // unix epoch seconds
}

/// List all `refs/allele/archive/*` refs in `canonical`, sorted by
/// timestamp (most recent first). Returns an empty vec if canonical is
/// not a git repo or has no archive refs.
pub fn list_archive_refs(canonical: &Path) -> anyhow::Result<Vec<ArchiveEntry>> {
    if !is_git_repo(canonical) {
        return Ok(Vec::new());
    }
    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("for-each-ref")
        .arg("--format=%(refname)%09%(objectname:short)%09%(committerdate:unix)")
        .arg("refs/allele/archive/");
    let listing = run_git_stdout(cmd, "for-each-ref (list archives)")?;
    if listing.is_empty() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for line in listing.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let session_id = parts[0]
            .strip_prefix("refs/allele/archive/")
            .unwrap_or(parts[0])
            .to_string();
        let commit_hash = parts[1].to_string();
        let timestamp = parts[2].trim().parse::<u64>().unwrap_or(0);
        entries.push(ArchiveEntry {
            session_id,
            commit_hash,
            timestamp,
        });
    }
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(entries)
}

/// Result of a merge attempt — distinguishes actual merges from no-ops.
#[derive(Debug, PartialEq)]
pub enum MergeResult {
    /// New merge commit created — work was integrated.
    Merged,
    /// Archive ref was already an ancestor of HEAD — nothing to merge.
    AlreadyUpToDate,
}

/// Merge an archived session ref into canonical's current branch.
/// Uses `--no-ff --no-edit` to preserve the merge as a distinct commit.
/// Returns `MergeResult::AlreadyUpToDate` if the archive ref is already
/// an ancestor of HEAD (i.e. no new work to merge).
/// Returns an error if there are merge conflicts or the working tree is
/// dirty — the caller should display the error and let the user resolve
/// conflicts manually.
pub fn merge_archive(canonical: &Path, session_id: &str) -> anyhow::Result<MergeResult> {
    if !is_git_repo(canonical) {
        anyhow::bail!("merge_archive: not a git repo: {}", canonical.display());
    }

    // Record HEAD before merge to detect no-ops.
    let head_before = {
        let mut cmd = git_cmd(Some(canonical));
        cmd.arg("rev-parse").arg("HEAD");
        run_git_stdout(cmd, "rev-parse HEAD (pre-merge)")?
    };

    let ref_name = archive_ref_name(session_id);
    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("merge")
        .arg("--no-ff")
        .arg("--no-edit")
        .arg(&ref_name);
    run_git(cmd, "merge archive")?;

    // Check if HEAD actually moved.
    let head_after = {
        let mut cmd = git_cmd(Some(canonical));
        cmd.arg("rev-parse").arg("HEAD");
        run_git_stdout(cmd, "rev-parse HEAD (post-merge)")?
    };

    if head_before == head_after {
        Ok(MergeResult::AlreadyUpToDate)
    } else {
        Ok(MergeResult::Merged)
    }
}

// --- Branch introspection -----------------------------------------------

/// Read the current branch name (short form, e.g. `main` or
/// `allele/session/abc12345`). Returns `None` if the repo isn't a git
/// repo or HEAD is detached.
pub fn current_branch(repo: &Path) -> Option<String> {
    if !is_git_repo(repo) {
        return None;
    }
    let mut cmd = git_cmd(Some(repo));
    cmd.arg("symbolic-ref").arg("--short").arg("HEAD");
    cmd.output().ok().and_then(|o| {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        } else {
            None
        }
    })
}

/// Extract the session ID from a branch name like `allele/session/<id>`
/// or `allele/session/<id>/<slug>` (after auto-naming).
/// Returns `None` if the branch doesn't follow the Allele session naming
/// convention.
pub fn session_id_from_branch(branch: &str) -> Option<&str> {
    let rest = branch.strip_prefix("allele/session/")?;
    // After rename, rest might be "<uuid>/<slug>" — take only the UUID part
    Some(rest.split('/').next().unwrap_or(rest))
}

// --- Ref name helpers ---------------------------------------------------

/// `refs/heads/allele/session/<session-id>` — session branch in the clone.
pub fn session_branch_name(session_id: &str) -> String {
    format!("allele/session/{session_id}")
}

/// `refs/allele/archive/<session-id>` — archived session in canonical.
pub fn archive_ref_name(session_id: &str) -> String {
    format!("refs/allele/archive/{session_id}")
}


// --- Session auto-naming ------------------------------------------------

/// Sanitise a string for use as a git branch name segment.
/// Lowercase, hyphens only, max length, no leading/trailing hyphens.
pub fn slugify(input: &str, max_len: usize) -> String {
    let slug: String = input
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let truncated = if slug.len() > max_len {
        &slug[..max_len]
    } else {
        &slug
    };
    truncated.trim_end_matches('-').to_string()
}

/// Rename a session branch from `allele/session/<id>` to
/// `allele/session/<id>/<slug>`. Idempotent — returns `Ok(())` if the
/// old branch doesn't exist (already renamed or detached HEAD).
pub fn rename_session_branch(
    clone: &Path,
    session_id: &str,
    slug: &str,
) -> anyhow::Result<()> {
    if !is_git_repo(clone) {
        anyhow::bail!(
            "rename_session_branch: not a git repo: {}",
            clone.display()
        );
    }

    let old_branch = session_branch_name(session_id);
    let new_branch = format!("allele/session/{session_id}/{slug}");

    // Check current branch — if already renamed, skip.
    if let Some(current) = current_branch(clone) {
        if current == new_branch || current != old_branch {
            return Ok(());
        }
    }

    let mut cmd = git_cmd(Some(clone));
    cmd.arg("branch").arg("-m").arg(&old_branch).arg(&new_branch);
    run_git(cmd, "branch -m (session rename)")?;

    Ok(())
}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Create a fresh canonical repo in a temp directory with a single
    /// committed file, and return the TempDir (keeps the repo alive) and
    /// the path.
    fn make_canonical(content: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().to_path_buf();
        fs::write(path.join("file.txt"), content).expect("write");
        git_init(&path).expect("git_init");
        (dir, path)
    }

    /// Get the current HEAD commit hash of a repo.
    fn head_commit(repo: &Path) -> String {
        let mut cmd = git_cmd(Some(repo));
        cmd.arg("rev-parse").arg("HEAD");
        run_git_stdout(cmd, "rev-parse HEAD (test)").expect("head")
    }

    /// Get the tree hash of a commit.
    fn tree_of(repo: &Path, commit: &str) -> String {
        let mut cmd = git_cmd(Some(repo));
        cmd.arg("rev-parse").arg(format!("{commit}^{{tree}}"));
        run_git_stdout(cmd, "rev-parse tree (test)").expect("tree")
    }

    /// Read a ref's target hash, or None if the ref doesn't exist.
    fn resolve_ref(repo: &Path, ref_name: &str) -> Option<String> {
        let mut cmd = git_cmd(Some(repo));
        cmd.arg("rev-parse").arg("--verify").arg(ref_name);
        cmd.output().ok().and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
    }

    /// List the files tracked in a commit's tree.
    fn ls_tree(repo: &Path, tree_ish: &str) -> Vec<String> {
        let mut cmd = git_cmd(Some(repo));
        cmd.arg("ls-tree").arg("-r").arg("--name-only").arg(tree_ish);
        let out = run_git_stdout(cmd, "ls-tree (test)").expect("ls-tree");
        out.lines().map(|s| s.to_string()).collect()
    }

    #[test]
    fn git_available_on_dev_machine() {
        // Dev machines running Allele's test suite always have git.
        assert!(git_available());
    }

    #[test]
    fn is_git_repo_rejects_empty_dir() {
        let dir = TempDir::new().unwrap();
        assert!(!is_git_repo(dir.path()));
    }

    #[test]
    fn is_git_repo_accepts_initialised_dir() {
        let (_dir, path) = make_canonical("hello");
        assert!(is_git_repo(&path));
    }

    #[test]
    fn git_init_creates_dot_git() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.txt"), "x").unwrap();
        git_init(dir.path()).unwrap();
        assert!(dir.path().join(".git").is_dir());
    }

    #[test]
    fn git_init_creates_exactly_one_commit() {
        let (_dir, path) = make_canonical("hello");
        let mut cmd = git_cmd(Some(&path));
        cmd.arg("rev-list").arg("--count").arg("HEAD");
        let count = run_git_stdout(cmd, "rev-list").unwrap();
        assert_eq!(count, "1");
    }

    #[test]
    fn git_init_is_idempotent() {
        let (_dir, path) = make_canonical("hello");
        let head_before = head_commit(&path);
        // Second init should not create another commit.
        git_init(&path).unwrap();
        let head_after = head_commit(&path);
        assert_eq!(head_before, head_after);
    }

    #[test]
    fn git_init_works_on_empty_dir() {
        let dir = TempDir::new().unwrap();
        // Completely empty — no files at all.
        git_init(dir.path()).unwrap();
        assert!(is_git_repo(dir.path()));
    }

    #[test]
    fn is_working_tree_dirty_clean_repo() {
        let (_dir, path) = make_canonical("hello");
        assert!(!is_working_tree_dirty(&path));
    }

    #[test]
    fn is_working_tree_dirty_with_modifications() {
        let (_dir, path) = make_canonical("hello");
        fs::write(path.join("file.txt"), "modified").unwrap();
        assert!(is_working_tree_dirty(&path));
    }

    #[test]
    fn is_working_tree_dirty_with_untracked() {
        let (_dir, path) = make_canonical("hello");
        fs::write(path.join("new.txt"), "untracked").unwrap();
        assert!(is_working_tree_dirty(&path));
    }

    #[test]
    fn create_session_branch_creates_and_checks_out_branch() {
        let (_dir, path) = make_canonical("hello");
        create_session_branch(&path, "testsession07").unwrap();

        let mut cmd = git_cmd(Some(&path));
        cmd.arg("symbolic-ref").arg("HEAD");
        let head_ref = run_git_stdout(cmd, "symbolic-ref HEAD (test)").unwrap();
        assert_eq!(head_ref, format!("refs/heads/allele/session/testsession07"));
    }

    #[test]
    fn create_session_branch_points_at_head() {
        let (_dir, path) = make_canonical("hello");
        let head_before = head_commit(&path);
        create_session_branch(&path, "testsession08").unwrap();
        let head_after = head_commit(&path);
        assert_eq!(head_before, head_after);
    }

    #[test]
    fn fetch_session_branch_round_trip() {
        let (_cdir, canonical) = make_canonical("base");
        let canonical_head = head_commit(&canonical);

        // Second repo seeded via git clone --local to share history.
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        // Create session branch in the clone rooted at HEAD.
        create_session_branch(&clone_path, "roundtrip01").unwrap();

        // Do some "session work" in the clone.
        fs::write(clone_path.join("session-work.txt"), "work").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("add").arg("-A");
        run_git(cmd, "add session work").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("commit")
            .arg("--no-verify")
            .arg("-m")
            .arg("session work");
        run_git(cmd, "commit session work").unwrap();
        let session_head = head_commit(&clone_path);

        // Session branch parent should be canonical's HEAD (no base commit).
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("rev-parse").arg(format!("{session_head}^"));
        let parent = run_git_stdout(cmd, "rev-parse parent (test)").unwrap();
        assert_eq!(parent, canonical_head);

        // Fetch the session branch back into canonical.
        fetch_session_branch(&canonical, &clone_path, "roundtrip01").unwrap();

        let archive_target =
            resolve_ref(&canonical, &archive_ref_name("roundtrip01"));
        assert_eq!(archive_target.as_deref(), Some(session_head.as_str()));

        // Session work file reachable from canonical's object database.
        let mut cmd = git_cmd(Some(&canonical));
        cmd.arg("cat-file")
            .arg("-e")
            .arg(format!("{session_head}:session-work.txt"));
        run_git(cmd, "cat-file session-work (canonical)").unwrap();
    }

    #[test]
    fn delete_ref_removes_target() {
        let (_dir, path) = make_canonical("hello");
        let head = head_commit(&path);
        // Create a ref to test deletion against
        let mut cmd = git_cmd(Some(&path));
        cmd.arg("update-ref").arg("refs/test/del01").arg(&head);
        run_git(cmd, "update-ref (test)").unwrap();
        assert!(resolve_ref(&path, "refs/test/del01").is_some());

        delete_ref(&path, "refs/test/del01").unwrap();
        assert!(resolve_ref(&path, "refs/test/del01").is_none());
    }

    #[test]
    fn prune_archive_refs_keeps_recent_entries() {
        let (_dir, path) = make_canonical("hello");
        // Create an archive ref pointing at HEAD, with a "now" timestamp.
        let head = head_commit(&path);
        let mut cmd = git_cmd(Some(&path));
        cmd.arg("update-ref")
            .arg(archive_ref_name("recent01"))
            .arg(&head);
        run_git(cmd, "update-ref archive (test)").unwrap();

        // Pruning with 14-day TTL should keep this one (committer date is now).
        let pruned = prune_archive_refs(&path, 14).unwrap();
        assert_eq!(pruned, 0);
        assert!(resolve_ref(&path, &archive_ref_name("recent01")).is_some());
    }

    #[test]
    fn prune_archive_refs_deletes_expired_entries() {
        let (_dir, path) = make_canonical("hello");

        // Create a commit with a backdated committer date (30 days ago).
        let past = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - (30 * 24 * 60 * 60);
        let head_tree = tree_of(&path, "HEAD");

        let mut cmd = git_cmd(Some(&path));
        cmd.env("GIT_COMMITTER_DATE", format!("{past} +0000"));
        cmd.env("GIT_AUTHOR_DATE", format!("{past} +0000"));
        cmd.arg("commit-tree")
            .arg(&head_tree)
            .arg("-m")
            .arg("backdated");
        let backdated_commit =
            run_git_stdout(cmd, "commit-tree backdated (test)").unwrap();

        let mut cmd = git_cmd(Some(&path));
        cmd.arg("update-ref")
            .arg(archive_ref_name("old01"))
            .arg(&backdated_commit);
        run_git(cmd, "update-ref old archive (test)").unwrap();

        // Also create a fresh one that should survive pruning.
        let head = head_commit(&path);
        let mut cmd = git_cmd(Some(&path));
        cmd.arg("update-ref")
            .arg(archive_ref_name("fresh01"))
            .arg(&head);
        run_git(cmd, "update-ref fresh archive (test)").unwrap();

        let pruned = prune_archive_refs(&path, 14).unwrap();
        assert_eq!(pruned, 1);
        assert!(resolve_ref(&path, &archive_ref_name("old01")).is_none());
        assert!(resolve_ref(&path, &archive_ref_name("fresh01")).is_some());
    }

    // --- Phase G: tests for B–F additions --------------------------------

    #[test]
    fn current_branch_returns_branch_name() {
        let (_dir, path) = make_canonical("hello");
        // make_canonical leaves HEAD on the default branch (usually "master")
        let branch = current_branch(&path);
        assert!(branch.is_some(), "expected a branch name");
        assert!(!branch.unwrap().is_empty());
    }

    #[test]
    fn current_branch_returns_none_for_detached_head() {
        let (_dir, path) = make_canonical("hello");
        let head = head_commit(&path);
        // Detach HEAD by checking out the commit directly
        let mut cmd = git_cmd(Some(&path));
        cmd.arg("checkout").arg("--detach").arg(&head);
        run_git(cmd, "detach HEAD (test)").unwrap();
        assert!(current_branch(&path).is_none());
    }

    #[test]
    fn current_branch_returns_none_for_non_git_dir() {
        let dir = TempDir::new().unwrap();
        assert!(current_branch(dir.path()).is_none());
    }

    #[test]
    fn session_id_from_branch_extracts_id() {
        assert_eq!(
            session_id_from_branch("allele/session/abc12345"),
            Some("abc12345")
        );
    }

    #[test]
    fn session_id_from_branch_rejects_other_branches() {
        assert_eq!(session_id_from_branch("main"), None);
        assert_eq!(session_id_from_branch("allele/archive/abc"), None);
        assert_eq!(session_id_from_branch(""), None);
    }

    #[test]
    fn archive_session_creates_archive_ref() {
        let (_cdir, canonical) = make_canonical("base");

        // Clone via git clone --local to share history
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        create_session_branch(&clone_path, "archtest01").unwrap();

        // Do some work in the clone
        fs::write(clone_path.join("work.txt"), "session work").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("add").arg("-A");
        run_git(cmd, "add (test)").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("commit").arg("--no-verify").arg("-m").arg("work");
        run_git(cmd, "commit (test)").unwrap();
        let work_commit = head_commit(&clone_path);

        archive_session(&canonical, &clone_path, "archtest01").unwrap();

        // Archive ref should exist and point at the session work
        let archive_target = resolve_ref(&canonical, &archive_ref_name("archtest01"));
        assert_eq!(archive_target.as_deref(), Some(work_commit.as_str()));
    }

    #[test]
    fn full_round_trip_init_branch_archive_merge() {
        // End-to-end: init → branch → commit → archive → merge → verify
        // clean history (no synthetic base commit).

        // 1. Canonical with a file
        let (_cdir, canonical) = make_canonical("original content");
        let canonical_head = head_commit(&canonical);

        // 2. Clone (simulates COW clonefile)
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        // 3. Create session branch rooted at HEAD
        create_session_branch(&clone_path, "e2e01").unwrap();

        // 4. Do session work
        fs::write(clone_path.join("session-notes.txt"), "important work").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("add").arg("-A");
        run_git(cmd, "add session work (test)").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("commit").arg("--no-verify").arg("-m").arg("session work");
        run_git(cmd, "commit session work (test)").unwrap();
        let session_head = head_commit(&clone_path);

        // 5. Archive — fetch session branch into canonical
        archive_session(&canonical, &clone_path, "e2e01").unwrap();

        // 6. Verify: archive ref exists
        let archive = resolve_ref(&canonical, &archive_ref_name("e2e01"));
        assert_eq!(archive.as_deref(), Some(session_head.as_str()));

        // 7. Merge the archive into canonical
        let result = merge_archive(&canonical, "e2e01").unwrap();
        assert_eq!(result, MergeResult::Merged);

        // 8. Verify: session work file is in canonical's HEAD
        let files = ls_tree(&canonical, "HEAD");
        assert!(files.contains(&"session-notes.txt".to_string()));

        // 9. Verify: the session work commit's parent is the original
        // canonical HEAD — no synthetic base commit in between.
        let mut cmd = git_cmd(Some(&canonical));
        cmd.arg("rev-parse").arg(format!("{session_head}^"));
        let parent = run_git_stdout(cmd, "rev-parse parent (test)").unwrap();
        assert_eq!(parent, canonical_head);

        // 10. Verify: current_branch + session_id_from_branch work on the clone
        let branch = current_branch(&clone_path).unwrap();
        assert_eq!(session_id_from_branch(&branch), Some("e2e01"));
    }

    // --- Auto-naming tests -------------------------------------------------

    #[test]
    fn session_id_from_branch_with_slug() {
        assert_eq!(
            session_id_from_branch("allele/session/855fa03e/fix-login-bug"),
            Some("855fa03e")
        );
    }

    #[test]
    fn session_id_from_branch_uuid_only() {
        // Original format still works
        assert_eq!(
            session_id_from_branch("allele/session/855fa03e"),
            Some("855fa03e")
        );
    }

    #[test]
    fn session_id_from_branch_full_uuid_with_slug() {
        assert_eq!(
            session_id_from_branch("allele/session/855fa03e-5cc7-477a-b1e6-4e9d127923b6/refactor-auth"),
            Some("855fa03e-5cc7-477a-b1e6-4e9d127923b6")
        );
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Fix the login bug", 50), "fix-the-login-bug");
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(slugify("Can you help me?!", 50), "can-you-help-me");
    }

    #[test]
    fn slugify_truncates() {
        assert_eq!(slugify("this is a very long prompt that should be truncated", 20), "this-is-a-very-long");
    }

    #[test]
    fn slugify_no_trailing_hyphens() {
        assert_eq!(slugify("hello world ---", 50), "hello-world");
    }

    #[test]
    fn slugify_collapses_multiple_hyphens() {
        assert_eq!(slugify("fix   the   bug", 50), "fix-the-bug");
    }

    #[test]
    fn rename_session_branch_works() {
        let (_dir, path) = make_canonical("hello");
        create_session_branch(&path, "rename01").unwrap();

        rename_session_branch(&path, "rename01", "fix-login-bug").unwrap();

        let branch = current_branch(&path).unwrap();
        assert_eq!(branch, "allele/session/rename01/fix-login-bug");
        // session_id extraction still works
        assert_eq!(session_id_from_branch(&branch), Some("rename01"));
    }

    #[test]
    fn rename_session_branch_is_idempotent() {
        let (_dir, path) = make_canonical("hello");
        create_session_branch(&path, "rename02").unwrap();

        rename_session_branch(&path, "rename02", "fix-bug").unwrap();
        // Second rename should be a no-op
        rename_session_branch(&path, "rename02", "fix-bug").unwrap();

        let branch = current_branch(&path).unwrap();
        assert_eq!(branch, "allele/session/rename02/fix-bug");
    }

    #[test]
    fn archive_after_rename_works() {
        let (_cdir, canonical) = make_canonical("base");

        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        create_session_branch(&clone_path, "archrename01").unwrap();

        // Do work
        fs::write(clone_path.join("work.txt"), "session work").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("add").arg("-A");
        run_git(cmd, "add (test)").unwrap();
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("commit").arg("--no-verify").arg("-m").arg("work");
        run_git(cmd, "commit (test)").unwrap();
        let work_commit = head_commit(&clone_path);

        // Rename the branch (simulating auto-naming)
        rename_session_branch(&clone_path, "archrename01", "fix-auth").unwrap();

        // Archive should still work — uses current_branch to find the ref
        archive_session(&canonical, &clone_path, "archrename01").unwrap();

        // Archive ref should exist and point at the session work
        let archive_target = resolve_ref(&canonical, &archive_ref_name("archrename01"));
        assert_eq!(archive_target.as_deref(), Some(work_commit.as_str()));
    }

    #[test]
    fn merge_archive_detects_noop_when_no_new_commits() {
        // Session branch with no new commits → merge is "Already up to date"
        let (_cdir, canonical) = make_canonical("base");
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        create_session_branch(&clone_path, "noop01").unwrap();
        // No commits — session branch is identical to master

        archive_session(&canonical, &clone_path, "noop01").unwrap();
        let result = merge_archive(&canonical, "noop01").unwrap();
        assert_eq!(result, MergeResult::AlreadyUpToDate);
    }

    #[test]
    fn auto_commit_if_dirty_captures_uncommitted_work() {
        let (_cdir, canonical) = make_canonical("base");
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        create_session_branch(&clone_path, "dirty01").unwrap();
        let head_before = head_commit(&clone_path);

        // Simulate uncommitted work
        fs::write(clone_path.join("unsaved.txt"), "important work").unwrap();
        assert!(is_working_tree_dirty(&clone_path));

        let committed = auto_commit_if_dirty(&clone_path).unwrap();
        assert!(committed);
        assert!(!is_working_tree_dirty(&clone_path));

        let head_after = head_commit(&clone_path);
        assert_ne!(head_before, head_after);

        // Archive and merge should now find actual work
        archive_session(&canonical, &clone_path, "dirty01").unwrap();
        let result = merge_archive(&canonical, "dirty01").unwrap();
        assert_eq!(result, MergeResult::Merged);

        let files = ls_tree(&canonical, "HEAD");
        assert!(files.contains(&"unsaved.txt".to_string()));
    }

    #[test]
    fn archive_session_auto_commits_dirty_clone() {
        // End-to-end: dirty clone → archive_session auto-commits → merge finds work
        let (_cdir, canonical) = make_canonical("base");
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        let mut cmd = git_cmd(None);
        cmd.arg("clone").arg("--local").arg(&canonical).arg(&clone_path);
        run_git(cmd, "git clone --local (test)").unwrap();

        create_session_branch(&clone_path, "autocommit01").unwrap();

        // Only uncommitted changes — no manual commit
        fs::write(clone_path.join("work.txt"), "session edits").unwrap();

        // archive_session should auto-commit before fetching
        archive_session(&canonical, &clone_path, "autocommit01").unwrap();

        let result = merge_archive(&canonical, "autocommit01").unwrap();
        assert_eq!(result, MergeResult::Merged);

        let files = ls_tree(&canonical, "HEAD");
        assert!(files.contains(&"work.txt".to_string()));
    }
}
