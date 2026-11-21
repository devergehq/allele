//! Working-tree snapshots, for "what changed since you looked" (DEV-1353).
//!
//! When you leave a session Allele records the whole working tree — tracked
//! edits, staged edits and untracked files, minus anything ignored — as a git
//! *tree object*, without touching the session's real index or refs. When you
//! come back, the current tree is recorded the same way and the two are
//! diffed. That answers "since you looked" exactly, whether the agent
//! committed in between or not, which a `git diff` against `HEAD` cannot.
//!
//! The snapshot goes through a throwaway index file (`GIT_INDEX_FILE`), seeded
//! from the real one so unchanged files are not re-hashed. `git add -A` writes
//! blob objects for changed files into the clone's object store, exactly as
//! `git stash` would; nothing references them, so a later `git gc` reclaims
//! them. A baseline old enough to have been collected makes the diff fail, and
//! the caller then shows no change summary rather than a wrong one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{git_cmd, parse_numstat_z, run_git_stdout, LineStat, LineStats};

/// One file that differs between two snapshots, with its line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeChange {
    /// Repo-relative path (the new path for a rename).
    pub path: String,
    pub stat: LineStat,
}

/// A temporary index file, removed on drop.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("index.lock"));
    }
}

fn temp_index_path() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "allele-snapshot-{}-{seq}.index",
        std::process::id()
    ))
}

/// Record the working tree of `repo` as a tree object and return its id.
///
/// Respects `.gitignore`, so build output is not counted. `None` when `repo`
/// is not a git work tree or git fails.
pub fn snapshot_worktree(repo: &Path) -> Option<String> {
    let index = TempIndex(temp_index_path());

    // Seed from the real index so `add -A` only re-hashes what changed. A
    // missing index (a repo with no commits yet) just means a cold start.
    let mut cmd = git_cmd(Some(repo));
    cmd.args(["rev-parse", "--git-path", "index"]);
    if let Ok(rel) = run_git_stdout(cmd, "rev-parse --git-path index") {
        let real = repo.join(rel);
        let _ = std::fs::copy(real, &index.0);
    }

    let mut cmd = git_cmd(Some(repo));
    cmd.env("GIT_INDEX_FILE", &index.0).args(["add", "-A"]);
    if let Err(e) = super::run_git(cmd, "add -A (snapshot)") {
        tracing::warn!("snapshot: {e}");
        return None;
    }
    let mut cmd = git_cmd(Some(repo));
    cmd.env("GIT_INDEX_FILE", &index.0).arg("write-tree");
    match run_git_stdout(cmd, "write-tree (snapshot)") {
        Ok(id) if !id.is_empty() => Some(id),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("snapshot: {e}");
            None
        }
    }
}

/// The tree `HEAD` points at, or `None` before the first commit.
pub fn head_tree(repo: &Path) -> Option<String> {
    let mut cmd = git_cmd(Some(repo));
    cmd.args(["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]);
    run_git_stdout(cmd, "rev-parse HEAD^{tree}")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Every file that differs between two trees, sorted by path. `None` when
/// either tree is unknown to the repo (e.g. collected by `git gc`).
pub fn tree_changes(repo: &Path, from: &str, to: &str) -> Option<Vec<TreeChange>> {
    let mut cmd = git_cmd(Some(repo));
    cmd.args(["diff", "--numstat", "-z", "--no-color", from, to]);
    let out = match super::run_git(cmd, "diff --numstat (snapshots)") {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("snapshot diff: {e}");
            return None;
        }
    };
    let mut stats = LineStats::new();
    parse_numstat_z(&String::from_utf8_lossy(&out.stdout), false, &mut stats);
    let mut changes: Vec<TreeChange> = stats
        .into_iter()
        .map(|((path, _), stat)| TreeChange { path, stat })
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Some(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn repo_with(content: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().to_path_buf();
        fs::write(path.join("file.txt"), content).expect("write");
        super::super::git_init(&path).expect("git_init");
        (dir, path)
    }

    #[test]
    fn an_untouched_tree_snapshots_to_head() {
        let (_d, repo) = repo_with("a\n");
        let snap = snapshot_worktree(&repo).expect("snapshot");
        assert_eq!(Some(snap), head_tree(&repo));
    }

    #[test]
    fn snapshot_leaves_the_real_index_alone() {
        let (_d, repo) = repo_with("a\n");
        fs::write(repo.join("new.txt"), "x\n").expect("write");
        snapshot_worktree(&repo).expect("snapshot");
        let status = super::super::status_changes(&repo).expect("status");
        // Still untracked: the snapshot staged it only in its own index.
        assert!(status
            .iter()
            .any(|f| f.path == "new.txt" && f.kind == super::super::ChangeKind::Untracked));
    }

    #[test]
    fn changes_between_snapshots_carry_line_counts() {
        let (_d, repo) = repo_with("a\nb\n");
        let before = snapshot_worktree(&repo).expect("before");
        fs::write(repo.join("file.txt"), "a\nc\nd\n").expect("edit");
        fs::write(repo.join("new.txt"), "1\n2\n").expect("add");
        let after = snapshot_worktree(&repo).expect("after");
        let changes = tree_changes(&repo, &before, &after).expect("diff");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "file.txt");
        assert_eq!((changes[0].stat.added, changes[0].stat.removed), (2, 1));
        assert_eq!(changes[1].path, "new.txt");
        assert_eq!((changes[1].stat.added, changes[1].stat.removed), (2, 0));
    }

    #[test]
    fn ignored_files_are_not_counted() {
        let (_d, repo) = repo_with("a\n");
        fs::write(repo.join(".gitignore"), "target/\n").expect("ignore");
        let before = snapshot_worktree(&repo).expect("before");
        fs::create_dir(repo.join("target")).expect("mkdir");
        fs::write(repo.join("target/out.bin"), "junk\n").expect("write");
        let after = snapshot_worktree(&repo).expect("after");
        assert_eq!(before, after);
    }

    #[test]
    fn an_unknown_baseline_yields_none() {
        let (_d, repo) = repo_with("a\n");
        let now = snapshot_worktree(&repo).expect("now");
        assert!(tree_changes(&repo, "0123456789abcdef0123456789abcdef01234567", &now).is_none());
    }
}
