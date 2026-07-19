//! Portable, machine-independent representation of a session (DEV-190).
//!
//! A [`crate::state::PersistedSession`] embeds the absolute APFS clone path,
//! which lives under the local home dir and so differs on every machine.
//! [`SessionBundleMeta`] strips that to a path relative to the workspaces root
//! (`~/.allele/workspaces`), plus the subset of session fields that are
//! meaningful on another machine. Machine-local state (browser tab ids, drawer
//! layout, live status, merge overrides) is intentionally dropped and defaulted
//! on import.
//!
//! **Invariant:** a serialized bundle contains NO absolute paths — no home dir,
//! no `/Users/…`. Enforced by [`tests::serialized_bundle_has_no_absolute_paths`].

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::session::SessionStatus;
use crate::state::PersistedSession;

/// Sync bookkeeping carried alongside the session payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncHeader {
    /// Monotonic revision, bumped on every push. Divergence detection (Phase 3)
    /// compares this against the device's last-synced base — NOT wall-clock
    /// time, which drifts across machines.
    pub revision: u64,
    /// Stable id of the device that last pushed this bundle. Display only.
    pub last_writer_device: String,
    /// Wall-clock time of the last push. Display only.
    pub updated_at: SystemTime,
}

/// Project identity carried in the bundle so the target machine can match the
/// session to a local project. DEV-191 populates `git_remote`; the resolver
/// matches on remote URL first and falls back to `name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIdentity {
    /// Opened-folder name (≈ repo name) — the legacy key.
    pub name: String,
    /// Git remote URL, the sturdier key. `None` until DEV-191 populates it, or
    /// for projects with no remote.
    #[serde(default)]
    pub git_remote: Option<String>,
}

/// Portable form of a [`PersistedSession`] — the unit that travels through the
/// [`crate::sync::SyncStore`]. Only fields that survive a move to another
/// machine are carried.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBundleMeta {
    /// Session UUID — also Claude's session id. Stable across machines.
    pub id: String,
    /// Post-`/clear` conversation id, if it has diverged from `id`.
    #[serde(default)]
    pub claude_session_id: Option<String>,
    /// Sidebar display label.
    pub label: String,
    /// Optional user comment shown as a row subtitle.
    #[serde(default)]
    pub comment: Option<String>,
    /// Git branch backing the session (the target rematerializes code from it).
    #[serde(default)]
    pub branch_name: Option<String>,
    /// Clone path RELATIVE to `~/.allele/workspaces` (e.g. `allele/4989c913`).
    /// Rebased onto the local workspaces root on import. `None` if the source
    /// session had no clone, or its clone lived outside the workspaces root.
    #[serde(default)]
    pub clone_rel: Option<PathBuf>,
    /// Coding agent that spawned the session (resume uses it).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Whether the session was pinned to the top of its project list.
    #[serde(default)]
    pub pinned: bool,
    /// Original creation time.
    pub started_at: SystemTime,
    /// Last observed activity time.
    pub last_active: SystemTime,
    /// Banked active runtime in whole seconds.
    #[serde(default)]
    pub active_runtime_secs: u64,
    /// Owning project's portable identity (matched to a local project on import).
    pub project: ProjectIdentity,
    /// Sync bookkeeping (revision, writer device, timestamp).
    pub sync: SyncHeader,
}

impl SessionBundleMeta {
    /// Build a portable bundle from a local session row. `project` and `header`
    /// come from the project resolver (DEV-191) and the sync ledger (DEV-192).
    pub fn from_persisted(
        session: &PersistedSession,
        project: ProjectIdentity,
        header: SyncHeader,
    ) -> Self {
        let clone_rel = session
            .clone_path
            .as_ref()
            .and_then(|abs| relativize_clone(abs));
        Self {
            id: session.id.clone(),
            claude_session_id: session.claude_session_id.clone(),
            label: session.label.clone(),
            comment: session.comment.clone(),
            branch_name: session.branch_name.clone(),
            clone_rel,
            agent_id: session.agent_id.clone(),
            pinned: session.pinned,
            started_at: session.started_at,
            last_active: session.last_active,
            active_runtime_secs: session.active_runtime_secs,
            project,
            sync: header,
        }
    }

    /// Rebuild a local [`PersistedSession`] for this machine. The clone path is
    /// rebased onto THIS machine's workspaces root; the session rehydrates as
    /// [`SessionStatus::Suspended`], and every machine-local field defaults
    /// (browser/drawer state, merge overrides, branch lock).
    ///
    /// `project_id` is the *local* `Project.id` the resolver matched this
    /// bundle to — it is per-machine and never travels in the bundle itself.
    pub fn to_persisted(&self, project_id: &str) -> PersistedSession {
        let clone_path = self.clone_rel.as_deref().and_then(localize_clone);
        PersistedSession {
            id: self.id.clone(),
            claude_session_id: self.claude_session_id.clone(),
            project_id: project_id.to_string(),
            label: self.label.clone(),
            clone_path,
            // Imported sessions always land Suspended — no PTY is attached until
            // the user cold-resumes (Phase 2 wires materialization + resume).
            last_known_status: SessionStatus::Suspended,
            started_at: self.started_at,
            last_active: self.last_active,
            active_runtime_secs: self.active_runtime_secs,
            merged: false,
            drawer_tab_names: Vec::new(),
            drawer_active_tab: 0,
            browser_tab_id: None,
            browser_last_url: None,
            agent_id: self.agent_id.clone(),
            pinned: self.pinned,
            comment: self.comment.clone(),
            branch_name: self.branch_name.clone(),
            merge_strategy_override: None,
            branch_locked: false,
        }
    }
}

/// Strip the workspaces-root prefix from an absolute clone path, yielding the
/// portable relative form. Returns `None` (with a warning) if the path is not
/// under `~/.allele/workspaces` — such a clone can't be rebased, and the target
/// will recreate the workspace from git instead.
fn relativize_clone(abs: &Path) -> Option<PathBuf> {
    let root = crate::clone::clones_root()?;
    match abs.strip_prefix(&root) {
        Ok(rel) => Some(rel.to_path_buf()),
        Err(_) => {
            warn!(
                "session-sync: clone path {} is outside the workspaces root {} — \
                 dropping it from the bundle (target will rematerialize from git)",
                abs.display(),
                root.display()
            );
            None
        }
    }
}

/// Rebase a portable relative clone path onto this machine's workspaces root.
fn localize_clone(rel: &Path) -> Option<PathBuf> {
    Some(crate::clone::clones_root()?.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SyncHeader {
        SyncHeader {
            revision: 1,
            last_writer_device: "device-A".into(),
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn identity() -> ProjectIdentity {
        ProjectIdentity {
            name: "allele".into(),
            git_remote: Some("git@github.com:devergehq/allele.git".into()),
        }
    }

    /// A fully-populated row, including machine-local fields, to prove they are
    /// dropped on import.
    fn sample(clone: Option<PathBuf>) -> PersistedSession {
        PersistedSession {
            id: "4989c913-e28a-482a-9c11-abcdef012345".into(),
            claude_session_id: Some("rotated-after-clear".into()),
            project_id: "local-project-uuid".into(),
            label: "My session".into(),
            clone_path: clone,
            last_known_status: SessionStatus::Running,
            started_at: SystemTime::UNIX_EPOCH,
            last_active: SystemTime::UNIX_EPOCH,
            active_runtime_secs: 120,
            merged: true,
            drawer_tab_names: vec!["build".into(), "logs".into()],
            drawer_active_tab: 1,
            browser_tab_id: Some(7),
            browser_last_url: Some("https://example.test/x".into()),
            agent_id: Some("claude".into()),
            pinned: true,
            comment: Some("wip on auth".into()),
            branch_name: Some("fix-auth-5dc47535".into()),
            merge_strategy_override: None,
            branch_locked: true,
        }
    }

    #[test]
    fn roundtrip_rebases_clone_path() {
        let root = crate::clone::clones_root().expect("home dir");
        let abs = root.join("allele").join("4989c913");
        let s = sample(Some(abs.clone()));

        let meta = SessionBundleMeta::from_persisted(&s, identity(), header());
        assert_eq!(
            meta.clone_rel.as_deref(),
            Some(Path::new("allele/4989c913")),
            "clone path should be stored relative to the workspaces root"
        );

        let back = meta.to_persisted("local-project-uuid");
        assert_eq!(
            back.clone_path,
            Some(abs),
            "rebases to the same absolute path"
        );
        assert_eq!(back.id, s.id);
        assert_eq!(back.claude_session_id, s.claude_session_id);
        assert_eq!(back.branch_name, s.branch_name);
        // Imported rows are always Suspended, regardless of source status.
        assert_eq!(back.last_known_status, SessionStatus::Suspended);
    }

    #[test]
    fn serialized_bundle_has_no_absolute_paths() {
        let root = crate::clone::clones_root().expect("home dir");
        let abs = root.join("allele").join("abcd1234");
        let meta = SessionBundleMeta::from_persisted(&sample(Some(abs)), identity(), header());

        let json = serde_json::to_string(&meta).expect("serialize");
        assert!(
            !json.contains("/Users/"),
            "bundle leaked an absolute path: {json}"
        );
        let home = dirs::home_dir().expect("home dir");
        assert!(
            !json.contains(home.to_str().expect("utf8 home")),
            "bundle leaked the home dir: {json}"
        );
    }

    #[test]
    fn import_defaults_machine_local_fields() {
        let meta = SessionBundleMeta::from_persisted(&sample(None), identity(), header());
        let back = meta.to_persisted("some-project");

        // Machine-local fields are dropped/defaulted on import.
        assert_eq!(back.browser_tab_id, None);
        assert_eq!(back.browser_last_url, None);
        assert!(back.drawer_tab_names.is_empty());
        assert_eq!(back.drawer_active_tab, 0);
        assert!(back.merge_strategy_override.is_none());
        assert!(!back.branch_locked);
        assert!(!back.merged);
        assert_eq!(back.clone_path, None);

        // Portable fields survive intact.
        assert_eq!(back.label, "My session");
        assert!(back.pinned);
        assert_eq!(back.comment.as_deref(), Some("wip on auth"));
        assert_eq!(back.agent_id.as_deref(), Some("claude"));
        assert_eq!(back.active_runtime_secs, 120);
        assert_eq!(back.project_id, "some-project");
    }
}
