use crate::session::Session;
use std::path::PathBuf;
use uuid::Uuid;

/// A session that's mid-clone — shown in the sidebar with a "Cloning..." label.
pub struct LoadingSession {
    pub id: String,
    pub label: String,
}

/// A project is a source directory that hosts zero or more sessions.
/// Each session runs in an APFS clone of the source, stored under
/// `~/.cc-multiplex/workspaces/<project-name>/<session-id>/`.
pub struct Project {
    pub id: String,
    pub name: String,
    pub source_path: PathBuf,
    pub sessions: Vec<Session>,
    pub loading_sessions: Vec<LoadingSession>,
    pub expanded: bool,
}

impl Project {
    pub fn new(name: String, source_path: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            source_path,
            sessions: Vec::new(),
            loading_sessions: Vec::new(),
            expanded: true,
        }
    }

    /// Derive a display name from the source path basename.
    pub fn name_from_path(path: &std::path::Path) -> String {
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    }
}
