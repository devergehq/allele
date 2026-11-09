//! Per-session context use and the account's quota, read from the end of each
//! session's transcript (DEV-1253).
//!
//! Every Claude session's transcript ends with its latest model call, and
//! that line carries the call's `usage` — which is how full the context
//! window was. Reading it needs no live tailer, so every session's row can
//! show it, not just the one whose Transcript tab is open.
//!
//! Cost is kept to a `stat` per session per tick. A file is only read again
//! when its size or mtime moved, and then only its last
//! [`TAIL_BYTES`](crate::stream::TAIL_BYTES). Polling every clone with real
//! work is what DEV-684 removed; this does not bring it back.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use gpui::Context;

use crate::app_state::AppState;
use crate::settings::AgentKind;
use crate::stream::{summarize_tail, ContextUse, QuotaReading, RichEvent, TailSummary};

/// How often transcripts are checked for a new turn.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// One session's transcript to watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// The stable workspace id (`Session::id`), which survives a `/clear`.
    pub session_id: String,
    pub path: PathBuf,
}

/// What one tick found for one session.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Found {
    pub session_id: String,
    pub summary: TailSummary,
    /// The session now points at a different transcript than last tick (a
    /// `/clear` rotated its conversation), so a figure from the old one no
    /// longer describes it.
    pub conversation_changed: bool,
}

/// Remembers what each transcript looked like when last read.
#[derive(Default)]
pub(crate) struct ContextWatcher {
    /// (mtime, length) per transcript at its last read.
    seen: HashMap<PathBuf, (SystemTime, u64)>,
    /// Transcript each session pointed at last tick.
    paths: HashMap<String, PathBuf>,
}

impl ContextWatcher {
    /// Read every target whose transcript changed since the last call.
    /// Blocking — run it on the background executor.
    pub(crate) fn poll(&mut self, targets: &[Target]) -> Vec<Found> {
        let mut out = Vec::new();
        for t in targets {
            let conversation_changed = self
                .paths
                .insert(t.session_id.clone(), t.path.clone())
                .is_some_and(|prev| prev != t.path);
            let Ok(meta) = std::fs::metadata(&t.path) else {
                if conversation_changed {
                    out.push(Found {
                        session_id: t.session_id.clone(),
                        summary: TailSummary::default(),
                        conversation_changed,
                    });
                }
                continue;
            };
            let key = (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            );
            if self.seen.get(&t.path) == Some(&key) && !conversation_changed {
                continue;
            }
            let Some(summary) = summarize_tail(&t.path) else {
                continue;
            };
            self.seen.insert(t.path.clone(), key);
            out.push(Found {
                session_id: t.session_id.clone(),
                summary,
                conversation_changed,
            });
        }
        // Forget sessions that went away, so the maps track the live set.
        self.seen
            .retain(|p, _| targets.iter().any(|t| &t.path == p));
        self.paths
            .retain(|id, _| targets.iter().any(|t| &t.session_id == id));
        out
    }
}

impl AppState {
    /// The transcript of every Claude session. Touches no disk.
    pub(crate) fn context_targets(&self) -> Vec<Target> {
        let mut out = Vec::new();
        for project in &self.projects {
            for session in &project.sessions {
                let kind = session
                    .agent_id
                    .as_ref()
                    .and_then(|id| self.user_settings.agents.iter().find(|a| &a.id == id))
                    .map(|a| a.kind)
                    .unwrap_or(AgentKind::Claude);
                if kind != AgentKind::Claude {
                    continue;
                }
                let cwd = session
                    .clone_path
                    .clone()
                    .unwrap_or_else(|| project.source_path.clone());
                if let Some(path) =
                    crate::transcript::expected_session_jsonl(&cwd, session.claude_session_id())
                {
                    out.push(Target {
                        session_id: session.id.clone(),
                        path,
                    });
                }
            }
        }
        out
    }

    /// Fold one tick's findings in. Returns whether anything shown changed.
    pub(crate) fn apply_context_found(&mut self, found: Vec<Found>) -> bool {
        let mut changed = false;
        for f in found {
            for reading in f.summary.quota {
                changed |= self.record_quota(reading);
            }
            match (f.summary.context, f.conversation_changed) {
                (Some(ctx), _) => changed |= self.set_context_use(&f.session_id, Some(ctx)),
                (None, true) => changed |= self.set_context_use(&f.session_id, None),
                // An unchanged conversation whose tail held no model call (a
                // huge tool result pushed it out of range) keeps its figure.
                (None, false) => {}
            }
        }
        changed
    }

    /// Fold a parsed event's budget reading in, if it carries one. Used by
    /// the live transcript tailer, which sees each line as it lands.
    pub(crate) fn apply_budget_event(&mut self, session_id: &str, event: &RichEvent) -> bool {
        match event {
            RichEvent::Quota { reading } => self.record_quota(reading.clone()),
            RichEvent::ContextUse { context } => self.set_context_use(session_id, Some(*context)),
            _ => false,
        }
    }

    /// Record a reading against the account. Whichever session carried it,
    /// it lands in the one shared store.
    pub(crate) fn record_quota(&mut self, reading: QuotaReading) -> bool {
        self.account_quota.record(reading)
    }

    fn set_context_use(&mut self, session_id: &str, ctx: Option<ContextUse>) -> bool {
        let Some(session) = self
            .projects
            .iter_mut()
            .flat_map(|p| p.sessions.iter_mut())
            .find(|s| s.id == session_id)
        else {
            return false;
        };
        if session.context_use == ctx {
            return false;
        }
        session.context_use = ctx;
        true
    }
}

/// Start the watcher. Called once, from `startup::pollers::spawn_all`.
pub(crate) fn spawn(cx: &mut Context<AppState>) {
    cx.spawn(async move |this, cx| {
        let mut watcher = ContextWatcher::default();
        // First pass immediately, so a relaunch shows each session's figure
        // without waiting a tick.
        let mut interval = Duration::ZERO;
        loop {
            cx.background_executor().timer(interval).await;
            interval = POLL_INTERVAL;

            let Ok(targets) = this.read_with(cx, |this, _cx| this.context_targets()) else {
                break; // AppState dropped — app is exiting
            };
            let (returned, found) = cx
                .background_executor()
                .spawn(async move {
                    let found = watcher.poll(&targets);
                    (watcher, found)
                })
                .await;
            watcher = returned;
            if found.is_empty() {
                continue;
            }
            if this
                .update(cx, |this: &mut AppState, cx| {
                    if this.apply_context_found(found) {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[],"usage":{"input_tokens":2,"cache_read_input_tokens":123998}}}"#;

    fn target(id: &str, path: PathBuf) -> Target {
        Target {
            session_id: id.into(),
            path,
        }
    }

    #[test]
    fn a_completed_turn_yields_the_sessions_context_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, format!("{TURN}\n")).unwrap();
        let mut w = ContextWatcher::default();
        let found = w.poll(&[target("a", path)]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].summary.context.unwrap().label(), "ctx 62%");
    }

    #[test]
    fn an_unchanged_transcript_is_not_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, format!("{TURN}\n")).unwrap();
        let mut w = ContextWatcher::default();
        let targets = [target("a", path.clone())];
        assert_eq!(w.poll(&targets).len(), 1);
        assert!(w.poll(&targets).is_empty());
        // A new turn appended is picked up.
        std::fs::write(&path, format!("{TURN}\n{TURN}\n")).unwrap();
        assert_eq!(w.poll(&targets).len(), 1);
    }

    #[test]
    fn a_session_with_no_transcript_yet_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ContextWatcher::default();
        assert!(w
            .poll(&[target("a", dir.path().join("none.jsonl"))])
            .is_empty());
    }

    #[test]
    fn a_rotated_conversation_is_flagged_so_the_old_figure_clears() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.jsonl");
        std::fs::write(&old, format!("{TURN}\n")).unwrap();
        let mut w = ContextWatcher::default();
        w.poll(&[target("a", old)]);
        let found = w.poll(&[target("a", dir.path().join("new.jsonl"))]);
        assert_eq!(found.len(), 1);
        assert!(found[0].conversation_changed);
        assert!(found[0].summary.context.is_none());
    }

    /// The account-level guarantee: two sessions on one account, each
    /// carrying its own `rate_limit_event`, show the same quota at the same
    /// instant — because both fold into one store and the line is read from
    /// that store, never from a session.
    #[test]
    fn two_sessions_on_one_account_show_identical_quota() {
        use crate::stream::{AccountQuota, StreamParser};

        let now = 1_000_000;
        let older = format!(
            r#"{{"type":"rate_limit_event","rate_limit_info":{{"rateLimitType":"five_hour","utilization":0.08,"resetsAt":{}}}}}"#,
            now + 3600
        );
        let newer = format!(
            r#"{{"type":"rate_limit_event","rate_limit_info":{{"rateLimitType":"five_hour","utilization":0.95,"resetsAt":{}}}}}"#,
            now + 3600
        );

        let mut account = AccountQuota::default();
        let mut session_a = StreamParser::new();
        let mut session_b = StreamParser::new();
        for (parser, line) in [(&mut session_a, &older), (&mut session_b, &newer)] {
            for event in parser.feed_line(line) {
                if let RichEvent::Quota { reading } = event {
                    account.record(reading);
                }
            }
        }

        // Each session's readout is the account's line. Rendering takes no
        // session argument, so there is nothing that could make them differ.
        let shown_for_a = account.line(now);
        let shown_for_b = account.line(now);
        assert_eq!(shown_for_a, shown_for_b);
        assert_eq!(shown_for_a.as_deref(), Some("5h 95% · resets in 1h"));
    }
}
