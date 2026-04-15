// Persistent session state — tracks sessions across app restarts so that
// Claude Code conversations can be cold-resumed via `claude --resume <uuid>`.
//
// Stored at `~/.allele/state.json`. Writes are atomic (temp + rename).
// Loads are defensive — a missing or unparseable file returns an empty state
// rather than panicking.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::session::{Session, SessionStatus};

/// One persisted session row — everything we need to rehydrate a sidebar
/// entry and later cold-resume the Claude conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Stable UUID — matches both `Session.id` in memory and Claude's
    /// own session ID (we force it via `claude --session-id <uuid>`).
    pub id: String,
    /// Links back to the owning `Project.id` in settings.json.
    pub project_id: String,
    /// Display label for the sidebar.
    pub label: String,
    /// APFS clone path for this session. This is the cwd we'll re-enter
    /// when cold-resuming via `claude --resume <id>`.
    pub clone_path: Option<PathBuf>,
    /// Last known status when the session was persisted. Rehydrated sessions
    /// are always shown as `Suspended` regardless of what's stored here —
    /// this field is kept for diagnostics.
    pub last_known_status: SessionStatus,
    /// Wall-clock time the session was originally created.
    pub started_at: SystemTime,
    /// Wall-clock time we last observed activity on the session.
    pub last_active: SystemTime,
    /// True if this session's work was already merged into canonical via
    /// merge-and-close. When set, discard skips creating an archive entry.
    #[serde(default)]
    pub merged: bool,
    /// Drawer terminal tab names at save time. Tabs are re-spawned with
    /// these names when the drawer is first opened on the rehydrated session.
    #[serde(default)]
    pub drawer_tab_names: Vec<String>,
    /// Index of the active drawer tab at save time.
    #[serde(default)]
    pub drawer_active_tab: usize,
    /// Chrome tab id linked to this session. May be stale after Chrome
    /// restart — reconciled on first sync after rehydration.
    #[serde(default)]
    pub browser_tab_id: Option<i64>,
    /// Last URL seen on the linked tab.
    #[serde(default)]
    pub browser_last_url: Option<String>,
    /// Id of the coding agent that originally spawned the session.
    #[serde(default)]
    pub agent_id: Option<String>,
}

impl PersistedSession {
    pub fn from_session(session: &Session, project_id: &str) -> Self {
        Self {
            id: session.id.clone(),
            project_id: project_id.to_string(),
            label: session.label.clone(),
            clone_path: session.clone_path.clone(),
            last_known_status: session.status,
            started_at: session.started_at,
            last_active: session.last_active,
            merged: session.merged,
            drawer_tab_names: if session.drawer_tabs.is_empty() {
                // Tabs not yet materialised — preserve pending names from disk.
                session.pending_drawer_tab_names.clone()
            } else {
                session.drawer_tabs.iter().map(|t| t.name.clone()).collect()
            },
            drawer_active_tab: session.drawer_active_tab,
            browser_tab_id: session.browser_tab_id,
            browser_last_url: session.browser_last_url.clone(),
            agent_id: session.agent_id.clone(),
        }
    }
}

/// A session that was discarded and archived into canonical's git refs.
/// Stored in state.json so the archive browser can show a human-readable
/// label instead of a raw UUID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedSession {
    /// Session UUID — matches the `refs/allele/archive/<id>` ref in canonical.
    pub id: String,
    /// Owning project ID (links to `ProjectSave.id` in settings.json).
    pub project_id: String,
    /// Display label from the session's sidebar entry at discard time.
    pub label: String,
    /// Unix timestamp when the session was archived (seconds since epoch).
    pub archived_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub sessions: Vec<PersistedSession>,
    #[serde(default)]
    pub archived_sessions: Vec<ArchivedSession>,
    /// Session ID that was active when the app last saved state. On next
    /// launch, auto-resume this session so the user lands back in their
    /// conversation without clicking. `None` → no auto-resume.
    #[serde(default)]
    pub last_active_session_id: Option<String>,
}

impl PersistedState {
    /// Path to `~/.allele/state.json`. Co-located with the workspaces
    /// directory so a single `.allele/` folder owns everything session-
    /// related (workspaces, trash, state).
    pub fn path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        Some(home.join(".allele").join("state.json"))
    }

    /// Load state from disk. Returns an empty state if:
    /// - the file does not exist (first run)
    /// - the file cannot be read (permissions, etc.)
    /// - the file cannot be parsed (corruption)
    ///
    /// In the parse-failure case we log a warning so the user knows what
    /// happened, but we do NOT crash the app.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            eprintln!("state.json: no home directory — starting with empty state");
            return Self::default();
        };

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<Self>(&contents) {
                Ok(state) => state,
                Err(e) => {
                    eprintln!(
                        "state.json at {} failed to parse ({e}) — starting with empty state",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!(
                    "state.json at {} could not be read ({e}) — starting with empty state",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Atomically save state to disk. Writes to `state.json.tmp` first, then
    /// renames over `state.json` — either the new state is fully on disk or
    /// the old state is untouched. Never leaves a half-written file.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path()
            .ok_or_else(|| anyhow::anyhow!("no home directory"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");

        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Collect every clone path referenced by any persisted session. Used by the
/// orphan sweep to distinguish live clones from leaked ones.
pub fn referenced_clone_paths(state: &PersistedState) -> std::collections::HashSet<PathBuf> {
    state
        .sessions
        .iter()
        .filter_map(|s| s.clone_path.clone())
        .map(|p| canonical_or_raw(&p))
        .collect()
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
