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
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub sessions: Vec<PersistedSession>,
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
