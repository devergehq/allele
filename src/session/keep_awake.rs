//! Keep the Mac awake while sessions are working (DEV-1156).
//!
//! When the host idles into system sleep every running agent stops, which is
//! fatal to leaving twenty sessions running over lunch. While any session is
//! [`Running`](SessionStatus::Running) or
//! [`AwaitingInput`](SessionStatus::AwaitingInput), Allele holds a power
//! assertion that prevents idle *system* sleep — the display still sleeps.
//!
//! The decision is a pure function of the setting and the session statuses
//! ([`should_keep_awake`]); the assertion itself lives behind
//! [`crate::platform::PowerManagement`], which is idempotent, so reconciling is
//! just "say what we want".
//!
//! Reconciling runs from its own poller, not from the render tick: GPUI does
//! not draw a hidden or minimised window, and a session dispatched over the
//! control socket while the window is hidden must still keep the Mac awake.

use std::time::Duration;

use gpui::Context;

use super::SessionStatus;
use crate::app_state::AppState;

/// How often the assertion is reconciled with session state. The ticket
/// allows five seconds for the release; one second leaves ample margin and
/// costs one walk of the session list.
pub(crate) const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// True when a session in this state is doing work the Mac must stay awake
/// for. `AwaitingInput` counts: the agent is alive and blocked on a prompt
/// the user may answer from another machine. `ResponseReady` does not — the
/// turn is finished and nothing is running.
pub(crate) fn status_keeps_awake(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Running | SessionStatus::AwaitingInput
    )
}

/// Whether the idle-sleep assertion should be held right now.
pub(crate) fn should_keep_awake(
    enabled: bool,
    statuses: impl IntoIterator<Item = SessionStatus>,
) -> bool {
    enabled && statuses.into_iter().any(status_keeps_awake)
}

impl AppState {
    /// Bring the power assertion in line with the setting and the sessions.
    /// Cheap and idempotent; safe to call every tick.
    pub(crate) fn reconcile_keep_awake(&self) {
        let want = should_keep_awake(
            self.user_settings.keep_awake_while_working,
            self.projects
                .iter()
                .flat_map(|p| p.sessions.iter())
                .map(|s| s.status),
        );
        self.set_keep_awake(want);
    }

    fn set_keep_awake(&self, want: bool) {
        if let Err(e) = self.platform.power.set_prevent_idle_sleep(want) {
            tracing::warn!("keep-awake: could not set idle-sleep assertion to {want}: {e}");
        }
    }
}

/// Start the reconcile poller and release the assertion on quit. Called once,
/// from [`crate::startup::pollers::spawn_all`].
pub(crate) fn spawn(cx: &mut Context<AppState>) {
    // Every quit path (menu, ⌘Q, the quit confirmation, the control socket)
    // ends in `cx.quit()`, which runs these. The kernel also drops a dead
    // process's assertions, so a crash cannot pin the Mac awake either.
    cx.on_app_quit(|this: &mut AppState, _cx| {
        this.set_keep_awake(false);
        async {}
    })
    .detach();

    cx.spawn(async move |this, cx| loop {
        if this
            .update(cx, |this: &mut AppState, _cx| this.reconcile_keep_awake())
            .is_err()
        {
            break; // AppState dropped — app is exiting
        }
        cx.background_executor().timer(RECONCILE_INTERVAL).await;
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionStatus::*;

    #[test]
    fn running_session_keeps_awake() {
        assert!(should_keep_awake(true, [Idle, Running, Done]));
    }

    #[test]
    fn awaiting_input_session_keeps_awake() {
        assert!(should_keep_awake(true, [Suspended, AwaitingInput]));
    }

    #[test]
    fn sessions_that_are_not_working_let_the_mac_sleep() {
        assert!(!should_keep_awake(
            true,
            [Idle, Done, Suspended, ResponseReady]
        ));
    }

    #[test]
    fn no_sessions_lets_the_mac_sleep() {
        assert!(!should_keep_awake(true, []));
    }

    #[test]
    fn setting_off_lets_the_mac_sleep_even_while_running() {
        assert!(!should_keep_awake(false, [Running, AwaitingInput]));
    }

    // --- handler level (ARCHITECTURE.md §5.6) --------------------------------

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::SystemTime;

    use gpui::TestAppContext;

    use crate::app_state::fixture::Fixture;
    use crate::platform::PowerManagement;
    use crate::project::Project;
    use crate::session::Session;
    use crate::SettingsAction;

    /// Records the last requested state instead of calling IOKit, so these
    /// tests never touch the real machine's power state.
    #[derive(Default)]
    struct FakePower(AtomicBool);

    impl PowerManagement for FakePower {
        fn set_prevent_idle_sleep(&self, prevent: bool) -> crate::errors::Result<()> {
            self.0.store(prevent, Ordering::SeqCst);
            Ok(())
        }
        fn is_preventing_idle_sleep(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn fixture_with(status: SessionStatus, cx: &mut TestAppContext) -> (Fixture, Arc<FakePower>) {
        let now = SystemTime::now();
        let mut session = Session::suspended_from_persisted(
            "s0".into(),
            "s0".into(),
            now,
            now,
            Duration::ZERO,
            None,
        );
        session.status = status;
        let mut project = Project::new("alpha".into(), PathBuf::from("/tmp/alpha"));
        project.sessions.push(session);
        let fx = Fixture::with_projects(vec![project], cx);
        let power = Arc::new(FakePower::default());
        let handle: Arc<dyn PowerManagement> = power.clone();
        fx.update(cx, |state, _w, _cx| state.platform.power = handle);
        (fx, power)
    }

    #[gpui::test]
    fn reconcile_holds_while_running_and_releases_once_idle(cx: &mut TestAppContext) {
        let (fx, power) = fixture_with(Running, cx);

        fx.update(cx, |state, _w, _cx| state.reconcile_keep_awake());
        assert!(
            power.is_preventing_idle_sleep(),
            "a Running session holds it"
        );

        fx.update(cx, |state, _w, _cx| {
            state.projects[0].sessions[0].status = Idle;
            state.reconcile_keep_awake();
        });
        assert!(
            !power.is_preventing_idle_sleep(),
            "the last idle session releases it"
        );
    }

    /// Acceptance: turning the setting off releases a held assertion at once,
    /// in the handler — not on the next poll — and the choice is persisted.
    #[gpui::test]
    fn turning_the_setting_off_releases_immediately(cx: &mut TestAppContext) {
        let (fx, power) = fixture_with(AwaitingInput, cx);
        fx.update(cx, |state, _w, _cx| state.reconcile_keep_awake());
        assert!(power.is_preventing_idle_sleep());

        let (_, settings_dirty) = fx.dispatch(SettingsAction::UpdateKeepAwake(false), cx);

        assert!(!power.is_preventing_idle_sleep(), "released by the handler");
        assert!(
            settings_dirty,
            "the handler marks settings for the coordinator"
        );
        fx.checkpoint(cx);
        assert!(!fx.settings.snapshot().keep_awake_while_working);

        fx.dispatch(SettingsAction::UpdateKeepAwake(true), cx);
        assert!(
            power.is_preventing_idle_sleep(),
            "turning it back on re-holds"
        );
    }
}
