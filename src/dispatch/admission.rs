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

// The caller lands with the socket listener; the policy is testable now.
#![allow(dead_code)]

use crate::app_state::AppState;
use crate::dispatch::protocol::ErrorCode;
use crate::session::SessionOrigin;

/// Maximum dispatched sessions alive at once, counted across all dispatchers.
///
/// Human-started sessions are deliberately uncapped: a human can see what
/// they are doing, and an orchestrator in a loop cannot.
pub const MAX_DISPATCHED_SESSIONS: usize = 20;

/// How deep dispatch may nest. `1` means a human's session may dispatch, and
/// the sessions it dispatches may not.
///
/// Orchestration is a capability, not a role — so the ability to dispatch
/// stays with the session a human started.
pub const MAX_DISPATCH_DEPTH: u8 = 1;

/// Decide whether `creator` may dispatch, given how many dispatched sessions
/// are already alive. Returns the **child's** depth on success.
///
/// The returned depth is the only correct source for the new session's
/// `SessionOrigin::depth`: it is computed here from the creator's own record,
/// never taken from the request.
pub fn admit(creator: &SessionOrigin, live_dispatched: usize) -> Result<u8, ErrorCode> {
    let child_depth = creator.depth().saturating_add(1);
    if child_depth > MAX_DISPATCH_DEPTH {
        return Err(ErrorCode::DepthLimitExceeded);
    }
    // Checked after depth so a recursion attempt is reported as recursion
    // rather than as a capacity problem — the two want different responses
    // from the caller, and "capacity" invites a retry that will never work.
    if live_dispatched >= MAX_DISPATCHED_SESSIONS {
        return Err(ErrorCode::CapacityExceeded);
    }
    Ok(child_depth)
}

/// How many dispatched sessions are alive, across every project.
///
/// Counts dispatched sessions only — human-started ones are uncapped, because
/// a human can see what they are doing and an orchestrator in a loop cannot.
/// Aggregated globally rather than per dispatcher; see [`super::admission`].
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

    #[test]
    fn a_humans_session_may_dispatch() {
        assert_eq!(admit(&SessionOrigin::Human, 0), Ok(1));
    }

    /// The fork bomb: a dispatched session running the same "dispatch when
    /// parallelisable" rule its creator ran must not be able to recurse.
    #[test]
    fn a_dispatched_session_may_not_dispatch() {
        assert_eq!(admit(&dispatched(1), 0), Err(ErrorCode::DepthLimitExceeded));
    }

    #[test]
    fn cap_is_global_not_per_dispatcher() {
        assert_eq!(admit(&SessionOrigin::Human, 19), Ok(1));
        assert_eq!(
            admit(&SessionOrigin::Human, 20),
            Err(ErrorCode::CapacityExceeded)
        );
        // A different creator sees the same full pool — the count is not
        // scoped to who is asking.
        assert_eq!(
            admit(&SessionOrigin::Human, 25),
            Err(ErrorCode::CapacityExceeded)
        );
    }

    /// Depth outranks capacity: a recursion attempt with room to spare is
    /// still recursion, and telling the caller "capacity" would invite a
    /// retry that can never succeed.
    #[test]
    fn depth_is_reported_before_capacity() {
        assert_eq!(
            admit(&dispatched(1), 999),
            Err(ErrorCode::DepthLimitExceeded)
        );
    }

    /// Depth comes from the creator's record, so it cannot be reset by a
    /// caller claiming to be shallower than it is.
    #[test]
    fn child_depth_is_derived_from_the_creator() {
        assert_eq!(admit(&SessionOrigin::Human, 0), Ok(1));
        assert!(admit(&dispatched(200), 0).is_err());
    }
}
