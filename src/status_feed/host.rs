//! Host side: hold the last published view and fan changes out to
//! subscribers.
//!
//! [`StatusFeed`] is a cheap `Clone` handle. `AppState` owns one and calls
//! [`StatusFeed::publish`] with the current view; the transport holds a clone
//! and calls [`StatusFeed::subscribe`] from any thread, without touching
//! `AppState`. Because the publisher keeps its own copy of the view, a
//! subscription never has to hop to the UI thread to build a snapshot.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use super::diff::diff;
use super::model::{FeedAttention, FeedStatus, SessionEntry, StatusDelta, StatusSnapshot};
use crate::project::Project;
use crate::session::{AttentionKind, Session, SessionStatus};

/// Deltas a subscriber may fall behind by before it is told it lagged. A
/// lagged subscriber resubscribes and gets a fresh snapshot, so this bounds
/// memory, not correctness.
const DELTA_BUFFER: usize = 256;

/// A new subscriber's starting point: everything up to `snapshot.seq`, then
/// every delta after it on `deltas`, with nothing missed in between.
pub(crate) struct Subscription {
    pub snapshot: StatusSnapshot,
    /// `RecvError::Lagged` means deltas were dropped; the consumer must
    /// resubscribe rather than carry on.
    pub deltas: broadcast::Receiver<StatusDelta>,
}

struct Publisher {
    /// Sequence number of the last delta emitted. 0 before the first.
    seq: u64,
    last: Vec<SessionEntry>,
    tx: broadcast::Sender<StatusDelta>,
}

#[derive(Clone)]
pub(crate) struct StatusFeed(Arc<Mutex<Publisher>>);

impl Default for StatusFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusFeed {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(DELTA_BUFFER);
        Self(Arc::new(Mutex::new(Publisher {
            seq: 0,
            last: Vec::new(),
            tx,
        })))
    }

    /// A feed already holding `projects`' sessions. Startup uses this: a
    /// client may subscribe before anything marks state dirty, and must
    /// still see every session in its snapshot.
    pub(crate) fn seeded(projects: &[Project]) -> Self {
        let feed = Self::new();
        feed.publish(entries_from_projects(projects));
        feed
    }

    fn lock(&self) -> MutexGuard<'_, Publisher> {
        // Nothing in the critical section can leave the view half-written,
        // so a poisoned lock is still a consistent one.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the published view with `current`, emitting one delta per
    /// difference. Returns how many were emitted. Idempotent: publishing the
    /// same view twice emits nothing the second time.
    pub(crate) fn publish(&self, current: Vec<SessionEntry>) -> usize {
        let mut p = self.lock();
        let changes = diff(&p.last, &current);
        let emitted = changes.len();
        for change in changes {
            p.seq += 1;
            // `Err` only means nobody is subscribed. That is the common case
            // and not an error: the view is kept either way.
            let _ = p.tx.send(StatusDelta { seq: p.seq, change });
        }
        // Store even when nothing was emitted, so `last_activity_ms` in the
        // next snapshot is fresh.
        p.last = current;
        emitted
    }

    /// Start a subscription. The snapshot and the receiver are taken under
    /// the same lock as `publish`, so no delta can fall between them.
    pub(crate) fn subscribe(&self) -> Subscription {
        let p = self.lock();
        Subscription {
            snapshot: StatusSnapshot {
                seq: p.seq,
                sessions: p.last.clone(),
            },
            deltas: p.tx.subscribe(),
        }
    }
}

/// The current view of every live session, in sidebar order. Sessions still
/// being created (`loading_sessions`) are not sessions yet and are left out.
pub(crate) fn entries_from_projects(projects: &[Project]) -> Vec<SessionEntry> {
    projects
        .iter()
        .flat_map(|p| p.sessions.iter().map(move |s| entry_for(p, s)))
        .collect()
}

fn entry_for(project: &Project, session: &Session) -> SessionEntry {
    let status = feed_status(session.status);
    let attention = if session.status == SessionStatus::AwaitingInput {
        session
            .attention_context
            .as_ref()
            .map(|c| feed_attention(c.kind))
    } else {
        None
    };
    SessionEntry {
        id: session.id.clone(),
        label: session.label.clone(),
        project_id: project.id.clone(),
        project_name: project.name.clone(),
        group: session.group.clone(),
        status,
        attention,
        last_activity_ms: unix_ms(session.last_active),
    }
}

fn feed_status(s: SessionStatus) -> FeedStatus {
    match s {
        SessionStatus::Running => FeedStatus::Running,
        SessionStatus::Idle => FeedStatus::Idle,
        SessionStatus::Done => FeedStatus::Done,
        SessionStatus::Suspended => FeedStatus::Suspended,
        SessionStatus::AwaitingInput => FeedStatus::AwaitingInput,
        SessionStatus::ResponseReady => FeedStatus::ResponseReady,
    }
}

fn feed_attention(k: AttentionKind) -> FeedAttention {
    match k {
        AttentionKind::Permission => FeedAttention::Permission,
        AttentionKind::Question => FeedAttention::Question,
        AttentionKind::Idle => FeedAttention::Idle,
    }
}

pub(crate) fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui::TestAppContext;
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;
    use crate::actions::SessionAction;
    use crate::app_state::fixture::Fixture;
    use crate::status_feed::client::{DeltaOutcome, RemoteHostState};
    use crate::status_feed::diff::tests::entry;
    use crate::status_feed::model::StatusChange;

    fn project(name: &str, session_ids: &[&str]) -> Project {
        let mut p = Project::new(name.to_string(), PathBuf::from(format!("/tmp/{name}")));
        for id in session_ids {
            let now = SystemTime::now();
            p.sessions.push(Session::suspended_from_persisted(
                (*id).into(),
                format!("label {id}"),
                now,
                now,
                Duration::ZERO,
                None,
            ));
        }
        p
    }

    fn drain(sub: &mut Subscription) -> Vec<StatusDelta> {
        let mut out = Vec::new();
        loop {
            match sub.deltas.try_recv() {
                Ok(d) => out.push(d),
                Err(TryRecvError::Empty) => return out,
                Err(e) => panic!("subscription broke: {e:?}"),
            }
        }
    }

    // ── publisher ─────────────────────────────────────────────────────

    #[test]
    fn a_subscriber_gets_the_current_view_then_only_later_changes() {
        let feed = StatusFeed::new();
        feed.publish(vec![entry("a", FeedStatus::Idle)]);
        feed.publish(vec![entry("a", FeedStatus::Running)]);

        let mut sub = feed.subscribe();
        assert_eq!(sub.snapshot.seq, 2);
        assert_eq!(sub.snapshot.sessions, vec![entry("a", FeedStatus::Running)]);

        feed.publish(vec![entry("a", FeedStatus::ResponseReady)]);
        let deltas = drain(&mut sub);
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].seq, 3, "the first delta follows the snapshot");
    }

    #[test]
    fn republishing_the_same_view_emits_nothing() {
        let feed = StatusFeed::new();
        let mut sub = feed.subscribe();
        let view = vec![entry("a", FeedStatus::Idle)];
        assert_eq!(feed.publish(view.clone()), 1);
        assert_eq!(feed.publish(view), 0);
        assert_eq!(drain(&mut sub).len(), 1);
    }

    #[test]
    fn last_activity_reaches_the_next_snapshot_without_a_delta() {
        let feed = StatusFeed::new();
        feed.publish(vec![entry("a", FeedStatus::Running)]);
        let mut busier = entry("a", FeedStatus::Running);
        busier.last_activity_ms = 9_000;
        assert_eq!(feed.publish(vec![busier]), 0);
        assert_eq!(
            feed.subscribe().snapshot.sessions[0].last_activity_ms,
            9_000
        );
    }

    /// End to end without a network: host publishes, client store applies,
    /// and they agree after each step.
    #[test]
    fn a_client_fed_from_the_publisher_tracks_the_host() {
        let feed = StatusFeed::new();
        feed.publish(vec![
            entry("a", FeedStatus::Idle),
            entry("b", FeedStatus::Idle),
        ]);

        let mut sub = feed.subscribe();
        let mut client = RemoteHostState::new();
        client.apply_snapshot(sub.snapshot.clone(), SystemTime::now());

        let mut renamed = entry("b", FeedStatus::AwaitingInput);
        renamed.label = "renamed".into();
        let host_view = vec![renamed, entry("c", FeedStatus::Running)];
        feed.publish(host_view.clone());

        for d in drain(&mut sub) {
            assert!(matches!(
                client.apply_delta(d, SystemTime::now()),
                DeltaOutcome::Applied { .. }
            ));
        }
        assert_eq!(client.sessions(), host_view.as_slice());
    }

    #[test]
    fn a_lagging_subscriber_is_told_so_rather_than_silently_skipped() {
        let feed = StatusFeed::new();
        let mut sub = feed.subscribe();
        for i in 0..(DELTA_BUFFER + 5) {
            let s = if i % 2 == 0 {
                FeedStatus::Running
            } else {
                FeedStatus::Idle
            };
            feed.publish(vec![entry("a", s)]);
        }
        assert!(matches!(
            sub.deltas.try_recv(),
            Err(TryRecvError::Lagged(_))
        ));
    }

    // ── view built from AppState ─────────────────────────────────────

    #[test]
    fn the_view_lists_every_session_with_its_project() {
        let projects = vec![project("alpha", &["a1", "a2"]), project("beta", &["b1"])];
        let view = entries_from_projects(&projects);
        let got: Vec<(&str, &str, &str)> = view
            .iter()
            .map(|e| (e.id.as_str(), e.label.as_str(), e.project_name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("a1", "label a1", "alpha"),
                ("a2", "label a2", "alpha"),
                ("b1", "label b1", "beta"),
            ]
        );
        assert!(view.iter().all(|e| e.status == FeedStatus::Suspended));
        assert!(view.iter().all(|e| e.last_activity_ms > 0));
        assert_eq!(view[2].project_id, projects[1].id);
    }

    // AC: a client's subscription receives every host session in the
    // snapshot — including one that subscribes before anything changed.
    #[test]
    fn a_feed_seeded_at_startup_snapshots_every_session() {
        let projects = vec![project("alpha", &["a1", "a2"]), project("beta", &["b1"])];
        let sub = StatusFeed::seeded(&projects).subscribe();
        assert_eq!(sub.snapshot.sessions, entries_from_projects(&projects));
        assert_eq!(sub.snapshot.sessions.len(), 3);
    }

    #[test]
    fn attention_is_reported_only_while_awaiting_input() {
        let mut projects = vec![project("alpha", &["a"])];
        let s = &mut projects[0].sessions[0];
        s.attention_context = Some(crate::session::AttentionContext {
            kind: AttentionKind::Question,
            tool_name: None,
            tool_input_summary: None,
            message: None,
        });
        s.status = SessionStatus::Running;
        assert_eq!(entries_from_projects(&projects)[0].attention, None);
        projects[0].sessions[0].status = SessionStatus::AwaitingInput;
        assert_eq!(
            entries_from_projects(&projects)[0].attention,
            Some(FeedAttention::Question)
        );
    }

    // ── AppState wiring: deltas without polling ──────────────────────

    fn subscribe(fx: &Fixture, cx: &mut TestAppContext) -> Subscription {
        fx.update(cx, |state, _w, _cx| {
            // Seed the publisher the way startup's first `mark_state_dirty` would.
            state.publish_status_feed();
            state.status_feed.subscribe()
        })
    }

    fn hook(session_id: &str, kind: &str) -> crate::hooks::HookEvent {
        crate::hooks::HookEvent {
            session_id: session_id.into(),
            kind: kind.into(),
            cwd: None,
            payload: None,
        }
    }

    // AC: a status change on the host reaches the client within 2 seconds.
    // Over the network that is transport latency; here, the bound is that the
    // delta is queued before the hook handler returns.
    #[gpui::test]
    fn a_hook_event_is_a_delta_before_the_handler_returns(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![project("alpha", &["s1"])], cx);
        let mut sub = subscribe(&fx, cx);
        assert_eq!(sub.snapshot.sessions.len(), 1);

        fx.update(cx, |state, _w, cx| {
            state.apply_hook_event(hook("s1", "stop"), cx)
        });

        let deltas = drain(&mut sub);
        assert!(
            matches!(
                deltas.as_slice(),
                [StatusDelta {
                    change: StatusChange::StatusChanged {
                        status: FeedStatus::ResponseReady,
                        ..
                    },
                    ..
                }]
            ),
            "got {deltas:?}"
        );
    }

    // AC: a session renamed on the host shows the new name on the client.
    #[gpui::test]
    fn renaming_a_session_is_a_renamed_delta(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![project("alpha", &["s1"])], cx);
        let mut sub = subscribe(&fx, cx);

        fx.dispatch(
            SessionAction::ApplySessionEdit {
                project_idx: 0,
                session_idx: 0,
                label: "fix the build".into(),
                branch_slug: None,
                comment: None,
                pinned: false,
                orchestration: crate::session::Orchestration::Nothing,
                group: None,
            },
            cx,
        );

        let deltas = drain(&mut sub);
        assert_eq!(
            deltas.iter().map(|d| &d.change).collect::<Vec<_>>(),
            [&StatusChange::Renamed {
                id: "s1".into(),
                label: "fix the build".into()
            }]
        );
    }

    // AC: a session closed on the host disappears from the client. Every
    // removal path ends in `mark_state_dirty`, which is the trigger.
    #[gpui::test]
    fn removing_a_session_is_a_removed_delta(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![project("alpha", &["s1", "s2"])], cx);
        let mut sub = subscribe(&fx, cx);

        fx.update(cx, |state, _w, _cx| {
            state.projects[0].sessions.remove(0);
            state.mark_state_dirty();
        });

        let deltas = drain(&mut sub);
        assert_eq!(
            deltas.iter().map(|d| &d.change).collect::<Vec<_>>(),
            [&StatusChange::Removed { id: "s1".into() }]
        );
    }

    #[gpui::test]
    fn adding_a_session_is_an_added_delta(cx: &mut TestAppContext) {
        let fx = Fixture::with_projects(vec![project("alpha", &["s1"])], cx);
        let mut sub = subscribe(&fx, cx);

        fx.update(cx, |state, _w, _cx| {
            let extra = project("x", &["s2"]).sessions.remove(0);
            state.projects[0].sessions.push(extra);
            state.mark_state_dirty();
        });

        let deltas = drain(&mut sub);
        assert!(
            matches!(
                deltas.as_slice(),
                [StatusDelta { change: StatusChange::Added { session }, .. }] if session.id == "s2"
            ),
            "got {deltas:?}"
        );
    }
}
