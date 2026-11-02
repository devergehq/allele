//! Client side: one paired host's sessions, as last heard.
//!
//! The rules the sidebar (DEV-1163) and remote attention (DEV-1164) lean on:
//!
//! - A snapshot replaces everything. Whatever deltas came before it are gone.
//! - Deltas apply strictly in sequence. A duplicate is dropped; a gap stops
//!   the store taking deltas until the next snapshot, because applying past a
//!   gap would show a state the host was never in.
//! - Losing the connection keeps the sessions and marks the host
//!   `Unreachable { since }`. Repeated failed reconnects do not move `since`.
//! - Only a delta reports a live [`Transition`]. A snapshot reports what
//!   changed while we were away as `missed`, separately, so a reconnect can
//!   never look like fresh news — a session that was already AwaitingInput
//!   before the drop is not a transition at all.

use std::time::SystemTime;

use super::model::{FeedStatus, SessionEntry, StatusChange, StatusDelta, StatusSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reachability {
    /// No snapshot yet since this store was created.
    Connecting,
    /// Subscribed; the sessions are live.
    Connected,
    /// The connection dropped. The sessions are the last known; `since` is
    /// when we noticed, i.e. the last time the host was reachable.
    Unreachable { since: SystemTime },
}

/// A session's status moved. `from: None` means the session is new to us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transition {
    pub id: String,
    pub from: Option<FeedStatus>,
    pub to: FeedStatus,
}

impl Transition {
    /// True when this is the session starting to need the user, which is
    /// what a notification fires on.
    pub(crate) fn enters_awaiting_input(&self) -> bool {
        self.to == FeedStatus::AwaitingInput && self.from != Some(FeedStatus::AwaitingInput)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotOutcome {
    /// The first snapshot this store has seen (a cold start, not a reconnect).
    pub first: bool,
    /// Status differences between the last-known state and this snapshot.
    /// Always empty on the first snapshot. These happened while we were not
    /// listening; whether to surface them is the caller's call.
    pub missed: Vec<Transition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeltaOutcome {
    /// Applied. `transition` is set when a session's status changed or a
    /// session appeared.
    Applied { transition: Option<Transition> },
    /// Already reflected (`seq` at or before the current one). Dropped.
    Stale,
    /// Deltas were lost. The store takes no more until a snapshot arrives.
    Gap { expected: u64, got: u64 },
    /// The delta names a session we do not have, so our view has diverged.
    /// Same treatment as a gap.
    Diverged { id: String },
    /// No live subscription: disconnected, or waiting for a resync snapshot.
    NotSubscribed,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteHostState {
    sessions: Vec<SessionEntry>,
    /// The last applied sequence number while subscribed and in sync.
    seq: Option<u64>,
    reachability: Reachability,
    ever_synced: bool,
    last_heard: Option<SystemTime>,
}

impl Default for RemoteHostState {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteHostState {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Vec::new(),
            seq: None,
            reachability: Reachability::Connecting,
            ever_synced: false,
            last_heard: None,
        }
    }

    /// Host sessions in host sidebar order. Last known while unreachable.
    pub(crate) fn sessions(&self) -> &[SessionEntry] {
        &self.sessions
    }

    pub(crate) fn session(&self, id: &str) -> Option<&SessionEntry> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// Sessions blocked on the user, for the attention bar.
    #[allow(dead_code)] // first caller: remote attention + notifications (DEV-1164)
    pub(crate) fn awaiting_input(&self) -> impl Iterator<Item = &SessionEntry> {
        self.sessions
            .iter()
            .filter(|s| s.status == FeedStatus::AwaitingInput)
    }

    pub(crate) fn reachability(&self) -> Reachability {
        self.reachability
    }

    /// When a snapshot or delta last arrived.
    #[allow(dead_code)] // first caller: the Machines sidebar (DEV-1163)
    pub(crate) fn last_heard(&self) -> Option<SystemTime> {
        self.last_heard
    }

    /// Connected but out of sync after a gap: the transport should
    /// resubscribe to get a fresh snapshot.
    #[allow(dead_code)] // the follower resyncs on the delta outcome; for a UI that wants to show it
    pub(crate) fn needs_resync(&self) -> bool {
        self.reachability == Reachability::Connected && self.seq.is_none()
    }

    /// Replace everything with `snapshot`. The host is reachable again.
    pub(crate) fn apply_snapshot(
        &mut self,
        snapshot: StatusSnapshot,
        now: SystemTime,
    ) -> SnapshotOutcome {
        let first = !self.ever_synced;
        let missed = if first {
            Vec::new()
        } else {
            snapshot
                .sessions
                .iter()
                .filter_map(|new| {
                    let from = self.session(&new.id).map(|old| old.status);
                    (from != Some(new.status)).then(|| Transition {
                        id: new.id.clone(),
                        from,
                        to: new.status,
                    })
                })
                .collect()
        };
        self.sessions = snapshot.sessions;
        self.seq = Some(snapshot.seq);
        self.reachability = Reachability::Connected;
        self.ever_synced = true;
        self.last_heard = Some(now);
        SnapshotOutcome { first, missed }
    }

    pub(crate) fn apply_delta(&mut self, delta: StatusDelta, now: SystemTime) -> DeltaOutcome {
        let Some(seq) = self.seq else {
            return DeltaOutcome::NotSubscribed;
        };
        if delta.seq <= seq {
            return DeltaOutcome::Stale;
        }
        if delta.seq != seq + 1 {
            self.seq = None;
            return DeltaOutcome::Gap {
                expected: seq + 1,
                got: delta.seq,
            };
        }
        let transition = match self.apply_change(delta.change) {
            Ok(t) => t,
            Err(id) => {
                self.seq = None;
                return DeltaOutcome::Diverged { id };
            }
        };
        self.seq = Some(delta.seq);
        self.last_heard = Some(now);
        DeltaOutcome::Applied { transition }
    }

    /// `Err(id)` when the change names a session we do not hold.
    fn apply_change(&mut self, change: StatusChange) -> Result<Option<Transition>, String> {
        match change {
            StatusChange::Added { session } => {
                let to = session.status;
                let id = session.id.clone();
                let from = match self.sessions.iter_mut().find(|s| s.id == session.id) {
                    Some(existing) => {
                        let from = existing.status;
                        *existing = session;
                        Some(from)
                    }
                    None => {
                        self.sessions.push(session);
                        None
                    }
                };
                Ok((from != Some(to)).then_some(Transition { id, from, to }))
            }
            StatusChange::Removed { id } => {
                // Removing what is already gone leaves the same end state.
                self.sessions.retain(|s| s.id != id);
                Ok(None)
            }
            StatusChange::Renamed { id, label } => {
                let s = self.find_mut(&id)?;
                s.label = label;
                Ok(None)
            }
            StatusChange::StatusChanged {
                id,
                status,
                attention,
                last_activity_ms,
            } => {
                let s = self.find_mut(&id)?;
                let from = s.status;
                s.status = status;
                s.attention = attention;
                s.last_activity_ms = last_activity_ms;
                Ok((from != status).then_some(Transition {
                    id,
                    from: Some(from),
                    to: status,
                }))
            }
        }
    }

    fn find_mut(&mut self, id: &str) -> Result<&mut SessionEntry, String> {
        self.sessions
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| id.to_string())
    }

    /// The connection dropped. Sessions stay; the host is unreachable from
    /// `now`, or from the earlier time if it already was.
    pub(crate) fn connection_lost(&mut self, now: SystemTime) {
        self.seq = None;
        if !matches!(self.reachability, Reachability::Unreachable { .. }) {
            self.reachability = Reachability::Unreachable { since: now };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::status_feed::diff::tests::entry;
    use crate::status_feed::model::FeedAttention;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn snap(seq: u64, sessions: Vec<SessionEntry>) -> StatusSnapshot {
        StatusSnapshot { seq, sessions }
    }

    fn status(seq: u64, id: &str, to: FeedStatus) -> StatusDelta {
        StatusDelta {
            seq,
            change: StatusChange::StatusChanged {
                id: id.into(),
                status: to,
                attention: None,
                last_activity_ms: seq,
            },
        }
    }

    fn connected(sessions: Vec<SessionEntry>) -> RemoteHostState {
        let mut h = RemoteHostState::new();
        h.apply_snapshot(snap(10, sessions), at(1));
        h
    }

    fn ids(h: &RemoteHostState) -> Vec<&str> {
        h.sessions().iter().map(|s| s.id.as_str()).collect()
    }

    // AC: a client's subscription receives every host session in the snapshot.
    #[test]
    fn a_snapshot_lists_every_session_and_marks_the_host_connected() {
        let mut h = RemoteHostState::new();
        assert_eq!(h.reachability(), Reachability::Connecting);

        let out = h.apply_snapshot(
            snap(
                3,
                vec![
                    entry("a", FeedStatus::Idle),
                    entry("b", FeedStatus::AwaitingInput),
                ],
            ),
            at(5),
        );

        assert!(out.first);
        assert!(out.missed.is_empty(), "a cold start is never news");
        assert_eq!(ids(&h), ["a", "b"]);
        assert_eq!(h.reachability(), Reachability::Connected);
        assert_eq!(h.last_heard(), Some(at(5)));
        assert_eq!(h.awaiting_input().count(), 1);
    }

    #[test]
    fn a_snapshot_replaces_everything_a_delta_built() {
        let mut h = connected(vec![entry("a", FeedStatus::Idle)]);
        h.apply_delta(
            StatusDelta {
                seq: 11,
                change: StatusChange::Added {
                    session: entry("b", FeedStatus::Running),
                },
            },
            at(2),
        );
        assert_eq!(ids(&h), ["a", "b"]);

        h.apply_snapshot(snap(3, vec![entry("c", FeedStatus::Idle)]), at(3));
        assert_eq!(ids(&h), ["c"], "nothing from before the snapshot survives");

        // The snapshot also resets the sequence: the next delta is 4, even
        // though it is lower than anything seen before.
        assert!(matches!(
            h.apply_delta(status(4, "c", FeedStatus::Running), at(4)),
            DeltaOutcome::Applied { .. }
        ));
    }

    // AC: a status change on the host reaches the client.
    #[test]
    fn a_status_delta_updates_the_session_and_reports_the_transition() {
        let mut h = connected(vec![entry("a", FeedStatus::Running)]);
        let out = h.apply_delta(
            StatusDelta {
                seq: 11,
                change: StatusChange::StatusChanged {
                    id: "a".into(),
                    status: FeedStatus::AwaitingInput,
                    attention: Some(FeedAttention::Permission),
                    last_activity_ms: 77,
                },
            },
            at(2),
        );
        let DeltaOutcome::Applied {
            transition: Some(t),
        } = out
        else {
            panic!("expected an applied transition, got {out:?}");
        };
        assert!(t.enters_awaiting_input());
        let a = h.session("a").expect("still there");
        assert_eq!(a.status, FeedStatus::AwaitingInput);
        assert_eq!(a.attention, Some(FeedAttention::Permission));
        assert_eq!(a.last_activity_ms, 77);
        assert_eq!(h.last_heard(), Some(at(2)));
    }

    // AC: a session closed on the host disappears from the client.
    #[test]
    fn a_removed_session_disappears() {
        let mut h = connected(vec![
            entry("a", FeedStatus::Idle),
            entry("b", FeedStatus::Idle),
        ]);
        let out = h.apply_delta(
            StatusDelta {
                seq: 11,
                change: StatusChange::Removed { id: "a".into() },
            },
            at(2),
        );
        assert_eq!(out, DeltaOutcome::Applied { transition: None });
        assert_eq!(ids(&h), ["b"]);
    }

    // AC: a session renamed on the host shows the new name on the client.
    #[test]
    fn a_renamed_session_shows_the_new_name() {
        let mut h = connected(vec![entry("a", FeedStatus::Idle)]);
        h.apply_delta(
            StatusDelta {
                seq: 11,
                change: StatusChange::Renamed {
                    id: "a".into(),
                    label: "fix the build".into(),
                },
            },
            at(2),
        );
        assert_eq!(
            h.session("a").map(|s| s.label.as_str()),
            Some("fix the build")
        );
    }

    #[test]
    fn an_added_session_appends_and_an_added_existing_one_replaces_in_place() {
        let mut h = connected(vec![
            entry("a", FeedStatus::Idle),
            entry("b", FeedStatus::Idle),
        ]);
        let out = h.apply_delta(
            StatusDelta {
                seq: 11,
                change: StatusChange::Added {
                    session: entry("c", FeedStatus::AwaitingInput),
                },
            },
            at(2),
        );
        assert!(matches!(
            out,
            DeltaOutcome::Applied { transition: Some(ref t) } if t.from.is_none() && t.enters_awaiting_input()
        ));

        let mut moved = entry("a", FeedStatus::Idle);
        moved.project_name = "elsewhere".into();
        let out = h.apply_delta(
            StatusDelta {
                seq: 12,
                change: StatusChange::Added { session: moved },
            },
            at(3),
        );
        assert_eq!(out, DeltaOutcome::Applied { transition: None });
        assert_eq!(ids(&h), ["a", "b", "c"], "order kept");
        assert_eq!(
            h.session("a").map(|s| s.project_name.as_str()),
            Some("elsewhere")
        );
    }

    #[test]
    fn deltas_apply_in_order_and_a_replay_is_dropped() {
        let mut h = connected(vec![entry("a", FeedStatus::Idle)]);
        h.apply_delta(status(11, "a", FeedStatus::Running), at(2));
        h.apply_delta(status(12, "a", FeedStatus::ResponseReady), at(3));
        assert_eq!(
            h.session("a").map(|s| s.status),
            Some(FeedStatus::ResponseReady)
        );

        // A replay of 11 must not roll the session back to Running.
        assert_eq!(
            h.apply_delta(status(11, "a", FeedStatus::Running), at(4)),
            DeltaOutcome::Stale
        );
        // Nor may anything the snapshot already covered.
        assert_eq!(
            h.apply_delta(status(10, "a", FeedStatus::Running), at(4)),
            DeltaOutcome::Stale
        );
        assert_eq!(
            h.session("a").map(|s| s.status),
            Some(FeedStatus::ResponseReady)
        );
    }

    #[test]
    fn a_gap_stops_the_store_until_the_next_snapshot() {
        let mut h = connected(vec![entry("a", FeedStatus::Idle)]);
        assert_eq!(
            h.apply_delta(status(12, "a", FeedStatus::Running), at(2)),
            DeltaOutcome::Gap {
                expected: 11,
                got: 12
            }
        );
        assert_eq!(h.session("a").map(|s| s.status), Some(FeedStatus::Idle));
        assert!(h.needs_resync());
        // Even the delta that was "missing" is refused now: we cannot know
        // what else was lost.
        assert_eq!(
            h.apply_delta(status(11, "a", FeedStatus::Running), at(3)),
            DeltaOutcome::NotSubscribed
        );

        h.apply_snapshot(snap(12, vec![entry("a", FeedStatus::Running)]), at(4));
        assert!(!h.needs_resync());
    }

    #[test]
    fn a_delta_for_an_unknown_session_forces_a_resync() {
        let mut h = connected(vec![entry("a", FeedStatus::Idle)]);
        assert_eq!(
            h.apply_delta(status(11, "ghost", FeedStatus::Running), at(2)),
            DeltaOutcome::Diverged { id: "ghost".into() }
        );
        assert!(h.needs_resync());
    }

    // AC: while the host is unreachable, the client shows the time it was
    // last reachable.
    #[test]
    fn losing_the_host_keeps_its_sessions_and_the_time_it_went_away() {
        let mut h = connected(vec![entry("a", FeedStatus::AwaitingInput)]);
        h.connection_lost(at(100));
        assert_eq!(
            h.reachability(),
            Reachability::Unreachable { since: at(100) }
        );
        assert_eq!(ids(&h), ["a"], "last-known sessions stay listed");

        // Every failed reconnect reports the loss again; `since` must not
        // creep forward, or "unreachable since" would always read "just now".
        h.connection_lost(at(160));
        h.connection_lost(at(220));
        assert_eq!(
            h.reachability(),
            Reachability::Unreachable { since: at(100) }
        );

        // Deltas from the dead link are refused.
        assert_eq!(
            h.apply_delta(status(11, "a", FeedStatus::Running), at(230)),
            DeltaOutcome::NotSubscribed
        );
    }

    // AC (data half): after the host sleeps and wakes, the resubscription's
    // snapshot brings the client back without user action.
    #[test]
    fn a_reconnect_snapshot_restores_the_host() {
        let mut h = connected(vec![entry("a", FeedStatus::Running)]);
        h.connection_lost(at(100));
        let out = h.apply_snapshot(
            snap(
                2,
                vec![
                    entry("a", FeedStatus::ResponseReady),
                    entry("b", FeedStatus::Idle),
                ],
            ),
            at(500),
        );
        assert!(!out.first);
        assert_eq!(h.reachability(), Reachability::Connected);
        assert_eq!(ids(&h), ["a", "b"]);
        assert!(matches!(
            h.apply_delta(status(3, "b", FeedStatus::Running), at(501)),
            DeltaOutcome::Applied { .. }
        ));
    }

    // DEV-1164 relies on this: a reconnect must not re-notify.
    #[test]
    fn a_reconnect_does_not_report_a_session_that_was_already_waiting() {
        let mut h = connected(vec![
            entry("waiting", FeedStatus::AwaitingInput),
            entry("working", FeedStatus::Running),
        ]);
        h.connection_lost(at(100));
        let out = h.apply_snapshot(
            snap(
                1,
                vec![
                    entry("waiting", FeedStatus::AwaitingInput),
                    entry("working", FeedStatus::AwaitingInput),
                    entry("new", FeedStatus::Idle),
                ],
            ),
            at(200),
        );
        assert_eq!(
            out.missed,
            vec![
                Transition {
                    id: "working".into(),
                    from: Some(FeedStatus::Running),
                    to: FeedStatus::AwaitingInput,
                },
                Transition {
                    id: "new".into(),
                    from: None,
                    to: FeedStatus::Idle,
                },
            ],
            "only sessions whose status differs are reported, and only as missed"
        );
        assert!(!out.missed.iter().any(|t| t.id == "waiting"));
    }

    #[test]
    fn a_host_that_never_answered_is_unreachable_from_the_first_failure() {
        let mut h = RemoteHostState::new();
        h.connection_lost(at(7));
        assert_eq!(h.reachability(), Reachability::Unreachable { since: at(7) });
        assert!(h.sessions().is_empty());
        let out = h.apply_snapshot(snap(0, vec![entry("a", FeedStatus::Idle)]), at(9));
        assert!(out.first, "the first snapshot ever is still a cold start");
    }
}
