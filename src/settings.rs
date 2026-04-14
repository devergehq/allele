use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}

fn default_sidebar_width() -> f32 { 240.0 }
fn default_font_size() -> f32 { 13.0 }
fn default_drawer_height() -> f32 { 200.0 }
fn default_right_sidebar_width() -> f32 { 300.0 }
fn default_true() -> bool { true }

/// Built-in macOS sound for AwaitingInput. Used when the user hasn't set
/// a custom path in settings.json.
pub const DEFAULT_AWAITING_INPUT_SOUND: &str = "/System/Library/Sounds/Hero.aiff";
/// Built-in macOS sound for ResponseReady. Used when the user hasn't set
/// a custom path in settings.json.
pub const DEFAULT_RESPONSE_READY_SOUND: &str = "/System/Library/Sounds/Glass.aiff";

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
        let Some(path) = Self::path() else { return Self::default(); };
        let Ok(contents) = std::fs::read_to_string(&path) else { return Self::default(); };
        serde_json::from_str(&contents).unwrap_or_default()
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
