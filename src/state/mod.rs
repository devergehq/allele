// Persistent session state — tracks sessions across app restarts so that
// Claude Code conversations can be cold-resumed via `claude --resume <uuid>`.
//
// Stored at `~/.allele/state.json`. Writes are atomic (temp + rename).
// Loads are defensive — a missing or unparseable file returns an empty state
// rather than panicking.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::warn;

use crate::session::{Session, SessionStatus};

/// One persisted drawer tab: its name and the command it was running.
///
/// The command is what makes a rehydrated — or parked — drawer come back as
/// the thing it was rather than as a bare shell (DEV-445).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedDrawerTab {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<String>,
}

/// One persisted session row — everything we need to rehydrate a sidebar
/// entry and later cold-resume the Claude conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Stable UUID — matches both `Session.id` in memory and Claude's
    /// own session ID (we force it via `claude --session-id <uuid>`).
    pub id: String,
    /// The Claude conversation id currently backing this workspace. `None`
    /// (the back-compat default) means "same as `id`". Diverges from `id`
    /// once the session has been `/clear`ed — persisting it is what lets a
    /// cold resume pick up the post-clear transcript instead of the original.
    #[serde(default)]
    pub claude_session_id: Option<String>,
    /// Whether `claude_session_id` was chosen by the user in the conversation
    /// picker rather than inferred. Defaults false for state written before
    /// the field existed.
    #[serde(default)]
    pub conversation_choice_explicit: bool,
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
    /// Total banked active runtime in whole seconds — the sum of time the
    /// session spent "on" (see `SessionStatus::counts_toward_runtime`), never
    /// its wall-clock age. Legacy state.json files predating this field load
    /// as 0 (serde default), which resets those sessions' timers — intentional,
    /// since their historical active runtime is unrecoverable.
    #[serde(default)]
    pub active_runtime_secs: u64,
    /// True if this session's work was already merged into canonical via
    /// merge-and-close. When set, discard skips creating an archive entry.
    #[serde(default)]
    pub merged: bool,
    /// Drawer terminal tab names at save time.
    ///
    /// Superseded by `drawer_tabs`, which also carries each tab's command.
    /// Still *written*, so an older Allele reading this file restores the tab
    /// layout rather than tripping on a missing key — it gets bare shells,
    /// which is exactly what it did before DEV-445. Same degrade-forwards
    /// contract as `skip_orchestration` above.
    #[serde(default)]
    pub drawer_tab_names: Vec<String>,
    /// Drawer tabs at save time, each with the command it was running (DEV-445).
    ///
    /// Absent in files written before DEV-445; [`PersistedSession::drawer_tabs`]
    /// falls back to `drawer_tab_names` for those. Skipped when empty so a
    /// session with no drawer does not grow the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drawer_tabs: Vec<PersistedDrawerTab>,
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
    /// Pinned sessions sort to the top of their project's session list.
    #[serde(default)]
    pub pinned: bool,
    /// Optional user comment displayed as a subtitle on the session row.
    #[serde(default)]
    pub comment: Option<String>,
    /// Current git branch name (e.g. "fix-auth-5dc47535"). Used for orphan
    /// cleanup identification now that branches don't carry the allele/session/ prefix.
    #[serde(default)]
    pub branch_name: Option<String>,
    /// Per-session merge strategy override. `None` = project setting.
    #[serde(default)]
    pub merge_strategy_override: Option<crate::settings::MergeStrategy>,
    /// True when the user chose the session's branch explicitly; auto-naming
    /// must not rename it. Persisted so a placeholder-labelled session can't
    /// re-fire the rename after a restart.
    #[serde(default)]
    pub branch_locked: bool,
    /// Legacy DEV-400 flag: "run none of the project's orchestration".
    ///
    /// Superseded by `orchestration`, which splits the startup command from the
    /// drawer terminals. Still *written*, derived from `orchestration`, so an
    /// older Allele reading this file degrades sensibly rather than tripping on
    /// a missing key — it sees `StartupOnly` as full orchestration, which is the
    /// safe direction. `serde(default)` so state written before DEV-400 loads.
    #[serde(default)]
    pub skip_orchestration: bool,
    /// How much of the project's setup this session runs (DEV-415). Absent in
    /// files written before the split; [`PersistedSession::orchestration`]
    /// resolves those from `skip_orchestration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<crate::session::Orchestration>,
    /// Who started this session (DEV-415). `serde(default)` — everything
    /// written before dispatch existed was started by a human.
    #[serde(default)]
    pub origin: crate::session::SessionOrigin,
}

impl PersistedSession {
    /// The session's orchestration mode, resolving pre-DEV-415 state files.
    ///
    /// Reading the legacy bool rather than defaulting is what stops every
    /// lightweight session created under DEV-400 from silently spinning the
    /// whole project up on the first resume after upgrading.
    pub fn orchestration(&self) -> crate::session::Orchestration {
        self.orchestration.unwrap_or({
            if self.skip_orchestration {
                crate::session::Orchestration::Nothing
            } else {
                crate::session::Orchestration::Full
            }
        })
    }
}

/// The session's drawer tabs, wherever they currently live.
///
/// Live tabs and parked tabs are the same list in two representations, and the
/// [`Session`] invariant is that only one of them is ever populated.
fn drawer_snapshot(session: &Session) -> Vec<crate::session::ParkedTab> {
    if session.drawer_tabs.is_empty() {
        session.parked_drawer_tabs.clone()
    } else {
        session
            .drawer_tabs
            .iter()
            .map(|t| crate::session::ParkedTab {
                name: t.name.clone(),
                replay: t.replay.clone(),
            })
            .collect()
    }
}

impl PersistedSession {
    /// The session's drawer tabs, resolving pre-DEV-445 state files.
    ///
    /// Files written before DEV-445 carry names only; those tabs come back as
    /// bare shells, which is all the file can tell us. Reading the richer field
    /// first is what stops an upgrade from silently downgrading every restored
    /// drawer to a shell prompt.
    pub fn drawer_tabs(&self) -> Vec<crate::session::ParkedTab> {
        if !self.drawer_tabs.is_empty() {
            return self
                .drawer_tabs
                .iter()
                .map(|t| crate::session::ParkedTab {
                    name: t.name.clone(),
                    replay: t.replay.clone(),
                })
                .collect();
        }
        self.drawer_tab_names
            .iter()
            .cloned()
            .map(crate::session::ParkedTab::bare)
            .collect()
    }
}

impl PersistedSession {
    pub fn from_session(session: &Session, project_id: &str) -> Self {
        Self {
            id: session.id.clone(),
            claude_session_id: session.claude_session_id.clone(),
            conversation_choice_explicit: session.conversation_choice_explicit,
            project_id: project_id.to_string(),
            label: session.label.clone(),
            clone_path: session.clone_path.clone(),
            last_known_status: session.status,
            started_at: session.started_at,
            last_active: session.last_active,
            // Snapshot banked + in-flight runtime so a live session's progress
            // survives a save/quit (it rehydrates Suspended with this banked).
            active_runtime_secs: session.active_runtime().as_secs(),
            merged: session.merged,
            // A session's tabs live in exactly one of these two places — live
            // in `drawer_tabs`, or parked (rehydrated-but-unopened, or killed
            // by the idle reaper) in `parked_drawer_tabs`. Read whichever holds
            // them, and write both the rich and the legacy shape.
            drawer_tab_names: drawer_snapshot(session)
                .iter()
                .map(|t| t.name.clone())
                .collect(),
            drawer_tabs: drawer_snapshot(session)
                .into_iter()
                .map(|t| PersistedDrawerTab {
                    name: t.name,
                    replay: t.replay,
                })
                .collect(),
            drawer_active_tab: session.drawer_active_tab,
            browser_tab_id: session.browser_tab_id,
            browser_last_url: session.browser_last_url.clone(),
            agent_id: session.agent_id.clone(),
            pinned: session.pinned,
            comment: session.comment.clone(),
            branch_name: session.branch_name.clone(),
            merge_strategy_override: session.merge_strategy_override,
            branch_locked: session.branch_locked,
            // Derived, for older Allele versions reading this file.
            skip_orchestration: !session.orchestration.runs_startup(),
            orchestration: Some(session.orchestration),
            origin: session.origin.clone(),
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
    /// Transient merge failure, shown beside this archive with a retry action.
    #[serde(skip)]
    pub merge_error: Option<String>,
}

/// One saved Scratch Pad entry. Persisted so users can recall past
/// messages they sent to Claude via the compose overlay. Keyed by
/// `project_id` so history is naturally per-project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchPadEntry {
    /// Stable UUID for list keys / future deletion.
    pub id: String,
    /// Owning project ID (links to `ProjectSave.id` in settings.json).
    pub project_id: String,
    /// The composed text that was submitted. Attachments are not persisted.
    pub text: String,
    /// When the entry was submitted.
    pub created_at: SystemTime,
}

/// Per-project entry cap for scratch pad history. Prevents state.json
/// from growing unbounded for long-lived projects.
pub const SCRATCH_HISTORY_PER_PROJECT_LIMIT: usize = 50;

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
    /// Scratch Pad submission history, newest first across all projects.
    /// Consumers filter by `project_id` to show per-project history.
    #[serde(default)]
    pub scratch_pad_history: Vec<ScratchPadEntry>,
}

impl PersistedState {
    /// Path to `~/.allele/state.json`. Co-located with the workspaces
    /// directory so a single `.allele/` folder owns everything session-
    /// related (workspaces, trash, state).
    pub fn path() -> Option<PathBuf> {
        crate::paths::state_file()
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
            warn!("state.json: no home directory — starting with empty state");
            return Self::default();
        };

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<Self>(&contents) {
                Ok(state) => state,
                Err(e) => {
                    warn!(
                        "state.json at {} failed to parse ({e}) — starting with empty state",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(e) => {
                warn!(
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
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_TABS: &str = r#"{
        "id": "4989c913-0000-0000-0000-000000000000",
        "project_id": "proj-1",
        "label": "Claude 1",
        "clone_path": null,
        "last_known_status": "Suspended",
        "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
        "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
        "merged": false,
        "drawer_tab_names": ["Serve", "Vite"],
        "drawer_active_tab": 1
    }"#;

    /// `state.json` written before DEV-445 carries names only. Those tabs come
    /// back as bare shells — all the file can tell us — rather than failing to
    /// load.
    #[test]
    fn pre_dev_445_state_rehydrates_tabs_without_commands() {
        let session: PersistedSession = serde_json::from_str(LEGACY_TABS).unwrap();
        let tabs = session.drawer_tabs();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].name, "Serve");
        assert!(
            tabs.iter().all(|t| t.replay.is_none()),
            "a legacy file has no commands to give"
        );
    }

    /// The richer field wins when present. Reading `drawer_tab_names` first
    /// would silently downgrade every restored drawer to a shell prompt.
    #[test]
    fn drawer_commands_survive_a_round_trip() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Claude 1",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": ["Serve"],
            "drawer_tabs": [{ "name": "Serve", "replay": "php artisan serve --port={{unique_port}}" }],
            "drawer_active_tab": 0
        }"#;
        let session: PersistedSession = serde_json::from_str(json).unwrap();
        let tabs = session.drawer_tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            tabs[0].replay.as_deref(),
            Some("php artisan serve --port={{unique_port}}"),
            "the command is stored unsubstituted so unpark re-resolves the port"
        );

        let round_tripped: PersistedSession =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(round_tripped.drawer_tabs(), session.drawer_tabs());
    }

    /// The legacy key keeps being written so an older Allele reading a newer
    /// file still restores the tab layout, exactly as `skip_orchestration` does
    /// for orchestration.
    #[test]
    fn legacy_tab_names_are_still_written_for_older_allele() {
        let session: PersistedSession = serde_json::from_str(LEGACY_TABS).unwrap();
        let encoded = serde_json::to_value(&session).unwrap();
        assert_eq!(
            encoded["drawer_tab_names"],
            serde_json::json!(["Serve", "Vite"])
        );
    }

    /// `state.json` written before DEV-400 has no `skip_orchestration` key.
    /// It must still load, defaulting to "run the project's orchestration" —
    /// the behaviour every pre-existing session was created with.
    #[test]
    fn persisted_session_loads_without_skip_orchestration() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Claude 1",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": [],
            "drawer_active_tab": 0
        }"#;
        let session: PersistedSession =
            serde_json::from_str(json).expect("legacy state.json must still deserialise");
        assert!(!session.skip_orchestration);
        assert_eq!(session.label, "Claude 1");
    }

    /// A DEV-400 session that opted out predates the `orchestration` key. It
    /// must resolve to `Nothing`, not to the enum's `Full` default — otherwise
    /// upgrading Allele silently re-arms the project's startup scripts on every
    /// lightweight session the user has.
    #[test]
    fn legacy_skip_orchestration_resolves_to_nothing() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Claude 1",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": [],
            "drawer_active_tab": 0,
            "skip_orchestration": true
        }"#;
        let session: PersistedSession = serde_json::from_str(json).expect("parses");
        assert_eq!(session.orchestration, None, "legacy files carry no enum");
        assert_eq!(
            session.orchestration(),
            crate::session::Orchestration::Nothing
        );
    }

    /// The other legacy case: no opt-out means the full project setup.
    #[test]
    fn legacy_without_skip_orchestration_resolves_to_full() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Claude 1",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": [],
            "drawer_active_tab": 0
        }"#;
        let session: PersistedSession = serde_json::from_str(json).expect("parses");
        assert_eq!(session.orchestration(), crate::session::Orchestration::Full);
    }

    /// The new middle state has no legacy representation, so it must survive a
    /// round-trip on the enum — and must keep writing a legacy bool that an
    /// older Allele reads as "run the project's setup", the safe direction.
    #[test]
    fn startup_only_round_trips_and_stays_readable_by_older_allele() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Claude 1",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": [],
            "drawer_active_tab": 0,
            "skip_orchestration": false,
            "orchestration": "startup_only"
        }"#;
        let session: PersistedSession = serde_json::from_str(json).expect("parses");
        assert_eq!(
            session.orchestration(),
            crate::session::Orchestration::StartupOnly
        );

        let encoded = serde_json::to_string(&session).expect("serialises");
        let back: PersistedSession = serde_json::from_str(&encoded).expect("re-parses");
        assert_eq!(
            back.orchestration(),
            crate::session::Orchestration::StartupOnly
        );
        assert!(
            !back.skip_orchestration,
            "an older Allele must read StartupOnly as running the project's setup"
        );
    }

    /// And when the enum *is* present it wins over a stale legacy bool.
    #[test]
    fn explicit_orchestration_overrides_legacy_bool() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Claude 1",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": [],
            "drawer_active_tab": 0,
            "skip_orchestration": true,
            "orchestration": "full"
        }"#;
        let session: PersistedSession = serde_json::from_str(json).expect("parses");
        assert_eq!(session.orchestration(), crate::session::Orchestration::Full);
    }

    /// And a session that opted out round-trips through the file format.
    #[test]
    fn persisted_session_round_trips_skip_orchestration() {
        let json = r#"{
            "id": "4989c913-0000-0000-0000-000000000000",
            "project_id": "proj-1",
            "label": "Quick question",
            "clone_path": null,
            "last_known_status": "Suspended",
            "started_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "last_active": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "merged": false,
            "drawer_tab_names": [],
            "drawer_active_tab": 0,
            "skip_orchestration": true
        }"#;
        let session: PersistedSession = serde_json::from_str(json).expect("parses");
        assert!(session.skip_orchestration);

        let encoded = serde_json::to_string(&session).expect("serialises");
        let back: PersistedSession = serde_json::from_str(&encoded).expect("re-parses");
        assert!(back.skip_orchestration);
    }
}
