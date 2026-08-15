//! `sessions.create` — the part that actually starts a session (DEV-415).
//!
//! Everything else on the control socket answers from state already in hand.
//! This provisions a workspace, launches an agent and waits to see the prompt
//! land, so it is the only operation that cannot be answered synchronously.
//!
//! ## Why the reply comes back late
//!
//! Validation and launch run on the GPUI foreground thread; the wait for the
//! prompt cannot, because it takes seconds and would freeze the UI. So the
//! whole operation runs as a detached task holding the socket's reply channel,
//! and the socket thread — already blocked on `recv_timeout` — simply gets its
//! answer when there is one. The drain loop is never held up.
//!
//! ## Success means the prompt landed, not that the PTY was written to
//!
//! Allele delivers an initial prompt as a bracketed paste followed by a bare
//! carriage return on an 80ms timer, racing the agent's TUI boot. That has
//! only ever been exercised one session at a time by a human. Twenty
//! simultaneous dispatches on a loaded machine is a different test, and the
//! failure mode is a session that sits there having consumed nothing.
//!
//! So success is reported only once allele *observes* the prompt being
//! submitted — via the same hook events that drive the sidebar. On timeout the
//! session is deliberately **left alive and visible**: a human can see an idle
//! session and recover it, whereas a phantom success reported to an
//! orchestrator is unrecoverable, because nothing ever looks at it again.

use std::sync::mpsc::Sender;
use std::time::Duration;

use gpui::{AsyncApp, WeakEntity};

use crate::app_state::AppState;
use crate::dispatch::admission;
use crate::dispatch::protocol::{CreateRequest, CreatedSession, ErrorCode, Response, SessionState};
use crate::dispatch::update_in_main_window;
use crate::session::{SessionOrigin, SessionStatus};
use crate::{agents, config, git};

/// How long to wait for the agent to consume its prompt before giving up on
/// confirming it. Generous: a cold agent on a busy machine can take a while to
/// draw its first frame, and a false `prompt_delivery_unconfirmed` would send
/// an orchestrator chasing a session that is fine.
const PROMPT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Run a create request to completion and answer on `reply`.
pub(crate) fn spawn(
    request: CreateRequest,
    reply: Sender<Response>,
    this: WeakEntity<AppState>,
    cx: &mut AsyncApp,
) {
    cx.spawn(async move |cx| {
        let response = run(request, &this, cx).await;
        // A client that hung up is normal, not an error.
        let _ = reply.send(response);
    })
    .detach();
}

async fn run(request: CreateRequest, this: &WeakEntity<AppState>, cx: &mut AsyncApp) -> Response {
    let started = match update_in_main_window(this, cx, |state, window, cx| {
        begin(&request, state, window, cx)
    }) {
        Ok(inner) => inner,
        Err(e) => return error(ErrorCode::Internal, e.to_string()),
    };

    let started = match started {
        Ok(s) => s,
        Err((code, message)) => return error(code, message),
    };

    // Wait for allele to observe the prompt being submitted.
    let deadline = PROMPT_CONFIRM_TIMEOUT.as_millis() / POLL_INTERVAL.as_millis();
    for _ in 0..deadline {
        cx.background_executor().timer(POLL_INTERVAL).await;

        let state = update_in_main_window(this, cx, |state, _window, _cx| {
            lookup_status(state, &started.session_id)
        });
        match state {
            Ok(Some(status)) if consumed_prompt(status) => {
                return Response::Created {
                    session: CreatedSession {
                        session_id: started.session_id,
                        name: started.name,
                        project: started.project,
                        state: SessionState::from(status),
                    },
                };
            }
            // The session vanished — closed by a human mid-create, most
            // likely. Report rather than spin to the deadline.
            Ok(None) => {
                return error(
                    ErrorCode::Internal,
                    "session disappeared before its prompt was confirmed".to_string(),
                )
            }
            Ok(Some(_)) => continue,
            Err(e) => return error(ErrorCode::Internal, e.to_string()),
        }
    }

    error(
        ErrorCode::PromptDeliveryUnconfirmed,
        format!(
            "session {} started as \"{}\" but allele did not observe its prompt \
             within {}s — it is alive and visible in the sidebar",
            started.session_id,
            started.name,
            PROMPT_CONFIRM_TIMEOUT.as_secs()
        ),
    )
}

struct Started {
    session_id: String,
    name: String,
    project: String,
}

/// Validate and launch, on the foreground thread.
///
/// Every failure here is a code. Allele's interactive creation paths — the
/// Relocate modal on a missing source path, the confirmation on a dirty
/// worktree — are pre-empted by checking the same conditions first, because a
/// modal raised for a caller that is not a person is a hang.
fn begin(
    request: &CreateRequest,
    state: &mut AppState,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<AppState>,
) -> Result<Started, (ErrorCode, String)> {
    let project_idx = state
        .projects
        .iter()
        .position(|p| p.name == request.project || p.id == request.project)
        .ok_or_else(|| {
            (
                ErrorCode::ProjectNotRegistered,
                format!("no project named or identified by {:?}", request.project),
            )
        })?;

    let source_path = state.projects[project_idx].source_path.clone();
    if !source_path.exists() {
        return Err((
            ErrorCode::SourcePathMissing,
            format!("project source {} no longer exists", source_path.display()),
        ));
    }

    // The UI asks the user whether to proceed with a dirty tree. Dispatch
    // refuses instead: the alternative is silently copying someone's
    // uncommitted work into twenty clones.
    if git::is_working_tree_dirty(&source_path) {
        return Err((
            ErrorCode::WorktreeDirty,
            format!(
                "{} has uncommitted changes; commit or stash before dispatching",
                source_path.display()
            ),
        ));
    }

    // An agent must actually be resolvable, or the session opens on a bare
    // shell and silently never answers anyone.
    let project_override = config::ProjectConfig::load(&source_path).and_then(|c| c.agent);
    if agents::resolve(
        &state.user_settings.agents,
        state.user_settings.default_agent.as_deref(),
        project_override.as_deref(),
        None,
    )
    .is_none()
    {
        return Err((
            ErrorCode::AgentNotConfigured,
            "no enabled agent with a resolvable binary".to_string(),
        ));
    }

    // Depth comes from allele's own record of the calling session, not from
    // the request. An unknown caller is depth 0 — see `caller_session_id`.
    let caller_origin = request
        .caller_session_id
        .as_deref()
        .and_then(|id| find_session_origin(state, id))
        .unwrap_or(SessionOrigin::Human);

    let depth = admission::admit(&caller_origin, admission::live_dispatched_count(state)).map_err(
        |code| {
            let message = match code {
                ErrorCode::DepthLimitExceeded => format!(
                    "dispatched sessions may not dispatch (depth limit {})",
                    admission::MAX_DISPATCH_DEPTH
                ),
                _ => format!(
                    "{} dispatched sessions already running (limit {})",
                    admission::live_dispatched_count(state),
                    admission::MAX_DISPATCHED_SESSIONS
                ),
            };
            (code, message)
        },
    )?;

    let name = unique_name(state, request.name.trim());
    let by_label = request
        .caller_session_id
        .as_deref()
        .and_then(|id| find_session_label(state, id))
        .unwrap_or_else(|| "an agent".to_string());
    let origin = SessionOrigin::Dispatched {
        by_session_id: request.caller_session_id.clone().unwrap_or_default(),
        by_label,
        depth,
    };

    let before: Vec<String> = state.projects[project_idx]
        .sessions
        .iter()
        .map(|s| s.id.clone())
        .collect();

    state.add_session_to_project_with_details(
        project_idx,
        name.clone(),
        None,
        None,
        Some(request.prompt.clone()),
        request.orchestration,
        window,
        cx,
    );

    // Creation is asynchronous — the session appears once its clone lands, so
    // the id is claimed from the loading list rather than the session list.
    let project = &mut state.projects[project_idx];
    let session_id = project
        .loading_sessions
        .iter()
        .map(|l| l.id.clone())
        .find(|id| !before.contains(id))
        .or_else(|| {
            project
                .sessions
                .iter()
                .map(|s| s.id.clone())
                .find(|id| !before.contains(id))
        })
        .ok_or_else(|| {
            (
                ErrorCode::CloneFailed,
                "allele did not begin provisioning a workspace".to_string(),
            )
        })?;

    // Recorded against the loading session so attribution survives even if
    // provisioning fails and a human is left looking at the wreckage.
    state
        .pending_dispatch_origins
        .insert(session_id.clone(), origin);

    Ok(Started {
        session_id,
        name,
        project: state.projects[project_idx].name.clone(),
    })
}

/// A name no session has ever carried, live or archived.
///
/// Uniqueness across *time*, not across live sessions. `ListAgents` shows only
/// live ones, so a dead session's name is exactly the one that collides — and
/// because bare-name addressing is keyed on the string and resolves to the
/// newest holder, a reused name silently retargets an orchestrator that
/// amortised it. Allele persists every session it has created and is the only
/// component positioned to prevent that.
fn unique_name(state: &AppState, requested: &str) -> String {
    let mut taken = std::collections::HashSet::new();
    for project in state.projects.iter() {
        taken.extend(project.sessions.iter().map(|s| s.label.clone()));
        taken.extend(project.loading_sessions.iter().map(|l| l.label.clone()));
        taken.extend(project.archives.iter().map(|a| a.label.clone()));
    }
    unique_name_among(&taken, requested)
}

/// The pure half of [`unique_name`], so the rule can be tested without an app.
fn unique_name_among(taken: &std::collections::HashSet<String>, requested: &str) -> String {
    let requested = if requested.is_empty() {
        "Dispatched"
    } else {
        requested
    };
    if !taken.contains(requested) {
        return requested.to_string();
    }
    for n in 2..1000 {
        let candidate = format!("{requested} {n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("{requested} {}", uuid::Uuid::new_v4())
}

fn find_session_origin(state: &AppState, session_id: &str) -> Option<SessionOrigin> {
    state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id || s.claude_session_id() == session_id)
        .map(|s| s.origin.clone())
}

fn find_session_label(state: &AppState, session_id: &str) -> Option<String> {
    state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id || s.claude_session_id() == session_id)
        .map(|s| s.label.clone())
}

fn lookup_status(state: &AppState, session_id: &str) -> Option<SessionStatus> {
    if state
        .projects
        .iter()
        .any(|p| p.loading_sessions.iter().any(|l| l.id == session_id))
    {
        // Still cloning. Not a status yet, but not gone either.
        return Some(SessionStatus::Suspended);
    }
    state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .find(|s| s.id == session_id)
        .map(|s| s.status)
}

/// Whether a status implies the agent consumed its prompt.
///
/// `Running` is set from `user_prompt_submit` and the tool-use hooks, all of
/// which mean the prompt landed. The attention states are included because an
/// agent that has already reached a permission prompt or finished a turn
/// plainly read something — treating those as unconfirmed would report failure
/// for a session that is further along than the one we were waiting for.
fn consumed_prompt(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Running | SessionStatus::AwaitingInput | SessionStatus::ResponseReady
    )
}

fn error(code: ErrorCode, message: String) -> Response {
    Response::Error { code, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn taken(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn an_unused_name_is_returned_unchanged() {
        assert_eq!(unique_name_among(&taken(&[]), "Fix Auth"), "Fix Auth");
    }

    /// The point of the rule: an *archived* session's name must still be
    /// treated as taken. `ListAgents` shows only live sessions, so a dead
    /// session's name is exactly the one that collides — and bare-name
    /// addressing resolves to the newest holder, silently retargeting an
    /// orchestrator that amortised it.
    #[test]
    fn a_dead_sessions_name_is_still_taken() {
        assert_eq!(
            unique_name_among(&taken(&["Fix Auth"]), "Fix Auth"),
            "Fix Auth 2"
        );
    }

    #[test]
    fn suffixes_skip_past_existing_suffixes() {
        let t = taken(&["Fix Auth", "Fix Auth 2", "Fix Auth 3"]);
        assert_eq!(unique_name_among(&t, "Fix Auth"), "Fix Auth 4");
    }

    /// An empty name would otherwise produce a bare suffix like " 2", which is
    /// unaddressable and unreadable in a sidebar.
    #[test]
    fn an_empty_name_gets_a_usable_default() {
        assert_eq!(unique_name_among(&taken(&[]), ""), "Dispatched");
        assert_eq!(
            unique_name_among(&taken(&["Dispatched"]), ""),
            "Dispatched 2"
        );
    }

    /// Success means the agent read its prompt. `Idle` is what a session that
    /// started and consumed nothing looks like — reporting success there is
    /// the phantom this whole wait exists to prevent.
    #[test]
    fn idle_does_not_count_as_having_consumed_the_prompt() {
        assert!(!consumed_prompt(SessionStatus::Idle));
        assert!(!consumed_prompt(SessionStatus::Suspended));
        assert!(!consumed_prompt(SessionStatus::Done));
    }

    /// A session already blocked on a permission prompt, or already finished a
    /// turn, has plainly read something — treating those as unconfirmed would
    /// report failure for a session further along than the one we waited for.
    #[test]
    fn states_past_the_prompt_all_count() {
        assert!(consumed_prompt(SessionStatus::Running));
        assert!(consumed_prompt(SessionStatus::AwaitingInput));
        assert!(consumed_prompt(SessionStatus::ResponseReady));
    }
}
