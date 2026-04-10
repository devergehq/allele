use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSave {
    pub id: String,
    pub name: String,
    pub source_path: PathBuf,
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
}

fn default_sidebar_width() -> f32 { 240.0 }
fn default_font_size() -> f32 { 13.0 }

impl Default for Settings {
    fn default() -> Self {
        Self {
            sidebar_width: default_sidebar_width(),
            font_size: default_font_size(),
            window_x: None,
            window_y: None,
            window_width: None,
            window_height: None,
            projects: Vec::new(),
        }
    }
}

impl Settings {
    /// Path to the settings file.
    pub fn path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        Some(home.join(".config").join("cc-multiplex").join("settings.json"))
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
