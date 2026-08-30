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
use crate::dispatch::address;
use crate::dispatch::protocol::{
    DiscardedSession, ErrorCode, ProjectSummary, Request, Response, SessionState, SessionSummary,
};
use crate::session::Session;

pub fn handle(
    request: Request,
    state: &mut AppState,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> Response {
    match request {
        Request::ProjectsList => projects_list(state),
        Request::SessionsList => sessions_list(state, cx),
        Request::SessionsStatus { session_id } => sessions_status(state, &session_id, cx),
        Request::SessionsDiscard { session_id } => sessions_discard(state, &session_id, window, cx),
        // Handled before this point — these wait on the session to react,
        // which takes seconds, so they answer on the socket's reply channel
        // directly rather than blocking the drain. See `dispatch::create`
        // and `dispatch::manage`.
        Request::SessionsCreate(_) | Request::SessionsInterrupt { .. } => Response::Error {
            code: ErrorCode::Internal,
            message: "this op must be routed to dispatch::create or dispatch::manage".to_string(),
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

fn sessions_list(state: &AppState, cx: &gpui::App) -> Response {
    Response::Sessions {
        sessions: state
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter().map(|s| summarise(s, &p.name, cx)))
            .collect(),
    }
}

fn sessions_status(state: &AppState, session_id: &str, cx: &gpui::App) -> Response {
    for project in state.projects.iter() {
        if let Some(session) = project.sessions.iter().find(|s| s.id == session_id) {
            return Response::Status {
                session: summarise(session, &project.name, cx),
            };
        }
    }
    Response::Error {
        code: ErrorCode::BadRequest,
        message: format!("no session with id {session_id}"),
    }
}

/// Remove a dispatched session, archiving its work (DEV-429).
///
/// Bypasses the sidebar's inline Confirm/Cancel gate deliberately: that is a
/// UI affordance, and a confirmation nobody can answer is the hang this whole
/// surface exists to avoid.
///
/// Non-destructive. `remove_session` auto-commits any uncommitted work and
/// fetches the session branch into `refs/allele/archive/<id>` before deleting
/// the clone, and keeps a labelled row in the archive browser. That is what
/// makes this safe to expose to a caller that is not a person.
fn sessions_discard(
    state: &mut AppState,
    session_id: &str,
    window: &mut Window,
    cx: &mut Context<AppState>,
) -> Response {
    let found = state.projects.iter().enumerate().find_map(|(p_idx, p)| {
        p.sessions
            .iter()
            .position(|s| s.id == session_id)
            .map(|s_idx| (p_idx, s_idx))
    });
    let Some((project_idx, session_idx)) = found else {
        return Response::Error {
            code: ErrorCode::BadRequest,
            message: format!("no session with id {session_id}"),
        };
    };

    let session = &state.projects[project_idx].sessions[session_idx];

    // A caller may clean up what agents created. A human's session is not the
    // agent's to delete, and a confused orchestrator must not be able to wipe
    // work someone is in the middle of.
    if !session.origin.is_dispatched() {
        return Response::Error {
            code: ErrorCode::NotDispatched,
            message: format!(
                "session {:?} was started by a human, not dispatched — \
                 discard it from the sidebar",
                session.label
            ),
        };
    }

    let discarded = DiscardedSession {
        session_id: session.id.clone(),
        name: session.label.clone(),
        project: state.projects[project_idx].name.clone(),
        was_state: SessionState::from(session.status),
    };

    state.remove_session(
        crate::actions::SessionCursor {
            project_idx,
            session_idx,
        },
        window,
        cx,
    );

    Response::Discarded { session: discarded }
}

fn summarise(session: &Session, project: &str, cx: &gpui::App) -> SessionSummary {
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
        // Resolved per call, never cached: `/clear` and a cold resume both
        // replace the agent process, and a remembered address would then name
        // a socket that is gone. See DEV-440.
        reply_to: address::for_session(session, cx),
    }
}
