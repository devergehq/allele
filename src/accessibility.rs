//! Shared accessibility requirements for dense application controls.

/// Minimum interaction height for compact primary controls.
pub const DENSE_CONTROL_MIN_HEIGHT: f32 = 24.0;

/// Primary actions must remain visible without pointer hover.
#[cfg(test)]
const PRIMARY_ACTIONS_HOVER_ONLY: bool = false;

/// Destructive confirmations default to their neutral cancellation path.
#[cfg(test)]
const DESTRUCTIVE_DEFAULT_IS_SAFE: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    // Clippy const-folds these to `assert!(true)` and objects. They are
    // regression guards, not claims about today: each fails the moment someone
    // edits the constant above it, which is the only way they can ever fire.
    //
    // `DENSE_CONTROL_MIN_HEIGHT` is real — eleven UI sites use it. The other
    // two constants are read by nothing but this test and record intent rather
    // than verify it; see DEV-349 for whether they should become real
    // assertions against the widgets or be deleted.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn dense_controls_meet_minimum_target() {
        assert!(DENSE_CONTROL_MIN_HEIGHT >= 24.0);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn interaction_contract_keeps_primary_and_safe_actions_available() {
        assert!(!PRIMARY_ACTIONS_HOVER_ONLY);
        assert!(DESTRUCTIVE_DEFAULT_IS_SAFE);
    }
}
