//! The `allele.ext/1` session stream's fan-out (DEV-1254).
//!
//! The foreground thread hands the hub a [`View`] of every session; the hub
//! diffs it against the last one and queues a delta per change for each
//! subscriber. Socket threads own the other end of each queue and do the
//! writing. Nothing here knows about GPUI, which is what lets the sequencing,
//! filtering and backpressure rules be tested without an app.
//!
//! ## Why diffing rather than events at each call site
//!
//! A status change can come from a hook, the transcript tail, the UI, a resume
//! or a dispatch, and a turn can start and finish between two looks. Diffing a
//! complete view finds every change no matter which path made it, so no call
//! site has to remember to announce itself. `turn` is the one thing a diff
//! cannot reconstruct, which is why it is counted in
//! [`crate::session::Session::set_status`] and merely read here. Only removal
//! reasons and dispatch refusals are told to the hub directly, because a diff
//! sees that a session is gone but not why, and a refusal leaves no trace in
//! the view at all.
//!
//! ## Never blocks the foreground
//!
//! Each subscriber has a bounded queue, and the foreground only ever
//! `try_send`s onto it. A full queue marks the subscriber lagged and drops it;
//! its socket thread then writes `lagged` (if the client is reading at all)
//! and closes. A slow reader costs itself its subscription and costs the UI
//! nothing.
//!
//! The view is only built while someone is listening: while a subscriber
//! exists (at most every [`DIFF_INTERVAL`]), or when a socket thread is
//! waiting on a [`Hub::query`]. With nobody connected it costs one lock per
//! drain tick.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use allele_contract::{
    DispatchLimits, DispatchRefused, ErrorCode, Event, Filter, Frame, RemovalReason, SessionEntry,
};
use tracing::warn;

/// Frames a subscriber may have queued before it is dropped as lagged.
///
/// Generous: a snapshot plus a burst of a thousand transitions fits. A reader
/// that falls this far behind is not reading, and resubscribing gets it a
/// fresh snapshot, which is cheaper for both sides than catching up.
pub(crate) const QUEUE_CAPACITY: usize = 1024;

/// How often a view is diffed while someone is subscribed.
///
/// Status arrives through the hook poller every 250 ms, so diffing faster buys
/// nothing; this keeps a change's worst case under half a second from the
/// hook landing to the frame being queued.
pub(crate) const DIFF_INTERVAL: Duration = Duration::from_millis(200);

/// A complete look at Allele's sessions, built on the foreground thread.
#[derive(Debug, Clone, Default)]
pub(crate) struct View {
    /// Every live session, in sidebar order.
    pub sessions: Vec<SessionEntry>,
    /// Archived sessions as `state: archived` entries. Only filled when a
    /// [`Hub::query`] asked for it, since only `sessions_status` reads it.
    pub archived: Vec<SessionEntry>,
    /// Ids with an archive row, so a vanished session can be reported as
    /// `archived` rather than `closed`.
    pub archived_ids: HashSet<String>,
    pub limits: DispatchLimits,
}

/// A socket thread's end of one subscription.
pub(crate) struct Subscription {
    /// Encoded frames, one JSON object each, without the trailing newline.
    pub frames: Receiver<String>,
    /// Set when the hub dropped this subscriber for falling behind.
    pub lagged: Arc<AtomicBool>,
}

struct Subscriber {
    tx: SyncSender<String>,
    filter: Option<Filter>,
    /// The last `seq` sent. Per subscription, so a filtered stream is as
    /// gap-free as an unfiltered one.
    seq: u64,
    lagged: Arc<AtomicBool>,
    awaiting_snapshot: bool,
}

/// Who a queued event is for.
enum Audience {
    /// Every subscriber: the limits are not about any one session.
    All,
    /// Subscribers whose filter matches this session.
    Session(Box<SessionEntry>),
    /// Only subscribers with no filter: a refusal from a caller Allele has no
    /// record of cannot be matched against anything.
    Unfiltered,
}

#[derive(Default)]
struct Inner {
    subscribers: Vec<Subscriber>,
    queries: Vec<Sender<Arc<View>>>,
    /// The view the last deltas were computed against.
    last: Option<Vec<SessionEntry>>,
    last_limits: Option<DispatchLimits>,
    last_diff: Option<Instant>,
    /// Ids removed through `sessions_discard`, awaiting the diff that sees
    /// them go.
    discarded: HashSet<String>,
    /// Refusals waiting for the next publish.
    refused: Vec<DispatchRefused>,
    /// Refusals per caller this epoch, for `DispatchRefused::count`.
    refusal_counts: HashMap<String, u64>,
}

pub(crate) struct Hub {
    epoch: String,
    inner: Mutex<Inner>,
}

static GLOBAL: LazyLock<Hub> = LazyLock::new(Hub::new);

impl Hub {
    pub(crate) fn new() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The hub `control.sock` serves. One per process, so the epoch is the
    /// process's lifetime.
    pub(crate) fn global() -> &'static Hub {
        &GLOBAL
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    /// A poisoned lock means a panic mid-publish. The state it guards is a
    /// set of queues and a cache, all safe to keep using, and the UI thread
    /// must not panic over a socket feature.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a subscriber. Its snapshot is queued at the next publish, on
    /// the foreground thread, so snapshot and deltas are cut from the same
    /// sequence of views and nothing falls between them.
    pub(crate) fn subscribe(&self, filter: Option<Filter>) -> Subscription {
        let (tx, frames) = sync_channel(QUEUE_CAPACITY);
        let lagged = Arc::new(AtomicBool::new(false));
        self.lock().subscribers.push(Subscriber {
            tx,
            filter,
            seq: 0,
            lagged: lagged.clone(),
            awaiting_snapshot: true,
        });
        Subscription { frames, lagged }
    }

    /// Ask for the next view, answered at the next drain tick.
    pub(crate) fn query(&self) -> Receiver<Arc<View>> {
        let (tx, rx) = channel();
        self.lock().queries.push(tx);
        rx
    }

    /// Whether the foreground should build a view now, and if so whether it
    /// must include archived sessions. `None` costs nothing further.
    pub(crate) fn wants_view(&self, now: Instant) -> Option<bool> {
        let inner = self.lock();
        if !inner.queries.is_empty() || inner.subscribers.iter().any(|s| s.awaiting_snapshot) {
            return Some(!inner.queries.is_empty());
        }
        if inner.subscribers.is_empty() {
            return None;
        }
        let due = inner
            .last_diff
            .is_none_or(|at| now.saturating_duration_since(at) >= DIFF_INTERVAL);
        (due || !inner.refused.is_empty()).then_some(false)
    }

    /// Record that `sessions_discard` is removing `session_id`, so the diff
    /// that sees it go reports `discarded`.
    pub(crate) fn note_discarded(&self, session_id: &str) {
        let mut inner = self.lock();
        if !inner.subscribers.is_empty() {
            inner.discarded.insert(session_id.to_string());
        }
    }

    /// Record a `sessions_create` refused by a dispatch limit.
    pub(crate) fn dispatch_refused(
        &self,
        caller: Option<String>,
        code: ErrorCode,
        depth: u8,
        limit: u64,
    ) {
        let mut inner = self.lock();
        let key = caller.clone().unwrap_or_default();
        let count = inner.refusal_counts.entry(key).or_insert(0);
        *count += 1;
        let count = *count;
        if inner.subscribers.is_empty() {
            return; // counted, but there is nobody to tell
        }
        inner.refused.push(DispatchRefused {
            caller,
            code: Some(code),
            depth,
            limit,
            at: unix_secs(SystemTime::now()),
            count,
        });
    }

    /// Fold a new view in: deltas to existing subscribers, snapshots to new
    /// ones, the view itself to waiting queries. Never blocks.
    pub(crate) fn publish(&self, view: View, now: Instant) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.last_diff = Some(now);

        let events = changes(inner, &view);
        let parents = parent_map(inner.last.as_deref().unwrap_or_default(), &view.sessions);

        inner.subscribers.retain_mut(|sub| {
            if sub.awaiting_snapshot {
                sub.awaiting_snapshot = false;
                let sessions = view
                    .sessions
                    .iter()
                    .filter(|e| matches(sub.filter.as_ref(), e, &parents))
                    .cloned()
                    .collect();
                return send(sub, &Frame::Snapshot { seq: 0, sessions });
            }
            for (event, audience) in &events {
                let wanted = match audience {
                    Audience::All => true,
                    Audience::Session(entry) => matches(sub.filter.as_ref(), entry, &parents),
                    Audience::Unfiltered => is_open(sub.filter.as_ref()),
                };
                if !wanted {
                    continue;
                }
                let frame = Frame::Delta {
                    seq: sub.seq + 1,
                    event: event.clone(),
                };
                if !send(sub, &frame) {
                    return false;
                }
                sub.seq += 1;
            }
            true
        });

        let view = Arc::new(view);
        for query in inner.queries.drain(..) {
            let _ = query.send(view.clone()); // a caller that gave up is fine
        }
        inner.last = Some(view.sessions.clone());
        inner.last_limits = Some(view.limits);
    }

    #[cfg(test)]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.lock().subscribers.len()
    }
}

/// Everything that changed between the last view and `view`, in the order a
/// subscriber should hear it.
fn changes(inner: &mut Inner, view: &View) -> Vec<(Event, Audience)> {
    let mut events = Vec::new();

    if let Some(last) = &inner.last {
        let before: HashMap<&str, &SessionEntry> =
            last.iter().map(|e| (e.session_id.as_str(), e)).collect();
        let now: HashSet<&str> = view
            .sessions
            .iter()
            .map(|e| e.session_id.as_str())
            .collect();

        for entry in &view.sessions {
            let event = match before.get(entry.session_id.as_str()) {
                None => Event::SessionAdded {
                    session: Box::new(entry.clone()),
                },
                Some(old) if !same_but_for_age(old, entry) => Event::SessionStatusChanged {
                    session: Box::new(entry.clone()),
                },
                Some(_) => continue,
            };
            events.push((event, Audience::Session(Box::new(entry.clone()))));
        }
        for old in last.iter().filter(|e| !now.contains(e.session_id.as_str())) {
            let reason = if inner.discarded.remove(&old.session_id) {
                RemovalReason::Discarded
            } else if view.archived_ids.contains(&old.session_id) {
                RemovalReason::Archived
            } else {
                RemovalReason::Closed
            };
            events.push((
                Event::SessionRemoved {
                    session_id: old.session_id.clone(),
                    reason,
                },
                Audience::Session(Box::new(old.clone())),
            ));
        }
    }

    if inner.last_limits.is_some_and(|l| l != view.limits) {
        events.push((
            Event::LimitsChanged {
                dispatch_limits: view.limits,
            },
            Audience::All,
        ));
    }

    for refused in inner.refused.drain(..) {
        let caller = refused
            .caller
            .as_deref()
            .and_then(|id| view.sessions.iter().find(|e| e.session_id == id));
        let audience = match caller {
            Some(entry) => Audience::Session(Box::new(entry.clone())),
            None => Audience::Unfiltered,
        };
        events.push((Event::DispatchRefused(refused), audience));
    }
    events
}

/// `state_age_secs` ticks every second on its own; a delta for that alone
/// would be a frame per session per second saying nothing.
fn same_but_for_age(a: &SessionEntry, b: &SessionEntry) -> bool {
    let mut b = b.clone();
    b.state_age_secs = a.state_age_secs;
    *a == b
}

/// Encode and queue a frame. `false` drops the subscriber: either it is gone,
/// or its queue is full, which marks it lagged.
fn send(sub: &Subscriber, frame: &Frame) -> bool {
    let line = match serde_json::to_string(frame) {
        Ok(line) => line,
        Err(e) => {
            warn!("ext/1: could not encode a frame: {e}");
            return true;
        }
    };
    match sub.tx.try_send(line) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            sub.lagged.store(true, Ordering::SeqCst);
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Session id → parent id, across both views, so a removed session's
/// ancestry still resolves for the event that reports its removal.
fn parent_map(before: &[SessionEntry], after: &[SessionEntry]) -> HashMap<String, Option<String>> {
    before
        .iter()
        .chain(after)
        .map(|e| (e.session_id.clone(), e.parent.clone()))
        .collect()
}

/// A filter that selects everything: absent, or with every field absent.
fn is_open(filter: Option<&Filter>) -> bool {
    filter
        .is_none_or(|f| f.descendants_of.is_none() && f.group.is_none() && f.session_ids.is_none())
}

/// Deepest `parent` chain walked. Dispatch depth is capped far below this;
/// the bound exists so a corrupt cycle cannot spin the foreground.
const MAX_ANCESTRY: usize = 64;

/// Whether `entry` passes `filter`. The fields are OR-combined.
pub(crate) fn matches(
    filter: Option<&Filter>,
    entry: &SessionEntry,
    parents: &HashMap<String, Option<String>>,
) -> bool {
    let Some(f) = filter else { return true };
    if is_open(filter) {
        return true;
    }
    if f.session_ids
        .as_ref()
        .is_some_and(|ids| ids.contains(&entry.session_id))
    {
        return true;
    }
    if let (Some(want), Some(have)) = (&f.group, &entry.group) {
        if want.trim().to_lowercase() == have.to_lowercase() {
            return true;
        }
    }
    if let Some(root) = &f.descendants_of {
        let mut cursor = entry.parent.clone();
        for _ in 0..MAX_ANCESTRY {
            match cursor {
                Some(ref id) if id == root => return true,
                Some(id) => cursor = parents.get(&id).cloned().flatten(),
                None => break,
            }
        }
    }
    false
}

pub(crate) fn unix_secs(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use allele_contract::State;

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            session_id: id.into(),
            name: format!("label {id}"),
            project: "p".into(),
            state: State::Idle,
            ..SessionEntry::default()
        }
    }

    fn view(sessions: Vec<SessionEntry>) -> View {
        View {
            sessions,
            ..View::default()
        }
    }

    fn frames(sub: &Subscription) -> Vec<Frame> {
        sub.frames
            .try_iter()
            .map(|line| serde_json::from_str(&line).expect("a frame"))
            .collect()
    }

    fn deltas(sub: &Subscription) -> Vec<(u64, Event)> {
        frames(sub)
            .into_iter()
            .filter_map(|f| match f {
                Frame::Delta { seq, event } => Some((seq, event)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_first_frame_is_a_snapshot_at_seq_zero() {
        let hub = Hub::new();
        let sub = hub.subscribe(None);
        hub.publish(view(vec![entry("a"), entry("b")]), Instant::now());
        match frames(&sub).as_slice() {
            [Frame::Snapshot { seq: 0, sessions }] => assert_eq!(sessions.len(), 2),
            other => panic!("got {other:?}"),
        }
    }

    /// AC: a subscriber's `seq` values are strictly consecutive across 1,000
    /// synthetic transitions.
    #[test]
    fn seq_is_strictly_consecutive_across_a_thousand_transitions() {
        let hub = Hub::new();
        let sub = hub.subscribe(None);
        hub.publish(view(vec![entry("a"), entry("b")]), Instant::now());
        let mut seen = vec![0u64];
        for i in 0..1000u64 {
            let mut a = entry("a");
            a.state = if i % 2 == 0 {
                State::Running
            } else {
                State::Idle
            };
            a.turn = i / 2 + 1;
            hub.publish(view(vec![a, entry("b")]), Instant::now());
            // Read as we go, like a live client.
            seen.extend(deltas(&sub).into_iter().map(|(seq, _)| seq));
        }
        assert_eq!(seen.len(), 1001);
        assert!(
            seen.windows(2).all(|w| w[1] == w[0] + 1),
            "seq skipped or repeated: {seen:?}"
        );
    }

    /// A filtered subscriber skips other sessions' events without a gap in
    /// its own `seq`, which is why the counter is per subscription.
    #[test]
    fn a_filtered_stream_has_no_seq_gaps() {
        let hub = Hub::new();
        let filter = Filter {
            session_ids: Some(vec!["b".into()]),
            ..Filter::default()
        };
        let sub = hub.subscribe(Some(filter));
        hub.publish(view(vec![entry("a"), entry("b")]), Instant::now());
        for state in [State::Running, State::Idle, State::Running] {
            let mut a = entry("a");
            a.state = state;
            let mut b = entry("b");
            b.state = state;
            hub.publish(view(vec![a, b]), Instant::now());
        }
        let seqs: Vec<u64> = deltas(&sub).into_iter().map(|(s, _)| s).collect();
        assert_eq!(seqs, [1, 2, 3]);
    }

    /// `turn` moving with `state` unchanged is still a status change: a turn
    /// that ran and finished between two looks must not be invisible.
    #[test]
    fn a_turn_alone_is_a_status_change_but_age_alone_is_not() {
        let hub = Hub::new();
        let sub = hub.subscribe(None);
        hub.publish(view(vec![entry("a")]), Instant::now());

        let mut aged = entry("a");
        aged.state_age_secs = 30;
        hub.publish(view(vec![aged.clone()]), Instant::now());
        assert!(deltas(&sub).is_empty(), "age alone must not send a delta");

        let mut turned = aged;
        turned.turn = 1;
        hub.publish(view(vec![turned]), Instant::now());
        assert!(matches!(
            deltas(&sub).as_slice(),
            [(1, Event::SessionStatusChanged { session })] if session.turn == 1
        ));
    }

    #[test]
    fn added_and_removed_sessions_are_reported_with_a_reason() {
        let hub = Hub::new();
        let sub = hub.subscribe(None);
        hub.publish(
            view(vec![entry("a"), entry("b"), entry("c")]),
            Instant::now(),
        );

        hub.note_discarded("a");
        let mut next = view(vec![entry("d")]);
        next.archived_ids.insert("b".into());
        hub.publish(next, Instant::now());

        let got: Vec<Event> = deltas(&sub).into_iter().map(|(_, e)| e).collect();
        assert!(matches!(&got[0], Event::SessionAdded { session } if session.session_id == "d"));
        let reasons: Vec<(&str, RemovalReason)> = got[1..]
            .iter()
            .map(|e| match e {
                Event::SessionRemoved { session_id, reason } => (session_id.as_str(), *reason),
                other => panic!("got {other:?}"),
            })
            .collect();
        assert_eq!(
            reasons,
            [
                ("a", RemovalReason::Discarded),
                ("b", RemovalReason::Archived),
                ("c", RemovalReason::Closed)
            ]
        );
    }

    #[test]
    fn limits_changing_reaches_every_subscriber() {
        let hub = Hub::new();
        let filtered = hub.subscribe(Some(Filter {
            group: Some("review".into()),
            ..Filter::default()
        }));
        hub.publish(view(vec![]), Instant::now());
        let mut next = view(vec![]);
        next.limits.live_dispatched = 1;
        hub.publish(next, Instant::now());
        assert!(matches!(
            deltas(&filtered).as_slice(),
            [(1, Event::LimitsChanged { dispatch_limits })] if dispatch_limits.live_dispatched == 1
        ));
    }

    #[test]
    fn a_refusal_is_counted_per_caller_and_follows_the_filter() {
        let hub = Hub::new();
        let mut lead = entry("lead");
        lead.group = Some("review".into());
        let reviewers = hub.subscribe(Some(Filter {
            group: Some("review".into()),
            ..Filter::default()
        }));
        let everyone = hub.subscribe(None);
        hub.publish(view(vec![lead.clone()]), Instant::now());

        hub.dispatch_refused(Some("lead".into()), ErrorCode::DepthLimitExceeded, 2, 1);
        hub.dispatch_refused(Some("lead".into()), ErrorCode::DepthLimitExceeded, 2, 1);
        hub.dispatch_refused(None, ErrorCode::CapacityExceeded, 1, 20);
        assert_eq!(
            hub.wants_view(Instant::now()),
            Some(false),
            "refusals are due now"
        );
        hub.publish(view(vec![lead]), Instant::now());

        let counts = |sub: &Subscription| -> Vec<(Option<String>, u64)> {
            deltas(sub)
                .into_iter()
                .map(|(_, e)| match e {
                    Event::DispatchRefused(r) => (r.caller, r.count),
                    other => panic!("got {other:?}"),
                })
                .collect()
        };
        assert_eq!(
            counts(&reviewers),
            [(Some("lead".into()), 1), (Some("lead".into()), 2)],
            "an unknown caller cannot match a filter"
        );
        assert_eq!(counts(&everyone).len(), 3);
    }

    /// AC: `descendants_of: L` matches a grandchild of L through `parent`.
    #[test]
    fn descendants_of_matches_a_grandchild_through_the_parent_chain() {
        let lead = entry("L");
        let mut child = entry("C");
        child.parent = Some("L".into());
        let mut grandchild = entry("G");
        grandchild.parent = Some("C".into());
        let stranger = entry("S");
        let all = [
            lead.clone(),
            child.clone(),
            grandchild.clone(),
            stranger.clone(),
        ];
        let parents = parent_map(&[], &all);
        let filter = Filter {
            descendants_of: Some("L".into()),
            ..Filter::default()
        };
        assert!(matches(Some(&filter), &grandchild, &parents));
        assert!(matches(Some(&filter), &child, &parents));
        assert!(!matches(Some(&filter), &lead, &parents), "not L itself");
        assert!(!matches(Some(&filter), &stranger, &parents));
    }

    #[test]
    fn filter_fields_are_or_combined_and_groups_ignore_case() {
        let mut reviewer = entry("r");
        reviewer.group = Some("Review".into());
        let named = entry("n");
        let other = entry("o");
        let filter = Filter {
            group: Some("review".into()),
            session_ids: Some(vec!["n".into()]),
            ..Filter::default()
        };
        let parents = HashMap::new();
        assert!(matches(Some(&filter), &reviewer, &parents));
        assert!(matches(Some(&filter), &named, &parents));
        assert!(!matches(Some(&filter), &other, &parents));
        assert!(
            matches(Some(&Filter::default()), &other, &parents),
            "empty filter is open"
        );
    }

    /// A parent cycle cannot happen through dispatch, but a corrupt record
    /// must not spin the UI thread.
    #[test]
    fn a_parent_cycle_terminates() {
        let mut a = entry("a");
        a.parent = Some("b".into());
        let mut b = entry("b");
        b.parent = Some("a".into());
        let parents = parent_map(&[], &[a.clone(), b]);
        let filter = Filter {
            descendants_of: Some("z".into()),
            ..Filter::default()
        };
        assert!(!matches(Some(&filter), &a, &parents));
    }

    /// AC: a reader that never reads is dropped as lagged, and publishing
    /// never waits on it. Each publish is a `try_send` per subscriber; this
    /// bounds the whole run, stalled reader included, well inside one frame.
    #[test]
    fn a_reader_that_never_reads_is_dropped_without_blocking_publish() {
        let hub = Hub::new();
        let stalled = hub.subscribe(None);
        let mut slowest = Duration::ZERO;
        for i in 0..(QUEUE_CAPACITY as u64 + 50) {
            let mut a = entry("a");
            a.turn = i;
            let started = Instant::now();
            hub.publish(view(vec![a]), Instant::now());
            slowest = slowest.max(started.elapsed());
        }
        assert!(stalled.lagged.load(Ordering::SeqCst), "marked lagged");
        assert_eq!(hub.subscriber_count(), 0, "and dropped");
        assert!(
            slowest < Duration::from_millis(16),
            "a publish took {slowest:?}, longer than a 60 Hz frame"
        );
    }

    #[test]
    fn nothing_is_built_while_nobody_listens() {
        let hub = Hub::new();
        assert_eq!(hub.wants_view(Instant::now()), None);
        let _query = hub.query();
        assert_eq!(
            hub.wants_view(Instant::now()),
            Some(true),
            "a query wants archives"
        );
    }

    #[test]
    fn a_subscriber_is_diffed_at_most_every_interval() {
        let hub = Hub::new();
        let _sub = hub.subscribe(None);
        let t0 = Instant::now();
        assert_eq!(hub.wants_view(t0), Some(false), "snapshot is due at once");
        hub.publish(view(vec![]), t0);
        assert_eq!(hub.wants_view(t0 + Duration::from_millis(50)), None);
        assert_eq!(hub.wants_view(t0 + DIFF_INTERVAL), Some(false));
    }
}
