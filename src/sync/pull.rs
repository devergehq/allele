//! Sync-down: browse remote sessions and pull one into local state (DEV-194).
//!
//! The pull half of the pipe. Lists the bundles in the store (decrypted headers
//! for the browser UI), and — for a chosen one — resolves its project against
//! the machine's local projects (the sync gate, DEV-191) and upserts a single
//! `Suspended` row into local state. A bundle whose project isn't present is
//! blocked ("add the project first"). The UI (browser + click) is DEV-195; this
//! is the testable core.
//!
//! Nothing here materializes a workspace or transcript — a pulled row is inert
//! until DEV-193/Phase-2 resume wiring. This layer only moves metadata.

use std::time::SystemTime;

use super::identity::{self, Candidate};
use super::ledger::SyncLedger;
use super::meta::SessionBundleMeta;
use super::store::{meta_key, SyncStore, SESSIONS_PREFIX};
use crate::state::{PersistedSession, PersistedState};

/// A remote session available to pull — the decoded bundle header shown in the
/// browser so the user can pick one.
#[derive(Debug, Clone)]
pub struct RemoteSession {
    pub id: String,
    pub label: String,
    pub project_name: String,
    pub git_remote: Option<String>,
    pub revision: u64,
    pub last_writer_device: String,
    pub updated_at: SystemTime,
}

/// List every session bundle in the store (decrypting each header), sorted by
/// label. A malformed/foreign object under the prefix is skipped, not fatal.
pub async fn list_remote_sessions(store: &dyn SyncStore) -> anyhow::Result<Vec<RemoteSession>> {
    let mut sessions = Vec::new();
    for key in store.list(SESSIONS_PREFIX).await? {
        if !key.ends_with("/meta.json") {
            continue;
        }
        let Some(bytes) = store.get(&key).await? else {
            continue;
        };
        let meta: SessionBundleMeta = match serde_json::from_slice(&bytes) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        sessions.push(RemoteSession {
            id: meta.id,
            label: meta.label,
            project_name: meta.project.name,
            git_remote: meta.project.git_remote,
            revision: meta.sync.revision,
            last_writer_device: meta.sync.last_writer_device,
            updated_at: meta.sync.updated_at,
        });
    }
    sessions.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(sessions)
}

/// Fetch a session's raw Claude transcript bytes, if one was synced.
pub async fn fetch_transcript(
    store: &dyn SyncStore,
    session_id: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    store.get(&super::store::transcript_key(session_id)).await
}

/// Fetch and decode a single session bundle.
pub async fn fetch_bundle(
    store: &dyn SyncStore,
    session_id: &str,
) -> anyhow::Result<Option<SessionBundleMeta>> {
    match store.get(&meta_key(session_id)).await? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        None => Ok(None),
    }
}

/// Outcome of resolving a pulled bundle against local projects (the sync gate).
pub enum PullResolution {
    /// Matched a local project; ready to upsert as this `PersistedSession`.
    Ready(PersistedSession),
    /// The project isn't present locally — blocked; add it first.
    ProjectMissing { name: String },
}

/// Resolve a bundle's project identity against local candidates and, on a
/// match, build the local `PersistedSession` (rebased clone path, `Suspended`).
pub fn resolve_pull(meta: &SessionBundleMeta, candidates: &[Candidate]) -> PullResolution {
    match identity::resolve(&meta.project, candidates) {
        Some(project_id) => PullResolution::Ready(meta.to_persisted(project_id)),
        None => PullResolution::ProjectMissing {
            name: meta.project.name.clone(),
        },
    }
}

/// Upsert a pulled session into local state (replace by id, else append) and
/// record the pulled revision as this device's base. Caller persists state +
/// ledger afterward.
pub fn apply_pull(
    state: &mut PersistedState,
    ledger: &mut SyncLedger,
    session: PersistedSession,
    revision: u64,
) {
    ledger.record_synced(&session.id, revision);
    match state.sessions.iter_mut().find(|s| s.id == session.id) {
        Some(existing) => *existing = session,
        None => state.sessions.push(session),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStatus;
    use crate::sync::crypto::DataKey;
    use crate::sync::encrypting_store::EncryptingStore;
    use crate::sync::meta::{ProjectIdentity, SyncHeader};
    use crate::sync::push::push_session_bundle;
    use crate::sync::store::MemStore;

    fn sample_session(id: &str) -> PersistedSession {
        PersistedSession {
            id: id.to_string(),
            claude_session_id: None,
            project_id: "local-A-project".into(),
            label: "My session".into(),
            clone_path: None,
            last_known_status: SessionStatus::Running,
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

    fn bundle(id: &str, project: ProjectIdentity) -> SessionBundleMeta {
        SessionBundleMeta::from_persisted(
            &sample_session(id),
            project,
            SyncHeader {
                revision: 1,
                last_writer_device: "mac-A".into(),
                updated_at: SystemTime::UNIX_EPOCH,
            },
        )
    }

    #[tokio::test]
    async fn push_then_list_and_pull_roundtrip() {
        // One shared bucket + key stands in for Mac A pushing and Mac B pulling.
        let store = EncryptingStore::new(MemStore::new(), DataKey::generate());
        let mut ledger_a = SyncLedger::default();
        push_session_bundle(
            &store,
            &mut ledger_a,
            &sample_session("sess-42"),
            identity(),
            "mac-A",
            SystemTime::UNIX_EPOCH,
        )
        .await
        .unwrap();

        // Mac B browses.
        let listed = list_remote_sessions(&store).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "sess-42");
        assert_eq!(listed[0].revision, 1);
        assert_eq!(listed[0].project_name, "allele");

        // Mac B pulls; its local project matches by remote URL.
        let bundle = fetch_bundle(&store, "sess-42").await.unwrap().unwrap();
        let local_identity = identity();
        let candidates = [Candidate {
            project_id: "local-B-project",
            identity: &local_identity,
        }];
        let session_b = match resolve_pull(&bundle, &candidates) {
            PullResolution::Ready(s) => s,
            PullResolution::ProjectMissing { .. } => panic!("expected a match"),
        };
        assert_eq!(session_b.id, "sess-42");
        assert_eq!(session_b.project_id, "local-B-project"); // re-homed to B's project
        assert_eq!(session_b.last_known_status, SessionStatus::Suspended);

        let mut state_b = PersistedState::default();
        let mut ledger_b = SyncLedger::default();
        apply_pull(&mut state_b, &mut ledger_b, session_b, bundle.sync.revision);
        assert_eq!(state_b.sessions.len(), 1);
        assert_eq!(ledger_b.base_revision("sess-42"), Some(1));

        // Pulling again upserts — no duplicate row.
        let bundle2 = fetch_bundle(&store, "sess-42").await.unwrap().unwrap();
        if let PullResolution::Ready(s) = resolve_pull(&bundle2, &candidates) {
            apply_pull(&mut state_b, &mut ledger_b, s, bundle2.sync.revision);
        }
        assert_eq!(state_b.sessions.len(), 1, "pull must upsert, not duplicate");
    }

    #[test]
    fn resolve_pull_blocks_when_project_missing() {
        let meta = bundle(
            "sess-7",
            ProjectIdentity {
                name: "ghost-project".into(),
                git_remote: Some("git@github.com:someone/ghost.git".into()),
            },
        );
        match resolve_pull(&meta, &[]) {
            PullResolution::ProjectMissing { name } => assert_eq!(name, "ghost-project"),
            PullResolution::Ready(_) => panic!("should have blocked — no local project"),
        }
    }

    #[test]
    fn apply_pull_replaces_existing_row() {
        let mut state = PersistedState::default();
        let mut ledger = SyncLedger::default();

        let mut first = sample_session("sess-1");
        first.label = "old label".into();
        apply_pull(&mut state, &mut ledger, first, 1);

        let mut second = sample_session("sess-1");
        second.label = "new label".into();
        apply_pull(&mut state, &mut ledger, second, 2);

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].label, "new label");
        assert_eq!(ledger.base_revision("sess-1"), Some(2));
    }
}
