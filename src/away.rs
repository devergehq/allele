//! One-click Away (DEV-1157): ask every live Claude session for Claude Code's
//! remote control, so they can be reached from a phone or another machine.
//!
//! Allele owns every PTY, so this is one keystroke sequence per session. The
//! hard part is *when* it is safe to type. Measured against a real `claude`
//! 2.1.316 (results recorded on DEV-1157):
//!
//! - **At the prompt** the command works.
//! - **Mid-turn** it works too, and the turn carries on.
//! - **At a permission prompt** the Enter picks the highlighted "Yes" and
//!   *approves the pending tool call*. Remote control is not turned on.
//! - Claude's `Notification` hook, which is what moves a session to
//!   `AwaitingInput`, fired **6 s after** the dialog appeared. So `Running`
//!   does not mean "no dialog is up", and a mid-turn send cannot be made safe
//!   from status alone.
//! - Sending it twice opens the Remote Control menu and leaves the session
//!   sitting in it.
//!
//! So Away types only where no turn is running and no dialog can be on
//! screen, queues everything else, and delivers on the first poll tick at
//! which the session is safe. It sends at most once per agent process.

use gpui::Context;
use tracing::info;

use crate::app_state::AppState;
use crate::session::{AttentionKind, RemoteControlState, Session, SessionStatus};
use crate::settings::AgentKind;

/// The command Away types. Claude Code's own; Allele adds nothing to it.
pub(crate) const COMMAND: &str = "/remote-control";

/// What Away does with one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Safe to type now.
    Send,
    /// A live Claude session that is not safe to type into yet.
    Queue,
    /// Already delivered to this agent process.
    AlreadySent,
    /// Not a Claude session: another agent, or a plain shell.
    NotClaude,
    /// No agent process to type into: suspended, ended, or still cloning.
    NotLive,
}

/// Decide what Away does with one session. Pure, so every branch is tested.
///
/// `live_pid` is the agent's child pid, `None` when there is no PTY.
pub(crate) fn decide(
    is_claude: bool,
    live_pid: Option<u32>,
    status: SessionStatus,
    attention: Option<AttentionKind>,
    rc: &RemoteControlState,
) -> Decision {
    if !is_claude {
        return Decision::NotClaude;
    }
    let Some(pid) = live_pid else {
        return Decision::NotLive;
    };
    if matches!(status, SessionStatus::Done | SessionStatus::Suspended) {
        return Decision::NotLive;
    }
    if rc.sent_to_pid == Some(pid) {
        return Decision::AlreadySent;
    }
    // Before this process's first hook, `Idle` is just the value a new
    // session starts with, and the agent may be on a startup dialog.
    // `SessionStart` fires only after those are dismissed (measured).
    if rc.heard_from_pid != Some(pid) {
        return Decision::Queue;
    }
    match status {
        SessionStatus::Idle | SessionStatus::ResponseReady => Decision::Send,
        // Only the sixty-second idle nudge is safe. A permission prompt or a
        // question is exactly where Enter does harm, and a notification with
        // no context is treated as blocking, as `absorbs_idle_nudge` does.
        SessionStatus::AwaitingInput => match attention {
            Some(AttentionKind::Idle) => Decision::Send,
            _ => Decision::Queue,
        },
        SessionStatus::Running => Decision::Queue,
        SessionStatus::Done | SessionStatus::Suspended => Decision::NotLive,
    }
}

/// Counts for the summary banner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub sent: usize,
    pub queued: usize,
    pub already: usize,
    pub skipped: usize,
}

impl Summary {
    fn count(&mut self, d: Decision) {
        match d {
            Decision::Send => self.sent += 1,
            Decision::Queue => self.queued += 1,
            Decision::AlreadySent => self.already += 1,
            Decision::NotClaude | Decision::NotLive => self.skipped += 1,
        }
    }

    /// The banner text. Says plainly what happens to queued sessions, because
    /// the user is about to walk away and will not see it happen.
    pub(crate) fn message(&self) -> String {
        if self.sent == 0 && self.queued == 0 && self.already == 0 {
            return "Away: no live Claude sessions to request remote control from".to_string();
        }
        let mut parts = vec![format!(
            "Remote control requested for {}",
            sessions(self.sent)
        )];
        if self.queued > 0 {
            parts.push(format!("{} busy, sent when idle", self.queued));
        }
        if self.already > 0 {
            parts.push(format!("{} already on", self.already));
        }
        if self.skipped > 0 {
            parts.push(format!("skipped {} (not live Claude)", self.skipped));
        }
        parts.join(" · ")
    }
}

fn sessions(n: usize) -> String {
    if n == 1 {
        "1 session".to_string()
    } else {
        format!("{n} sessions")
    }
}

/// Whether the session runs Claude Code. Strict on purpose: a session with
/// no recorded agent is skipped rather than assumed to be Claude, because a
/// Shell session has no agent either, and typing a slash command into zsh is
/// noise. (Elsewhere a missing agent defaults to Claude; that is right for
/// reading events, not for typing.)
fn is_claude(state: &AppState, session: &Session) -> bool {
    session
        .agent_id
        .as_deref()
        .and_then(|id| state.user_settings.agents.iter().find(|a| a.id == id))
        .is_some_and(|a| a.kind == AgentKind::Claude)
}

/// The agent's child pid, if the session has a live PTY. Only the session's
/// own terminal is consulted; drawer tabs are never typed into.
pub(crate) fn live_pid(session: &Session, cx: &Context<AppState>) -> Option<u32> {
    let tv = session.terminal_view.as_ref()?;
    let pty = tv.read(cx).pty()?;
    if pty.exited {
        return None;
    }
    pty.child_pid()
}

gpui::actions!(allele, [AwayAction]);

impl AppState {
    /// Away, from any entry point (menu, palette): send, queue, and say what
    /// happened in the notice banner.
    pub(crate) fn away_and_report(&mut self, cx: &mut Context<Self>) {
        let summary = self.go_away(cx);
        self.sync_notice = Some(summary.message());
        cx.notify();
    }

    /// Away: request remote control from every live Claude session.
    pub(crate) fn go_away(&mut self, cx: &mut Context<Self>) -> Summary {
        let mut summary = Summary::default();
        let plan = self.away_plan(cx);
        for (cursor, decision, pid) in plan {
            summary.count(decision);
            let Some(session) = self.session_mut_at(cursor) else {
                continue;
            };
            match decision {
                Decision::Send => {
                    session.remote_control.queued = false;
                    session.remote_control.sent_to_pid = pid;
                    if let Some(tv) = session.terminal_view.clone() {
                        crate::dispatch::pty::deliver(&tv, COMMAND.to_string(), &[], cx);
                    }
                }
                Decision::Queue => session.remote_control.queued = true,
                _ => session.remote_control.queued = false,
            }
        }
        info!("away: {summary:?}");
        summary
    }

    /// Whether any session is waiting for Away to deliver.
    pub(crate) fn away_pending(&self) -> bool {
        self.projects
            .iter()
            .flat_map(|p| &p.sessions)
            .any(|s| s.remote_control.queued)
    }

    /// Deliver to queued sessions that have become safe. Run on the hook
    /// poller's tick, after events are applied, so every path out of
    /// `Running` is covered, including interrupted turns that emit no hook.
    pub(crate) fn drain_away_queue(&mut self, cx: &mut Context<Self>) {
        if !self.away_pending() {
            return;
        }
        let plan = self.away_plan(cx);
        let mut delivered = 0usize;
        for (cursor, decision, pid) in plan {
            let Some(session) = self.session_mut_at(cursor) else {
                continue;
            };
            if !session.remote_control.queued {
                continue;
            }
            match decision {
                Decision::Queue => {}
                Decision::Send => {
                    session.remote_control.queued = false;
                    session.remote_control.sent_to_pid = pid;
                    if let Some(tv) = session.terminal_view.clone() {
                        crate::dispatch::pty::deliver(&tv, COMMAND.to_string(), &[], cx);
                        delivered += 1;
                    }
                }
                // Ended, suspended or already sent: nothing left to do.
                _ => session.remote_control.queued = false,
            }
        }
        if delivered > 0 {
            info!("away: delivered to {delivered} queued session(s)");
        }
    }

    /// Record that a hook event arrived from this session's current agent
    /// process. Called from `apply_hook_event`.
    pub(crate) fn note_hook_from(
        &mut self,
        cursor: crate::actions::SessionCursor,
        cx: &Context<Self>,
    ) {
        let pid = self
            .projects
            .get(cursor.project_idx)
            .and_then(|p| p.sessions.get(cursor.session_idx))
            .and_then(|s| live_pid(s, cx));
        if let Some(session) = self.session_mut_at(cursor) {
            if pid.is_some() {
                session.remote_control.heard_from_pid = pid;
            }
        }
    }

    fn away_plan(
        &self,
        cx: &Context<Self>,
    ) -> Vec<(crate::actions::SessionCursor, Decision, Option<u32>)> {
        let mut plan = Vec::new();
        for (p_idx, project) in self.projects.iter().enumerate() {
            for (s_idx, session) in project.sessions.iter().enumerate() {
                let pid = live_pid(session, cx);
                let decision = decide(
                    is_claude(self, session),
                    pid,
                    session.status,
                    session.attention_context.as_ref().map(|c| c.kind),
                    &session.remote_control,
                );
                plan.push((
                    crate::actions::SessionCursor {
                        project_idx: p_idx,
                        session_idx: s_idx,
                    },
                    decision,
                    pid,
                ));
            }
        }
        plan
    }

    fn session_mut_at(&mut self, cursor: crate::actions::SessionCursor) -> Option<&mut Session> {
        self.projects
            .get_mut(cursor.project_idx)
            .and_then(|p| p.sessions.get_mut(cursor.session_idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PID: u32 = 4242;

    fn heard() -> RemoteControlState {
        RemoteControlState {
            heard_from_pid: Some(PID),
            ..Default::default()
        }
    }

    fn d(status: SessionStatus, attention: Option<AttentionKind>) -> Decision {
        decide(true, Some(PID), status, attention, &heard())
    }

    #[test]
    fn idle_and_finished_sessions_are_sent_now() {
        assert_eq!(d(SessionStatus::Idle, None), Decision::Send);
        assert_eq!(d(SessionStatus::ResponseReady, None), Decision::Send);
    }

    /// The idle nudge is the one AwaitingInput with nothing on screen to
    /// answer: Claude has finished and is waiting for a prompt.
    #[test]
    fn the_idle_nudge_is_sent_now() {
        assert_eq!(
            d(SessionStatus::AwaitingInput, Some(AttentionKind::Idle)),
            Decision::Send
        );
    }

    /// The headline finding: Enter at a permission prompt approves the tool.
    #[test]
    fn a_permission_prompt_is_queued_never_typed_into() {
        assert_eq!(
            d(
                SessionStatus::AwaitingInput,
                Some(AttentionKind::Permission)
            ),
            Decision::Queue
        );
    }

    #[test]
    fn a_question_is_queued() {
        assert_eq!(
            d(SessionStatus::AwaitingInput, Some(AttentionKind::Question)),
            Decision::Queue
        );
    }

    /// No context means we cannot tell what is on screen; assume a dialog.
    #[test]
    fn an_unexplained_wait_is_queued() {
        assert_eq!(d(SessionStatus::AwaitingInput, None), Decision::Queue);
    }

    /// `Notification` lags the dialog by ~6s, so `Running` may hide one.
    #[test]
    fn a_running_session_is_queued() {
        assert_eq!(d(SessionStatus::Running, None), Decision::Queue);
    }

    #[test]
    fn other_agents_and_shells_get_nothing() {
        for status in [
            SessionStatus::Idle,
            SessionStatus::ResponseReady,
            SessionStatus::Running,
        ] {
            assert_eq!(
                decide(false, Some(PID), status, None, &heard()),
                Decision::NotClaude
            );
        }
    }

    #[test]
    fn sessions_without_a_live_agent_are_skipped() {
        assert_eq!(
            decide(true, None, SessionStatus::Idle, None, &heard()),
            Decision::NotLive
        );
        assert_eq!(d(SessionStatus::Suspended, None), Decision::NotLive);
        assert_eq!(d(SessionStatus::Done, None), Decision::NotLive);
    }

    /// A second `/remote-control` opens a menu and leaves the session in it.
    #[test]
    fn a_process_already_sent_to_is_not_sent_again() {
        let rc = RemoteControlState {
            sent_to_pid: Some(PID),
            heard_from_pid: Some(PID),
            queued: false,
        };
        assert_eq!(
            decide(true, Some(PID), SessionStatus::Idle, None, &rc),
            Decision::AlreadySent
        );
    }

    /// A resumed session is a new process, with remote control off again.
    #[test]
    fn a_respawned_agent_is_eligible_again() {
        let rc = RemoteControlState {
            sent_to_pid: Some(PID),
            heard_from_pid: Some(PID + 1),
            queued: false,
        };
        assert_eq!(
            decide(true, Some(PID + 1), SessionStatus::Idle, None, &rc),
            Decision::Send
        );
    }

    /// A new session starts `Idle` before any hook, possibly on a startup
    /// dialog, and the hook that would prove otherwise came from an older
    /// process. Neither counts.
    #[test]
    fn idle_before_this_process_has_spoken_is_queued() {
        let unheard = RemoteControlState::default();
        assert_eq!(
            decide(true, Some(PID), SessionStatus::Idle, None, &unheard),
            Decision::Queue
        );
        let stale = RemoteControlState {
            heard_from_pid: Some(PID - 1),
            ..Default::default()
        };
        assert_eq!(
            decide(true, Some(PID), SessionStatus::Idle, None, &stale),
            Decision::Queue
        );
    }

    #[test]
    fn the_banner_counts_each_outcome() {
        let mut s = Summary::default();
        for decision in [
            Decision::Send,
            Decision::Send,
            Decision::Queue,
            Decision::AlreadySent,
            Decision::NotClaude,
            Decision::NotLive,
        ] {
            s.count(decision);
        }
        assert_eq!(
            s,
            Summary {
                sent: 2,
                queued: 1,
                already: 1,
                skipped: 2
            }
        );
        assert_eq!(
            s.message(),
            "Remote control requested for 2 sessions · 1 busy, sent when idle · \
             1 already on · skipped 2 (not live Claude)"
        );
    }

    /// Through the real dispatcher: Away reaches the handler, types into no
    /// session without a live agent, and reports in the notice banner.
    #[gpui::test]
    fn away_reports_in_the_banner_and_skips_sessions_without_an_agent(
        cx: &mut gpui::TestAppContext,
    ) {
        let fx = fixture_with_suspended(cx);
        fx.dispatch(crate::actions::SessionAction::RequestRemoteControlAll, cx);
        let (notice, queued) = fx.update(cx, |state, _w, _cx| {
            (state.sync_notice.clone(), state.away_pending())
        });
        assert_eq!(
            notice.as_deref(),
            Some("Away: no live Claude sessions to request remote control from")
        );
        assert!(!queued, "a suspended session has nothing to queue for");
    }

    /// A queued session that is suspended before it becomes safe is dropped
    /// from the queue rather than waited on forever.
    #[gpui::test]
    fn a_queued_session_that_goes_away_is_dropped(cx: &mut gpui::TestAppContext) {
        let fx = fixture_with_suspended(cx);
        let still_queued = fx.update(cx, |state, _w, cx| {
            state.projects[0].sessions[0].remote_control.queued = true;
            assert!(state.away_pending());
            state.drain_away_queue(cx);
            state.away_pending()
        });
        assert!(!still_queued);
    }

    fn fixture_with_suspended(cx: &mut gpui::TestAppContext) -> crate::app_state::fixture::Fixture {
        use std::time::{Duration, SystemTime};
        let mut project =
            crate::project::Project::new("alpha".into(), std::path::PathBuf::from("/tmp/alpha"));
        let now = SystemTime::now();
        project.sessions.push(Session::suspended_from_persisted(
            "s-1".into(),
            "s-1".into(),
            now,
            now,
            Duration::ZERO,
            None,
        ));
        crate::app_state::fixture::Fixture::with_projects(vec![project], cx)
    }

    #[test]
    fn the_banner_says_so_when_there_is_nothing_to_do() {
        let s = Summary {
            skipped: 3,
            ..Default::default()
        };
        assert_eq!(
            s.message(),
            "Away: no live Claude sessions to request remote control from"
        );
    }

    #[test]
    fn the_banner_omits_empty_counts() {
        let s = Summary {
            sent: 1,
            ..Default::default()
        };
        assert_eq!(s.message(), "Remote control requested for 1 session");
    }
}
