//! Sync-up: build a session's bundle and upload it (DEV-193).
//!
//! Orchestrates the pieces built earlier — [`SessionBundleMeta`] (schema),
//! [`SyncLedger`] (revision), and a [`SyncStore`] (transport, wrapped in
//! `EncryptingStore` by the caller so this layer never sees plaintext at rest).
//! The UI trigger (a sidebar button) is wired in DEV-195; this is the testable
//! core it calls.
//!
//! Sync-up also has a **git precondition**: the bundle carries the transcript
//! but not the code, so a session whose branch is dirty or unpushed would
//! materialize a mismatched workspace on the other machine (design §2.5).
//! [`check_push_ready`] reports that; the caller warns/blocks on it.

use std::path::Path;
use std::time::SystemTime;

use super::ledger::SyncLedger;
use super::meta::{ProjectIdentity, SessionBundleMeta, SyncHeader};
use super::store::{meta_key, SyncStore};
use crate::state::PersistedSession;

/// Git state relevant to whether a session is safe to sync up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushReadiness {
    /// The working tree has uncommitted changes.
    pub dirty: bool,
    /// Commits on the branch not yet on its upstream. `None` = no upstream
    /// configured (the branch isn't pushed anywhere).
    pub unpushed: Option<usize>,
}

impl PushReadiness {
    /// Ready only when the tree is clean and the branch is fully pushed.
    pub fn is_ready(&self) -> bool {
        !self.dirty && self.unpushed == Some(0)
    }

    /// A human-facing warning when not ready, or `None` when it is.
    pub fn warning(&self) -> Option<String> {
        if self.is_ready() {
            return None;
        }
        let mut reasons = Vec::new();
        if self.dirty {
            reasons.push("the working tree has uncommitted changes".to_string());
        }
        match self.unpushed {
            None => reasons.push("the branch has no upstream (not pushed)".to_string()),
            Some(n) if n > 0 => reasons.push(format!("{n} commit(s) are not pushed")),
            Some(_) => {}
        }
        Some(format!(
            "This session's code isn't fully on the remote ({}). \
             The other machine rebuilds the workspace from git, so push the branch first.",
            reasons.join("; ")
        ))
    }
}

/// Inspect a session's workspace/source repo for sync-up readiness.
pub fn check_push_ready(repo: &Path) -> PushReadiness {
    PushReadiness {
        dirty: crate::git::is_working_tree_dirty(repo),
        unpushed: crate::git::unpushed_commit_count(repo),
    }
}

/// Build the session's bundle and upload it, returning the revision pushed.
///
/// `store` is expected to be encryption-wrapped by the caller. `project` and
/// `device_id` come from the resolver (DEV-191) and settings (DEV-188); `now`
/// is injected for testability. The revision is `max(local base, remote) + 1`
/// so it never moves backward, and the ledger records the new base on success.
pub async fn push_session_bundle(
    store: &dyn SyncStore,
    ledger: &mut SyncLedger,
    session: &PersistedSession,
    project: ProjectIdentity,
    device_id: &str,
    now: SystemTime,
) -> anyhow::Result<u64> {
    let remote_revision = read_remote_revision(store, &session.id).await?;
    let revision = ledger.next_push_revision(&session.id, remote_revision);

    let header = SyncHeader {
        revision,
        last_writer_device: device_id.to_string(),
        updated_at: now,
    };
    let meta = SessionBundleMeta::from_persisted(session, project, header);
    let bytes = serde_json::to_vec(&meta)?;
    store.put(&meta_key(&session.id), bytes).await?;

    ledger.record_synced(&session.id, revision);
    Ok(revision)
}

/// Upload a session's Claude transcript bytes alongside its bundle (the Phase-2
/// payload that lets the conversation replay on another Mac). Best-effort:
/// callers skip it when the local transcript file doesn't exist yet.
pub async fn push_transcript(
    store: &dyn SyncStore,
    session_id: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<()> {
    store
        .put(&super::store::transcript_key(session_id), bytes)
        .await
}

/// The revision currently in the store for `session_id`, if a bundle exists.
async fn read_remote_revision(
    store: &dyn SyncStore,
    session_id: &str,
) -> anyhow::Result<Option<u64>> {
    match store.get(&meta_key(session_id)).await? {
        Some(bytes) => {
            let meta: SessionBundleMeta = serde_json::from_slice(&bytes)?;
            Ok(Some(meta.sync.revision))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStatus;
    use crate::sync::crypto::DataKey;
    use crate::sync::encrypting_store::EncryptingStore;
    use crate::sync::store::MemStore;

    fn sample_session(id: &str) -> PersistedSession {
        PersistedSession {
            id: id.to_string(),
            claude_session_id: None,
            project_id: "local-project".into(),
            label: "My session".into(),
            clone_path: None,
            last_known_status: SessionStatus::Idle,
            started_at: SystemTime::UNIX_EPOCH,
            last_active: SystemTime::UNIX_EPOCH,
            active_runtime_secs: 0,
            merged: false,
            drawer_tab_names: Vec::new(),
            drawer_active_tab: 0,
            browser_tab_id: None,
            browser_last_url: None,
            agent_id: None,
            pinned: false,
            comment: None,
            branch_name: Some("fix-auth".into()),
            merge_strategy_override: None,
            branch_locked: false,
        }
    }

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            name: "allele".into(),
            git_remote: Some("git@github.com:devergehq/allele.git".into()),
        }
    }

    #[tokio::test]
    async fn push_uploads_encrypted_bundle_and_bumps_revision() {
        let store = EncryptingStore::new(MemStore::new(), DataKey::generate());
        let mut ledger = SyncLedger::default();
        let session = sample_session("sess-1");

        let rev = push_session_bundle(
            &store,
            &mut ledger,
            &session,
            identity(),
            "device-A",
            SystemTime::UNIX_EPOCH,
        )
        .await
        .unwrap();
        assert_eq!(rev, 1);
        assert_eq!(ledger.base_revision("sess-1"), Some(1));

        // At rest it's ciphertext (label must not appear in the stored bytes).
        let at_rest = store
            .inner()
            .get(&meta_key("sess-1"))
            .await
            .unwrap()
            .unwrap();
        assert!(!at_rest.windows(2).any(|w| w == b"My"));

        // Decodes to the expected bundle.
        let bytes = store.get(&meta_key("sess-1")).await.unwrap().unwrap();
        let meta: SessionBundleMeta = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(meta.id, "sess-1");
        assert_eq!(meta.sync.revision, 1);
        assert_eq!(meta.sync.last_writer_device, "device-A");
        assert_eq!(meta.project.name, "allele");

        // Second push sees remote revision 1 → bumps to 2.
        let rev2 = push_session_bundle(
            &store,
            &mut ledger,
            &session,
            identity(),
            "device-A",
            SystemTime::UNIX_EPOCH,
        )
        .await
        .unwrap();
        assert_eq!(rev2, 2);
    }

    #[test]
    fn readiness_ready_only_when_clean_and_pushed() {
        assert!(PushReadiness {
            dirty: false,
            unpushed: Some(0)
        }
        .is_ready());
        assert!(!PushReadiness {
            dirty: true,
            unpushed: Some(0)
        }
        .is_ready());
        assert!(!PushReadiness {
            dirty: false,
            unpushed: None
        }
        .is_ready());
        assert!(!PushReadiness {
            dirty: false,
            unpushed: Some(3)
        }
        .is_ready());
        assert!(PushReadiness {
            dirty: false,
            unpushed: Some(0)
        }
        .warning()
        .is_none());
        assert!(PushReadiness {
            dirty: true,
            unpushed: None
        }
        .warning()
        .is_some());
    }

    #[test]
    fn check_push_ready_detects_dirty_and_missing_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q"]);
        std::fs::write(repo.join("f.txt"), "hi").unwrap();
        git(repo, &["add", "-A"]);
        git(
            repo,
            &[
                "-c",
                "user.email=t@t.co",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "c",
            ],
        );

        // Clean tree, but no upstream configured → not ready.
        let r = check_push_ready(repo);
        assert!(!r.dirty);
        assert_eq!(r.unpushed, None);
        assert!(!r.is_ready());

        // Dirty the tree → still not ready, now flagged dirty.
        std::fs::write(repo.join("f.txt"), "changed").unwrap();
        let r2 = check_push_ready(repo);
        assert!(r2.dirty);
        assert!(!r2.is_ready());
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
