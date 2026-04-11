//! Phase A of the clone/session merge-back playbook: a typed wrapper around
//! subprocess `git` calls.
//!
//! ## Why shell out
//!
//! Allele is macOS-only and targets developer workstations where `git`
//! is universally present (via the Xcode Command Line Tools). Every operation
//! we need is a cheap one-shot — `init`, `read-tree`, `write-tree`,
//! `commit-tree`, `update-ref`, `fetch`. Shelling out gives 100% correct
//! git semantics with zero crate dependency bloat and follows the existing
//! Allele pattern of shelling to `claude` and FFI'ing to `clonefile(2)`.
//!
//! ## Ref namespace
//!
//! - `refs/allele/base/<session-id>`  — synthetic base commit capturing
//!   canonical's on-disk state at session start. Created at session create,
//!   deleted at session discard.
//! - `refs/heads/allele/session/<session-id>` — session work branch in the
//!   clone. Lives in the clone's own `.git/` until merged back.
//! - `refs/allele/archive/<session-id>` — session work fetched back into
//!   canonical on discard. Pruned after [`TRASH_TTL_DAYS`] to match the
//!   trash bin TTL.
//!
//! ## Synthetic base commit recipe
//!
//! The temp-`GIT_INDEX_FILE` plumbing recipe captures both tracked
//! modifications AND untracked files without touching canonical's HEAD,
//! index, or working tree. See [`record_base_commit`] for the implementation.
//!
//! ## Phase A note
//!
//! Phase A of the merge-back rollout ships this module as a self-contained
//! foundation with no call sites in the rest of Allele. Integration lands
//! in Phases B–F. Until then, the functions are exercised only by the unit
//! tests — the `#![allow(dead_code)]` below reflects that on-purpose state
//! and should be removed once Phase B starts consuming the module.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
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

/// Capture canonical's current on-disk state (tracked modifications AND
/// untracked files) as a synthetic commit, anchored as `refs/allele/base/<id>`.
///
/// Returns the commit hash.
///
/// ## Plumbing recipe
///
/// Uses a temporary `GIT_INDEX_FILE` so canonical's real HEAD, index, and
/// working tree are untouched:
///
/// 1. `read-tree HEAD` into the temp index
/// 2. `add -A` into the temp index (picks up all worktree state, tracked + untracked)
/// 3. `write-tree` produces a tree object
/// 4. `commit-tree -p HEAD` produces a commit object
/// 5. `update-ref refs/allele/base/<id>` anchors the commit against GC
/// 6. Temp index file is deleted
///
/// Experimentally verified to correctly snapshot dirty state in the
/// investigation phase (PRD: `20260411-133557_clone-session-merge-back-investigation`).
pub fn record_base_commit(canonical: &Path, session_id: &str) -> anyhow::Result<String> {
    if !is_git_repo(canonical) {
        anyhow::bail!(
            "record_base_commit: not a git repo: {}",
            canonical.display()
        );
    }

    // Temp index file — keep it inside the .git/ dir of the canonical so we
    // don't pollute /tmp and so it's cleaned up with the repo if something
    // goes wrong. `.git/allele-base-<session>.idx` is a unique path.
    let tmp_index = canonical
        .join(".git")
        .join(format!("allele-base-{session_id}.idx"));

    // Ensure we clean up the temp file even on error paths.
    let result = record_base_commit_inner(canonical, session_id, &tmp_index);

    // Best-effort cleanup — ignore errors (the file may not exist if an
    // early step failed before creating it).
    let _ = std::fs::remove_file(&tmp_index);

    result
}

fn record_base_commit_inner(
    canonical: &Path,
    session_id: &str,
    tmp_index: &Path,
) -> anyhow::Result<String> {
    // Step 1: read-tree HEAD into temp index
    let mut cmd = git_cmd(Some(canonical));
    cmd.env("GIT_INDEX_FILE", tmp_index);
    cmd.arg("read-tree").arg("HEAD");
    run_git(cmd, "read-tree (base)")?;

    // Step 2: add -A into temp index
    let mut cmd = git_cmd(Some(canonical));
    cmd.env("GIT_INDEX_FILE", tmp_index);
    cmd.arg("add").arg("-A");
    run_git(cmd, "add (base)")?;

    // Step 3: write-tree
    let mut cmd = git_cmd(Some(canonical));
    cmd.env("GIT_INDEX_FILE", tmp_index);
    cmd.arg("write-tree");
    let tree = run_git_stdout(cmd, "write-tree (base)")?;

    // Step 4: commit-tree -p HEAD
    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("commit-tree")
        .arg(&tree)
        .arg("-p")
        .arg("HEAD")
        .arg("-m")
        .arg(format!("allele-base {session_id}"));
    let commit = run_git_stdout(cmd, "commit-tree (base)")?;

    // Step 5: update-ref
    let ref_name = base_ref_name(session_id);
    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("update-ref").arg(&ref_name).arg(&commit);
    run_git(cmd, "update-ref (base)")?;

    Ok(commit)
}

/// Create `refs/heads/allele/session/<id>` in the clone, rooted at
/// `base_commit`, and switch HEAD to it.
///
/// After this call, the clone's working tree is unchanged (matches
/// `base_commit`'s tree, which was captured from the exact state that
/// COW'd into the clone), HEAD points at the new session branch, and any
/// subsequent commits in the clone extend that branch.
pub fn create_session_branch(
    clone: &Path,
    base_commit: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    if !is_git_repo(clone) {
        anyhow::bail!(
            "create_session_branch: not a git repo: {}",
            clone.display()
        );
    }

    let branch = session_branch_name(session_id);

    // `git checkout -B <branch> <commit>` creates or resets the branch and
    // checks it out. Safer than `checkout -b` because it's idempotent if
    // the branch already exists (e.g. on session resume).
    let mut cmd = git_cmd(Some(clone));
    cmd.arg("checkout")
        .arg("-B")
        .arg(&branch)
        .arg(base_commit);
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

    let src = session_branch_name(session_id);
    let dst = archive_ref_name(session_id);

    let mut cmd = git_cmd(Some(canonical));
    cmd.arg("fetch")
        .arg("--no-tags")
        .arg(clone)
        .arg(format!("{src}:{dst}"));
    run_git(cmd, "fetch (session → archive)")?;

    Ok(())
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

// --- Ref name helpers ---------------------------------------------------

/// `refs/allele/base/<session-id>` — synthetic base commit in canonical.
pub fn base_ref_name(session_id: &str) -> String {
    format!("refs/allele/base/{session_id}")
}

/// `refs/heads/allele/session/<session-id>` — session branch in the clone.
pub fn session_branch_name(session_id: &str) -> String {
    format!("allele/session/{session_id}")
}

/// `refs/allele/archive/<session-id>` — archived session in canonical.
pub fn archive_ref_name(session_id: &str) -> String {
    format!("refs/allele/archive/{session_id}")
}

/// Return the short (8-char) session ID slug used for clone workspace
/// directory naming, matching `crate::clone::create_session_clone`.
pub fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// Convenience: synthesize the standard Allele clone path for a
/// `(project_name, session_id)` pair. Mirrors `crate::clone::create_session_clone`'s
/// layout so callers that want to resolve a clone path without poking
/// into clone.rs internals can use this.
pub fn default_clone_path(project_name: &str, session_id: &str) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home
        .join(".allele/workspaces")
        .join(project_name)
        .join(short_session_id(session_id)))
}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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

    /// `git status --porcelain` output.
    fn status(repo: &Path) -> String {
        let mut cmd = git_cmd(Some(repo));
        cmd.arg("status").arg("--porcelain");
        run_git_stdout(cmd, "status (test)").expect("status")
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
    fn record_base_commit_clean_canonical_tree_equals_head() {
        let (_dir, path) = make_canonical("hello");
        let head_tree = tree_of(&path, "HEAD");

        let base_commit =
            record_base_commit(&path, "testsession01").unwrap();
        let base_tree = tree_of(&path, &base_commit);

        assert_eq!(head_tree, base_tree);
    }

    #[test]
    fn record_base_commit_dirty_canonical_captures_all_state() {
        let (_dir, path) = make_canonical("base");
        // Modify tracked file
        fs::write(path.join("file.txt"), "base\nmodified").unwrap();
        // Add untracked file
        fs::write(path.join("untracked.txt"), "new stuff").unwrap();

        let base_commit =
            record_base_commit(&path, "testsession02").unwrap();
        let files = ls_tree(&path, &base_commit);

        assert!(files.contains(&"file.txt".to_string()));
        assert!(files.contains(&"untracked.txt".to_string()));
    }

    #[test]
    fn record_base_commit_leaves_head_unchanged() {
        let (_dir, path) = make_canonical("base");
        fs::write(path.join("file.txt"), "modified").unwrap();
        fs::write(path.join("untracked.txt"), "new").unwrap();

        let head_before = head_commit(&path);
        record_base_commit(&path, "testsession03").unwrap();
        let head_after = head_commit(&path);

        assert_eq!(head_before, head_after);
    }

    #[test]
    fn record_base_commit_leaves_working_tree_unchanged() {
        let (_dir, path) = make_canonical("base");
        fs::write(path.join("file.txt"), "modified").unwrap();
        fs::write(path.join("untracked.txt"), "new").unwrap();

        let status_before = status(&path);
        record_base_commit(&path, "testsession04").unwrap();
        let status_after = status(&path);

        assert_eq!(status_before, status_after);
        // Sanity: tracked mod + untracked file should both still show.
        assert!(status_after.contains("file.txt"));
        assert!(status_after.contains("untracked.txt"));
    }

    #[test]
    fn record_base_commit_creates_base_ref() {
        let (_dir, path) = make_canonical("hello");
        let commit = record_base_commit(&path, "testsession05").unwrap();
        let ref_target = resolve_ref(&path, &base_ref_name("testsession05"));
        assert_eq!(ref_target.as_deref(), Some(commit.as_str()));
    }

    #[test]
    fn record_base_commit_cleans_up_temp_index() {
        let (_dir, path) = make_canonical("hello");
        record_base_commit(&path, "testsession06").unwrap();
        let tmp = path.join(".git").join("allele-base-testsession06.idx");
        assert!(!tmp.exists(), "temp index file should be cleaned up");
    }

    #[test]
    fn create_session_branch_creates_and_checks_out_branch() {
        let (_dir, path) = make_canonical("hello");
        let base = record_base_commit(&path, "testsession07").unwrap();

        create_session_branch(&path, &base, "testsession07").unwrap();

        // The branch should exist and HEAD should point at it.
        let mut cmd = git_cmd(Some(&path));
        cmd.arg("symbolic-ref").arg("HEAD");
        let head_ref = run_git_stdout(cmd, "symbolic-ref HEAD (test)").unwrap();
        assert_eq!(head_ref, format!("refs/heads/allele/session/testsession07"));
    }

    #[test]
    fn create_session_branch_points_at_base_commit() {
        let (_dir, path) = make_canonical("hello");
        let base = record_base_commit(&path, "testsession08").unwrap();

        create_session_branch(&path, &base, "testsession08").unwrap();
        let head = head_commit(&path);
        assert_eq!(head, base);
    }

    #[test]
    fn fetch_session_branch_round_trip() {
        // Build a canonical and a "clone" (really just a second repo
        // sharing history — good enough for Phase A tests; the real
        // integration with clonefile() happens in Phase C).
        let (_cdir, canonical) = make_canonical("base");
        let base = record_base_commit(&canonical, "roundtrip01").unwrap();

        // Second repo, seeded to look like a clone: init, fetch canonical's
        // HEAD into it, check out a session branch, commit new work.
        let clone_dir = TempDir::new().unwrap();
        let clone_path = clone_dir.path().to_path_buf();
        git_init(&clone_path).unwrap();

        // Fetch canonical's base commit into the clone.
        let mut cmd = git_cmd(Some(&clone_path));
        cmd.arg("fetch")
            .arg(&canonical)
            .arg(format!("{base}:refs/allele/base/roundtrip01"));
        run_git(cmd, "fetch base into clone").unwrap();

        // Create session branch in the clone from the fetched base.
        create_session_branch(&clone_path, &base, "roundtrip01").unwrap();

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

        // Now fetch the session branch back into canonical.
        fetch_session_branch(&canonical, &clone_path, "roundtrip01").unwrap();

        // Canonical should now have refs/allele/archive/roundtrip01 pointing
        // at the session work commit.
        let archive_target =
            resolve_ref(&canonical, &archive_ref_name("roundtrip01"));
        assert_eq!(archive_target.as_deref(), Some(session_head.as_str()));

        // And the session-work.txt file should be reachable from canonical's
        // object database.
        let mut cmd = git_cmd(Some(&canonical));
        cmd.arg("cat-file")
            .arg("-e")
            .arg(format!("{session_head}:session-work.txt"));
        run_git(cmd, "cat-file session-work (canonical)").unwrap();
    }

    #[test]
    fn delete_ref_removes_target() {
        let (_dir, path) = make_canonical("hello");
        record_base_commit(&path, "del01").unwrap();
        assert!(resolve_ref(&path, &base_ref_name("del01")).is_some());

        delete_ref(&path, &base_ref_name("del01")).unwrap();
        assert!(resolve_ref(&path, &base_ref_name("del01")).is_none());
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
}
