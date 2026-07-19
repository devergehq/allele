//! Project identity for the sync gate (DEV-191).
//!
//! A session only pulls onto a machine whose Allele already knows the session's
//! project. Identity is the opened-folder name plus — the sturdier key — the
//! canonical git remote URL. The resolver matches an incoming bundle's
//! [`ProjectIdentity`] to a *local* `Project.id`, preferring the remote URL so
//! two different repos that happen to share a folder name are not confused.

use std::path::Path;

use crate::sync::meta::ProjectIdentity;

/// Build a portable identity for a local project: its folder name plus the
/// `origin` remote URL (if the source is a git repo with a remote).
pub fn project_identity(name: &str, source_path: &Path) -> ProjectIdentity {
    ProjectIdentity {
        name: name.to_string(),
        git_remote: crate::git::remote_url(source_path, "origin"),
    }
}

/// Reduce a git remote URL to a scheme/user-agnostic `host/path` form so an SSH
/// remote and its HTTPS twin compare equal. For example
/// `git@github.com:devergehq/allele.git` and
/// `https://github.com/devergehq/allele` both canonicalize to
/// `github.com/devergehq/allele`.
pub fn canonical_remote(url: &str) -> String {
    let mut had_scheme = false;
    let mut rest = url.trim();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(stripped) = rest.strip_prefix(scheme) {
            rest = stripped;
            had_scheme = true;
            break;
        }
    }
    // Drop any `user@` prefix (e.g. `git@`).
    let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);

    // scp-like syntax `host:path` (only when there was no `scheme://`) uses a
    // colon to separate host from path — normalize it to a slash. We leave
    // scheme URLs alone so a `host:port` is not mangled.
    let mut owned = rest.to_string();
    if !had_scheme {
        if let Some(pos) = owned.find(':') {
            owned.replace_range(pos..=pos, "/");
        }
    }

    let trimmed = owned.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    trimmed.trim_end_matches('/').to_ascii_lowercase()
}

/// A local project the resolver may match an incoming bundle against.
pub struct Candidate<'a> {
    /// The local `Project.id` to return on a match.
    pub project_id: &'a str,
    /// The local project's identity (built via [`project_identity`]).
    pub identity: &'a ProjectIdentity,
}

/// Resolve an incoming bundle's project identity to a *local* project id.
///
/// Matching order:
/// 1. **Canonical remote URL** — authoritative; wins even if names differ.
/// 2. **Case-insensitive folder name** — but a candidate whose remote provably
///    differs from the target's is skipped (same name, different repo).
///
/// `None` means the target machine must add the project before the session can
/// be pulled (the sync gate blocks).
pub fn resolve<'a>(target: &ProjectIdentity, candidates: &[Candidate<'a>]) -> Option<&'a str> {
    let target_remote = target.git_remote.as_deref().map(canonical_remote);

    // 1. Remote match.
    if let Some(target_remote) = target_remote.as_deref() {
        if let Some(hit) = candidates.iter().find(|c| {
            c.identity
                .git_remote
                .as_deref()
                .map(canonical_remote)
                .as_deref()
                == Some(target_remote)
        }) {
            return Some(hit.project_id);
        }
    }

    // 2. Name fallback.
    for candidate in candidates {
        if !candidate.identity.name.eq_ignore_ascii_case(&target.name) {
            continue;
        }
        // Same name, but if both carry remotes that differ, they are different
        // repos — do not match.
        if let (Some(target_remote), Some(candidate_remote)) = (
            target_remote.as_deref(),
            candidate
                .identity
                .git_remote
                .as_deref()
                .map(canonical_remote),
        ) {
            if target_remote != candidate_remote {
                continue;
            }
        }
        return Some(candidate.project_id);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(name: &str, remote: Option<&str>) -> ProjectIdentity {
        ProjectIdentity {
            name: name.to_string(),
            git_remote: remote.map(String::from),
        }
    }

    #[test]
    fn canonical_remote_normalizes_ssh_https_and_git_suffix() {
        let ssh = canonical_remote("git@github.com:devergehq/allele.git");
        assert_eq!(ssh, "github.com/devergehq/allele");
        assert_eq!(
            canonical_remote("https://github.com/devergehq/allele.git"),
            ssh
        );
        assert_eq!(canonical_remote("https://github.com/devergehq/allele"), ssh);
        assert_eq!(
            canonical_remote("ssh://git@github.com/devergehq/allele"),
            ssh
        );
        assert_eq!(
            canonical_remote("  git@github.com:devergehq/allele.git/ "),
            ssh
        );
        // Different repo does not collide.
        assert_ne!(canonical_remote("git@github.com:devergehq/other.git"), ssh);
    }

    #[test]
    fn resolve_prefers_remote_over_name() {
        let a = ident("proj", Some("git@github.com:org/a.git"));
        let b = ident("different-name", Some("https://github.com/org/target.git"));
        let candidates = [
            Candidate {
                project_id: "id-a",
                identity: &a,
            },
            Candidate {
                project_id: "id-b",
                identity: &b,
            },
        ];
        // Target shares a's NAME but b's REPO — remote wins.
        let target = ident("proj", Some("ssh://git@github.com/org/target"));
        assert_eq!(resolve(&target, &candidates), Some("id-b"));
    }

    #[test]
    fn resolve_falls_back_to_name_when_no_remotes() {
        let a = ident("allele", None);
        let candidates = [Candidate {
            project_id: "id-a",
            identity: &a,
        }];
        // Case-insensitive.
        let target = ident("Allele", None);
        assert_eq!(resolve(&target, &candidates), Some("id-a"));
    }

    #[test]
    fn resolve_blocks_same_name_different_remote() {
        let local = ident("allele", Some("git@github.com:someoneelse/allele.git"));
        let candidates = [Candidate {
            project_id: "id-a",
            identity: &local,
        }];
        let target = ident("allele", Some("git@github.com:devergehq/allele.git"));
        // Same folder name, provably different repos → blocked.
        assert_eq!(resolve(&target, &candidates), None);
    }

    #[test]
    fn resolve_none_when_no_candidates() {
        let target = ident("allele", Some("git@github.com:devergehq/allele.git"));
        assert_eq!(resolve(&target, &[]), None);
    }

    #[test]
    fn project_identity_captures_origin_remote() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]);
        git(
            repo,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:devergehq/allele.git",
            ],
        );

        let id = project_identity("allele", repo);
        assert_eq!(id.name, "allele");
        assert_eq!(
            id.git_remote.as_deref(),
            Some("git@github.com:devergehq/allele.git")
        );
        // Canonicalizes to match the HTTPS twin.
        assert_eq!(
            canonical_remote(id.git_remote.as_deref().unwrap()),
            canonical_remote("https://github.com/devergehq/allele")
        );
    }

    #[test]
    fn project_identity_no_remote_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]);
        let id = project_identity("local-only", repo);
        assert_eq!(id.git_remote, None);
    }

    fn git(repo: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("spawn git")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }
}
