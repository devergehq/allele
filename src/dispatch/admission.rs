//! Whether a dispatch request is allowed to start a session (DEV-415).
//!
//! Kept pure and separate from the socket so the two limits can be tested
//! without a running app, and so neither is buried in I/O code where a later
//! refactor might quietly drop one.
//!
//! Two limits that bound different things and do not subsume each other:
//!
//! - **The cap** bounds *breadth* — how many dispatched sessions exist at
//!   once, aggregated globally rather than per-dispatcher. Per-dispatcher is
//!   the wrong shape: twenty dispatchers each under their own limit of twenty
//!   is four hundred sessions, every one individually compliant.
//! - **The depth limit** bounds *recursion*. It matters because a dispatched
//!   session runs the same orchestration rules its creator did — "dispatch
//!   when the work is parallelisable" fires in every child, so unbounded
//!   recursion is the rule working correctly rather than a misuse.
//!
//! ## What this does not bound
//!
//! The depth limit bounds **dispatch** recursion completely and **total**
//! recursion not at all. A dispatched session still has a shell, and tooling
//! it invokes there (`locus delegate run`, for one) spawns *processes* rather
//! than sessions — allele never sees them and they never reach the sidebar.
//! Enforcement for that belongs to the tool that spawns them; see DEV-419.
//! Do not describe this module as bounding "the fleet".
//!
//! ## Who sets the limits
//!
//! Both limits are read from `settings.json` (DEV-600) — see
//! [`DispatchLimits`]. The defaults are the protection: a user who has never
//! heard of the setting gets a depth of 1 and a cap of 20. Raising them is an
//! opt-in by whoever owns the machine. The request carries no limit fields,
//! so nothing a session sends over the socket can move them.
//!
//! That keeps the limits in the same class as the rest of this module: they
//! bound the *accident* — a session recursing because its own rules told it
//! to — and are not a security control. A session with a shell runs as the
//! machine's owner and could edit `settings.json` as readily as anything
//! else; see `CreateRequest::caller_session_id` and DEV-419.

use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::dispatch::protocol::ErrorCode;
use crate::session::SessionOrigin;

/// Default cap on dispatched sessions that exist at once. See
/// [`DispatchLimits::max_sessions`].
pub const DEFAULT_MAX_DISPATCHED_SESSIONS: usize = 20;

/// Default dispatch depth. `1` means a human's session may dispatch, and the
/// sessions it dispatches may not. See [`DispatchLimits::max_depth`].
pub const DEFAULT_MAX_DISPATCH_DEPTH: u8 = 1;

/// The two admission limits, as configured under `"dispatch"` in
/// `settings.json`. Either field may be omitted and keeps its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DispatchLimits {
    /// How deep dispatch may nest — the deepest `SessionOrigin::depth` a
    /// dispatched session may be created at. `1` (the default) means a
    /// human's session may dispatch and its workers may not; `2` lets those
    /// workers dispatch a helper of their own, which may not; `0` turns
    /// dispatch off.
    ///
    /// The default is 1 because orchestration is a capability, not a role:
    /// every dispatched session runs the same "dispatch when parallelisable"
    /// rule its creator did, so each level allowed multiplies the fleet.
    /// Allowing more is a choice the machine's owner makes deliberately, not
    /// something a default should do for everyone.
    pub max_depth: u8,
    /// Maximum dispatched sessions that exist at once, counted across all
    /// dispatchers at every depth.
    ///
    /// Human-started sessions are deliberately uncapped: a human can see what
    /// they are doing, and an orchestrator in a loop cannot. When raising
    /// `max_depth`, raise this with it — the cap is what still bounds the
    /// fleet once more than one level may dispatch.
    pub max_sessions: usize,
}

impl Default for DispatchLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DISPATCH_DEPTH,
            max_sessions: DEFAULT_MAX_DISPATCHED_SESSIONS,
        }
    }
}

/// Decide whether `creator` may dispatch under `limits`, given how many
/// dispatched sessions already exist. Returns the **child's** depth on
/// success.
///
/// The returned depth is the only correct source for the new session's
/// `SessionOrigin::depth`: it is computed here from the creator's own record,
/// never taken from the request.
pub fn admit(
    creator: &SessionOrigin,
    live_dispatched: usize,
    limits: &DispatchLimits,
) -> Result<u8, ErrorCode> {
    // `checked_add`, not `saturating_add`: once the limit is configurable a
    // limit of 255 is reachable, and saturating would hand a depth-255
    // creator a depth-255 child — admitted again, and again, forever.
    let child_depth = creator
        .depth()
        .checked_add(1)
        .filter(|&depth| depth <= limits.max_depth)
        .ok_or(ErrorCode::DepthLimitExceeded)?;
    // Checked after depth so a recursion attempt is reported as recursion
    // rather than as a capacity problem — the two want different responses
    // from the caller, and "capacity" invites a retry that will never work.
    if live_dispatched >= limits.max_sessions {
        return Err(ErrorCode::CapacityExceeded);
    }
    Ok(child_depth)
}

/// How many dispatched sessions exist, across every project.
///
/// Counts dispatched sessions only — human-started ones are uncapped, because
/// a human can see what they are doing and an orchestrator in a loop cannot.
/// Aggregated globally rather than per dispatcher; see [`super::admission`].
///
/// "Exist" is literal: a suspended or finished worker holds its slot until it
/// is discarded, which is what `sessions.discard` is for. A create still
/// provisioning its workspace does **not** yet count — its origin is parked in
/// `pending_dispatch_origins` until the clone lands — so a burst of creates
/// can overshoot the cap by the number in flight.
pub fn live_dispatched_count(state: &AppState) -> usize {
    state
        .projects
        .iter()
        .flat_map(|p| p.sessions.iter())
        .filter(|s| s.origin.is_dispatched())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatched(depth: u8) -> SessionOrigin {
        SessionOrigin::Dispatched {
            by_session_id: "s".into(),
            by_label: "S".into(),
            depth,
        }
    }

    fn defaults() -> DispatchLimits {
        DispatchLimits::default()
    }

    /// The opt-in this setting exists for: a human's dispatcher, its workers,
    /// and one helper each.
    fn depth_two() -> DispatchLimits {
        DispatchLimits {
            max_depth: 2,
            max_sessions: 30,
        }
    }

    /// The defaults are the fork-bomb protection everyone gets without
    /// asking, so pin them — raising either should be a deliberate edit here,
    /// not a side effect.
    #[test]
    fn the_defaults_are_depth_one_and_twenty_sessions() {
        assert_eq!(
            defaults(),
            DispatchLimits {
                max_depth: 1,
                max_sessions: 20,
            }
        );
    }

    #[test]
    fn a_humans_session_may_dispatch() {
        assert_eq!(admit(&SessionOrigin::Human, 0, &defaults()), Ok(1));
    }

    /// The fork bomb: under the defaults, a dispatched session running the
    /// same "dispatch when parallelisable" rule its creator ran must not be
    /// able to recurse.
    #[test]
    fn a_dispatched_session_may_not_dispatch_by_default() {
        assert_eq!(
            admit(&dispatched(1), 0, &defaults()),
            Err(ErrorCode::DepthLimitExceeded)
        );
    }

    /// Opting in to depth 2 lets a worker dispatch a helper, and the helper
    /// is recorded one level deeper than the worker.
    #[test]
    fn at_depth_two_a_worker_may_dispatch_a_helper() {
        assert_eq!(admit(&SessionOrigin::Human, 0, &depth_two()), Ok(1));
        assert_eq!(admit(&dispatched(1), 0, &depth_two()), Ok(2));
    }

    /// ...and the helper may not, so raising the limit moves the floor down
    /// by exactly one level rather than removing it.
    #[test]
    fn at_depth_two_a_helper_may_not_dispatch() {
        assert_eq!(
            admit(&dispatched(2), 0, &depth_two()),
            Err(ErrorCode::DepthLimitExceeded)
        );
    }

    /// `0` is a way to turn dispatch off, including for a human's session.
    #[test]
    fn depth_zero_turns_dispatch_off() {
        let off = DispatchLimits {
            max_depth: 0,
            ..defaults()
        };
        assert_eq!(
            admit(&SessionOrigin::Human, 0, &off),
            Err(ErrorCode::DepthLimitExceeded)
        );
    }

    /// The largest limit a `u8` can express must still end somewhere. With
    /// `saturating_add` a depth-255 creator would get a depth-255 child that
    /// passes the same check, forever.
    #[test]
    fn the_largest_depth_limit_still_terminates() {
        let max = DispatchLimits {
            max_depth: u8::MAX,
            ..defaults()
        };
        assert_eq!(admit(&dispatched(254), 0, &max), Ok(255));
        assert_eq!(
            admit(&dispatched(255), 0, &max),
            Err(ErrorCode::DepthLimitExceeded)
        );
    }

    #[test]
    fn cap_is_global_not_per_dispatcher() {
        assert_eq!(admit(&SessionOrigin::Human, 19, &defaults()), Ok(1));
        assert_eq!(
            admit(&SessionOrigin::Human, 20, &defaults()),
            Err(ErrorCode::CapacityExceeded)
        );
        // A different creator sees the same full pool — the count is not
        // scoped to who is asking.
        assert_eq!(
            admit(&SessionOrigin::Human, 25, &defaults()),
            Err(ErrorCode::CapacityExceeded)
        );
    }

    /// A configured cap is honoured at its own boundary, and it binds a
    /// worker's dispatch exactly as it binds a human's.
    #[test]
    fn a_configured_cap_is_honoured() {
        assert_eq!(admit(&SessionOrigin::Human, 29, &depth_two()), Ok(1));
        assert_eq!(admit(&dispatched(1), 29, &depth_two()), Ok(2));
        assert_eq!(
            admit(&SessionOrigin::Human, 30, &depth_two()),
            Err(ErrorCode::CapacityExceeded)
        );
        assert_eq!(
            admit(&dispatched(1), 30, &depth_two()),
            Err(ErrorCode::CapacityExceeded)
        );
    }

    /// Depth outranks capacity: a recursion attempt with room to spare is
    /// still recursion, and telling the caller "capacity" would invite a
    /// retry that can never succeed.
    #[test]
    fn depth_is_reported_before_capacity() {
        assert_eq!(
            admit(&dispatched(1), 999, &defaults()),
            Err(ErrorCode::DepthLimitExceeded)
        );
        assert_eq!(
            admit(&dispatched(2), 999, &depth_two()),
            Err(ErrorCode::DepthLimitExceeded)
        );
    }

    /// Depth comes from the creator's record, so it cannot be reset by a
    /// caller claiming to be shallower than it is.
    #[test]
    fn child_depth_is_derived_from_the_creator() {
        assert_eq!(admit(&SessionOrigin::Human, 0, &defaults()), Ok(1));
        assert!(admit(&dispatched(200), 0, &defaults()).is_err());
        assert!(admit(&dispatched(200), 0, &depth_two()).is_err());
    }
}
