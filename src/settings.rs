use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which built-in adapter drives an agent's command building. `Generic`
/// is used for custom user-added entries that just run a binary with the
/// configured extra args.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Opencode,
    Generic,
}

impl Default for AgentKind {
    fn default() -> Self { AgentKind::Generic }
}

/// One entry in the user's configured coding-agent list. Paths are
/// optional so an entry can represent "agent type I know about, but
/// not installed on this machine" — the settings UI shows an empty
/// override slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Stable id — referenced by `Session.agent_id` and by
    /// `allele.json`'s `"agent"` override field.
    pub id: String,
    #[serde(default)]
    pub kind: AgentKind,
    #[serde(default)]
    pub display_name: String,
    /// Absolute path to the binary. `None` means "not detected" for
    /// built-in kinds, or "not yet configured" for generic entries.
    #[serde(default)]
    pub path: Option<String>,
    /// Extra command-line arguments appended to the ones the adapter
    /// builds (e.g. `--dangerously-skip-permissions` for Claude).
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// How session work gets integrated back into the canonical branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    /// `git merge --no-ff --no-edit` — preserves merge commit (default).
    Merge,
    /// `git merge --squash` + explicit commit — collapses session into one commit.
    Squash,
    /// Rebase session commits onto target branch, then fast-forward merge — linear history.
    RebaseThenMerge,
}

impl Default for MergeStrategy {
    fn default() -> Self {
        Self::Merge
    }
}

/// Per-project settings that govern clone, merge, and sync behaviour.
///
/// Every field has a serde default matching the pre-settings-era behaviour,
/// so existing `settings.json` files deserialise without error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSettings {
    /// Override the auto-detected default branch (e.g. `"main"`, `"develop"`).
    /// `None` = auto-detect from `refs/remotes/<remote>/HEAD`, fallback `"master"`.
    #[serde(default)]
    pub default_branch: Option<String>,

    /// How session work gets integrated into canonical.
    #[serde(default)]
    pub merge_strategy: MergeStrategy,

    /// Fetch + rebase canonical onto the remote tip before merging session work.
    /// This syncs with upstream — orthogonal to `merge_strategy`.
    #[serde(default = "default_true")]
    pub rebase_before_merge: bool,

    /// Remote name for fetch/rebase operations. `None` = `"origin"`.
    #[serde(default)]
    pub remote: Option<String>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            default_branch: None,
            merge_strategy: MergeStrategy::default(),
            rebase_before_merge: true,
            remote: None,
        }
    }
}

impl ProjectSettings {
    /// Resolved remote name — returns the override or `"origin"`.
    pub fn resolved_remote(&self) -> &str {
        self.remote.as_deref().unwrap_or("origin")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSave {
    pub id: String,
    pub name: String,
    pub source_path: PathBuf,
    #[serde(default)]
    pub settings: ProjectSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default)]
    pub window_x: Option<f32>,
    #[serde(default)]
    pub window_y: Option<f32>,
    #[serde(default)]
    pub window_width: Option<f32>,
    #[serde(default)]
    pub window_height: Option<f32>,
    #[serde(default)]
    pub projects: Vec<ProjectSave>,

    // --- sidebar visibility -----------------------------------------------------
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,

    // --- drawer terminal ------------------------------------------------------
    #[serde(default = "default_drawer_height")]
    pub drawer_height: f32,
    #[serde(default)]
    pub drawer_visible: bool,

    // --- right sidebar --------------------------------------------------------
    #[serde(default)]
    pub right_sidebar_visible: bool,
    #[serde(default = "default_right_sidebar_width")]
    pub right_sidebar_width: f32,

    // --- attention routing -------------------------------------------------
    /// Play a sound when a session transitions to AwaitingInput.
    /// Default: ON (Patrick's primary attention channel).
    #[serde(default = "default_true")]
    pub sound_on_awaiting_input: bool,
    /// Play a sound when a session transitions to ResponseReady.
    /// Default: OFF (fires every response turn — too noisy by default).
    #[serde(default)]
    pub sound_on_response_ready: bool,
    /// Fire a macOS notification on attention events.
    /// Default: OFF (Patrick prefers sound over visual notifications).
    #[serde(default)]
    pub notifications_enabled: bool,
    /// Override the default AwaitingInput sound path.
    /// Default: `/System/Library/Sounds/Hero.aiff`.
    #[serde(default)]
    pub awaiting_input_sound_path: Option<String>,
    /// Override the default ResponseReady sound path.
    /// Default: `/System/Library/Sounds/Glass.aiff`.
    #[serde(default)]
    pub response_ready_sound_path: Option<String>,

    // --- editor --------------------------------------------------------------
    /// Command used for the Editor tab's "Open in External Editor" context
    /// menu. `None` falls back to `DEFAULT_EXTERNAL_EDITOR` (Sublime Text's
    /// `subl` CLI). The command is invoked as `<cmd> <path>` via the shell's
    /// PATH — either a bare binary name on PATH or a full executable path.
    #[serde(default)]
    pub external_editor_command: Option<String>,

    // --- session cleanup -----------------------------------------------------
    /// Paths (relative to the session clone root) to delete immediately after
    /// a clone is created. Catches stale runtime artifacts the parent left in
    /// the source tree — Overmind/Foreman sockets, server pid files, etc. —
    /// that would otherwise trip the child session when it tries to start
    /// those same processes. Users can add or remove entries via the Settings
    /// window.
    #[serde(default = "default_session_cleanup_paths")]
    pub session_cleanup_paths: Vec<String>,

    // --- browser integration -------------------------------------------------
    /// When true, Allele manages one tab in the user's running Google Chrome
    /// per session. Session switches activate the matching tab; session
    /// create/resume navigates the tab to the project's preview URL. Off by
    /// default because it talks to the user's real Chrome via AppleScript
    /// (Automation permission prompt on first use).
    #[serde(default)]
    pub browser_integration_enabled: bool,

    // --- coding agents -------------------------------------------------------
    /// Configured coding agents. Empty on legacy settings files — seeded
    /// via `ensure_agents_seeded` on first load.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Id of the globally-default agent. `allele.json` can override this
    /// per project via its `"agent"` field.
    #[serde(default)]
    pub default_agent: Option<String>,

    /// When true, run `git pull` on each project's source root before
    /// creating a new session clone. Failures are logged but do not abort
    /// the session — the clone proceeds against whatever is on disk.
    #[serde(default)]
    pub git_pull_before_new_session: bool,
}

fn default_sidebar_width() -> f32 { 240.0 }
fn default_font_size() -> f32 { 13.0 }
fn default_drawer_height() -> f32 { 200.0 }
fn default_right_sidebar_width() -> f32 { 300.0 }
fn default_true() -> bool { true }

/// Default list of stale runtime files to purge from a fresh session clone.
/// `.overmind.sock` is the canonical case — Overmind refuses to start when a
/// socket from the parent clone sticks around. `.foreman.sock` is the Foreman
/// equivalent; `tmp/pids/server.pid` is the Rails/Puma pid file that makes
/// `rails s` bail with "a server is already running".
fn default_session_cleanup_paths() -> Vec<String> {
    vec![
        ".overmind.sock".to_string(),
        ".foreman.sock".to_string(),
        "tmp/pids/server.pid".to_string(),
    ]
}

/// Built-in macOS sound for AwaitingInput. Used when the user hasn't set
/// a custom path in settings.json.
pub const DEFAULT_AWAITING_INPUT_SOUND: &str = "/System/Library/Sounds/Hero.aiff";
/// Built-in macOS sound for ResponseReady. Used when the user hasn't set
/// a custom path in settings.json.
pub const DEFAULT_RESPONSE_READY_SOUND: &str = "/System/Library/Sounds/Glass.aiff";

/// Default CLI used by "Open in External Editor". Sublime Text ships `subl`
/// on PATH when the user has installed the CLI helper.
pub const DEFAULT_EXTERNAL_EDITOR: &str = "subl";

impl Default for Settings {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            sidebar_visible: true,
            font_size: default_font_size(),
            window_x: None,
            window_y: None,
            window_width: None,
            window_height: None,
            projects: Vec::new(),
            drawer_height: default_drawer_height(),
            drawer_visible: false,
            right_sidebar_visible: false,
            right_sidebar_width: default_right_sidebar_width(),
            sound_on_awaiting_input: true,
            sound_on_response_ready: false,
            notifications_enabled: false,
            awaiting_input_sound_path: None,
            response_ready_sound_path: None,
            session_cleanup_paths: default_session_cleanup_paths(),
            external_editor_command: None,
            browser_integration_enabled: false,
            agents: Vec::new(),
            default_agent: None,
            git_pull_before_new_session: false,
        }
    }
}

impl Settings {
    /// Path to the settings file.
    pub fn path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        Some(home.join(".config").join("allele").join("settings.json"))
    }

    /// Load settings from disk. Returns default if file doesn't exist or is invalid.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            let mut s = Self::default();
            s.ensure_agents_seeded();
            return s;
        };
        let mut s: Self = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        s.ensure_agents_seeded();
        s
    }

    /// Populate the agents list (and a default id) on a fresh install or a
    /// legacy settings file that predates the feature. Runs filesystem
    /// probes via the `agents` module. Idempotent — skipped when the list
    /// already has entries.
    pub fn ensure_agents_seeded(&mut self) {
        if self.agents.is_empty() {
            self.agents = crate::agents::seed_agents();
        }
        if self.default_agent.is_none() {
            self.default_agent = self
                .agents
                .iter()
                .find(|a| a.enabled && a.path.is_some())
                .or_else(|| self.agents.first())
                .map(|a| a.id.clone());
        }
    }

    /// Save settings to disk. Silently fails on error (not critical).
    pub fn save(&self) {
        let Some(path) = Self::path() else { return; };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_includes_overmind_sock() {
        let s = Settings::default();
        assert!(s.session_cleanup_paths.iter().any(|p| p == ".overmind.sock"));
    }

    #[test]
    fn legacy_settings_file_without_cleanup_paths_gets_defaults() {
        // A settings.json saved before this field existed — deserialization
        // must fill session_cleanup_paths via serde(default) rather than
        // failing or leaving it empty.
        let legacy = r#"{ "sidebar_width": 240.0, "font_size": 13.0 }"#;
        let s: Settings = serde_json::from_str(legacy).expect("should deserialize");
        assert_eq!(s.session_cleanup_paths, default_session_cleanup_paths());
    }

    #[test]
    fn explicit_empty_cleanup_list_is_preserved() {
        // If a user deletes every entry, we must respect that — don't silently
        // re-seed defaults on load.
        let json = r#"{ "session_cleanup_paths": [] }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert!(s.session_cleanup_paths.is_empty());
    }
}
