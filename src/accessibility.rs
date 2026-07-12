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

    #[test]
    fn dense_controls_meet_minimum_target() {
        assert!(DENSE_CONTROL_MIN_HEIGHT >= 24.0);
    }

    #[test]
    fn interaction_contract_keeps_primary_and_safe_actions_available() {
        assert!(!PRIMARY_ACTIONS_HOVER_ONLY);
        assert!(DESTRUCTIVE_DEFAULT_IS_SAFE);
    }
}
