use crate::session::Session;
use crate::settings::ProjectSettings;
use crate::state::ArchivedSession;
use std::path::PathBuf;
use uuid::Uuid;

/// A session that's mid-clone — shown in the sidebar with a "Cloning..." label.
pub struct LoadingSession {
    pub id: String,
    pub label: String,
    /// Operation-specific progress text rendered beside the originating row.
    pub status: String,
}

/// A project is a source directory that hosts zero or more sessions.
/// Each session runs in an APFS clone of the source, stored under
/// `~/.allele/workspaces/<project-name>/<session-id>/`.
pub struct Project {
    pub id: String,
    pub name: String,
    pub source_path: PathBuf,
    pub sessions: Vec<Session>,
    pub loading_sessions: Vec<LoadingSession>,
    /// Archived session metadata — populated from state.json at startup
    /// and updated on merge/delete actions. The corresponding git refs
    /// live in canonical as `refs/allele/archive/<session-id>`.
    pub archives: Vec<ArchivedSession>,
    /// Per-project settings (merge strategy, default branch, etc.).
    pub settings: ProjectSettings,
}

impl Project {
    pub fn new(name: String, source_path: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            source_path,
            sessions: Vec::new(),
            loading_sessions: Vec::new(),
            archives: Vec::new(),
            settings: ProjectSettings::default(),
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

/// Resolve which project the no-session overview pane should describe.
///
/// `remembered` is the id of the project the user was last in — captured
/// while a session was still selected, because the overview only renders
/// once the active cursor has been cleared (DEV-310). Ids rather than
/// indices, so removing or reordering projects can't silently point the
/// overview at a different one.
///
/// Falls back to the first project when there is nothing remembered or the
/// remembered project has since been closed. Returns `None` only when no
/// projects are open at all.
pub fn resolve_overview_project(projects: &[Project], remembered: Option<&str>) -> Option<usize> {
    if let Some(id) = remembered {
        if let Some(idx) = projects.iter().position(|project| project.id == id) {
            return Some(idx);
        }
    }
    (!projects.is_empty()).then_some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str) -> Project {
        let mut project = Project::new(id.to_string(), PathBuf::from("/tmp").join(id));
        project.id = id.to_string();
        project
    }

    #[test]
    fn resolves_the_remembered_project() {
        let projects = [project("a"), project("b"), project("c")];
        assert_eq!(resolve_overview_project(&projects, Some("b")), Some(1));
    }

    #[test]
    fn falls_back_to_first_when_remembered_project_closed() {
        let projects = [project("a"), project("b")];
        assert_eq!(resolve_overview_project(&projects, Some("gone")), Some(0));
    }

    #[test]
    fn falls_back_to_first_when_nothing_remembered() {
        let projects = [project("a"), project("b")];
        assert_eq!(resolve_overview_project(&projects, None), Some(0));
    }

    #[test]
    fn resolves_nothing_without_projects() {
        assert_eq!(resolve_overview_project(&[], Some("a")), None);
        assert_eq!(resolve_overview_project(&[], None), None);
    }

    #[test]
    fn survives_projects_being_reordered() {
        // The bug this guards: an index captured before a reorder points at
        // whatever moved into that slot. An id follows the project.
        let projects = [project("b"), project("a"), project("c")];
        assert_eq!(resolve_overview_project(&projects, Some("a")), Some(1));
    }
}
