//! Turning control-socket requests into answers (DEV-415).
//!
//! Runs on the GPUI foreground thread with `&mut AppState` and a `&mut Window`
//! — the latter because session creation needs one.
//!
//! **The contract is non-interactive.** Nothing here may raise UI. Allele's
//! normal creation path has two interactive failure modes — a missing source
//! path opens a Relocate modal, a dirty worktree prompts — and both must
//! become error codes here instead. A modal nobody is watching is exactly the
//! hang this whole surface exists to avoid.

use gpui::{Context, Window};

use crate::app_state::AppState;
use crate::dispatch::protocol::{
    ErrorCode, ProjectSummary, Request, Response, SessionState, SessionSummary,
};
use crate::session::Session;

pub fn handle(
    request: Request,
    state: &mut AppState,
    _window: &mut Window,
    _cx: &mut Context<AppState>,
) -> Response {
    match request {
        Request::ProjectsList => projects_list(state),
        Request::SessionsList => sessions_list(state),
        Request::SessionsStatus { session_id } => sessions_status(state, &session_id),
        // Handled before this point — creation is asynchronous and answers
        // on the socket's reply channel directly. See `dispatch::create`.
        Request::SessionsCreate(_) => Response::Error {
            code: ErrorCode::Internal,
            message: "sessions.create must be routed to dispatch::create".to_string(),
        },
    }
}

fn projects_list(state: &AppState) -> Response {
    Response::Projects {
        projects: state
            .projects
            .iter()
            .map(|p| ProjectSummary {
                id: p.id.clone(),
                name: p.name.clone(),
                source_path: p.source_path.to_string_lossy().to_string(),
                session_count: p.sessions.len(),
            })
            .collect(),
    }
}

fn sessions_list(state: &AppState) -> Response {
    Response::Sessions {
        sessions: state
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter().map(|s| summarise(s, &p.name)))
            .collect(),
    }
}

fn sessions_status(state: &AppState, session_id: &str) -> Response {
    for project in state.projects.iter() {
        if let Some(session) = project.sessions.iter().find(|s| s.id == session_id) {
            return Response::Status {
                session: summarise(session, &project.name),
            };
        }
    }
    Response::Error {
        code: ErrorCode::BadRequest,
        message: format!("no session with id {session_id}"),
    }
}

fn summarise(session: &Session, project: &str) -> SessionSummary {
    SessionSummary {
        session_id: session.id.clone(),
        name: session.label.clone(),
        project: project.to_string(),
        state: SessionState::from(session.status),
        state_age_secs: session
            .status_since
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or(0),
        dispatched: session.origin.is_dispatched(),
        depth: session.origin.depth(),
    }
}
