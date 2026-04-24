//! Persistence backend traits for Settings and PersistedState.
//!
//! The production impls (`JsonFileSettingsRepo`, `JsonFileStateRepo`) delegate
//! to the inherent `load` / `save` methods on `Settings` / `PersistedState`,
//! which read/write JSON files under `~/.config/allele/settings.json` and
//! `~/.allele/state.json` respectively.
//!
//! Tests can swap in `InMemorySettingsRepository` / `InMemoryStateRepository`
//! (defined under `#[cfg(test)]` below) to get deterministic fixtures that
//! never touch the filesystem.
//!
//! Current scope (phase 13): save-side routing only. `AppState::save_state`
//! and `AppState::save_settings` go through `self.repos.*.save(...)`. The
//! load side is still called directly during startup because the loaded
//! values are consumed by the orphan-sweep and window-bounds machinery
//! before `AppState` exists.
//!
//! See ARCHITECTURE.md §3.3 and docs/RE-DECOMPOSITION-PLAN.md §5 phase 13.

use std::sync::Arc;

use crate::errors::{AlleleError, Result};
use crate::settings::Settings;
use crate::state::PersistedState;

pub(crate) trait SettingsRepository: Send + Sync {
    #[allow(dead_code)] // load side routed through Settings::load directly in phase 13
    fn load(&self) -> Settings;
    fn save(&self, settings: &Settings) -> Result<()>;
}

pub(crate) trait StateRepository: Send + Sync {
    #[allow(dead_code)] // load side routed through PersistedState::load directly in phase 13
    fn load(&self) -> PersistedState;
    fn save(&self, state: &PersistedState) -> Result<()>;
}

/// Production settings backend — reads/writes `~/.config/allele/settings.json`.
pub(crate) struct JsonFileSettingsRepo;

impl SettingsRepository for JsonFileSettingsRepo {
    fn load(&self) -> Settings {
        Settings::load()
    }

    fn save(&self, settings: &Settings) -> Result<()> {
        // `Settings::save` returns () and silently swallows IO errors; there
        // is nothing to map here. If that contract ever tightens to return
        // a Result, this adapter is the place to translate the error type.
        settings.save();
        Ok(())
    }
}

/// Production persisted-state backend — reads/writes `~/.allele/state.json`.
pub(crate) struct JsonFileStateRepo;

impl StateRepository for JsonFileStateRepo {
    fn load(&self) -> PersistedState {
        PersistedState::load()
    }

    fn save(&self, state: &PersistedState) -> Result<()> {
        state
            .save()
            .map_err(|e| AlleleError::State(e.to_string()))
    }
}

/// Bundle of every repository `AppState` needs. Arc-cloned into background
/// tasks so they can write without borrowing `AppState`.
pub(crate) struct Repositories {
    pub(crate) settings: Arc<dyn SettingsRepository>,
    pub(crate) state: Arc<dyn StateRepository>,
}

impl Repositories {
    /// Build the production bundle — JSON-file repos backed by the real
    /// inherent load/save methods.
    pub(crate) fn production() -> Self {
        Self {
            settings: Arc::new(JsonFileSettingsRepo),
            state: Arc::new(JsonFileStateRepo),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory settings backend for tests. Holds the current value behind
    /// an `Arc<Mutex<_>>` so multiple clones observe the same store.
    pub(crate) struct InMemorySettingsRepository {
        inner: Arc<Mutex<Settings>>,
    }

    impl InMemorySettingsRepository {
        pub(crate) fn new(initial: Settings) -> Self {
            Self {
                inner: Arc::new(Mutex::new(initial)),
            }
        }

        #[allow(dead_code)]
        pub(crate) fn snapshot(&self) -> Settings {
            self.inner.lock().unwrap().clone()
        }
    }

    impl SettingsRepository for InMemorySettingsRepository {
        fn load(&self) -> Settings {
            self.inner.lock().unwrap().clone()
        }

        fn save(&self, settings: &Settings) -> Result<()> {
            *self.inner.lock().unwrap() = settings.clone();
            Ok(())
        }
    }

    /// In-memory persisted-state backend for tests. Same shape as the
    /// settings fake above.
    pub(crate) struct InMemoryStateRepository {
        inner: Arc<Mutex<PersistedState>>,
    }

    impl InMemoryStateRepository {
        pub(crate) fn new(initial: PersistedState) -> Self {
            Self {
                inner: Arc::new(Mutex::new(initial)),
            }
        }

        #[allow(dead_code)]
        pub(crate) fn snapshot(&self) -> PersistedState {
            self.inner.lock().unwrap().clone()
        }
    }

    impl StateRepository for InMemoryStateRepository {
        fn load(&self) -> PersistedState {
            self.inner.lock().unwrap().clone()
        }

        fn save(&self, state: &PersistedState) -> Result<()> {
            *self.inner.lock().unwrap() = state.clone();
            Ok(())
        }
    }

    #[test]
    fn in_memory_settings_round_trips_save_and_load() {
        let repo = InMemorySettingsRepository::new(Settings::default());

        let next = Settings {
            sidebar_width: 999.0,
            ..Settings::default()
        };
        repo.save(&next).expect("in-memory save cannot fail");

        let loaded = repo.load();
        assert_eq!(loaded.sidebar_width, 999.0);
    }

    #[test]
    fn in_memory_state_round_trips_save_and_load() {
        let repo = InMemoryStateRepository::new(PersistedState::default());

        let next = PersistedState {
            last_active_session_id: Some("sess-xyz".to_string()),
            ..PersistedState::default()
        };
        repo.save(&next).expect("in-memory save cannot fail");

        let loaded = repo.load();
        assert_eq!(loaded.last_active_session_id.as_deref(), Some("sess-xyz"));
    }

    #[test]
    fn production_bundle_constructs() {
        // Smoke-test that `production()` wires up both repos without panic.
        // We don't call save() — that would touch the user's real
        // ~/.config/allele and ~/.allele directories.
        let repos = Repositories::production();
        // Arc count starts at 1 — confirm the fields are populated.
        assert_eq!(Arc::strong_count(&repos.settings), 1);
        assert_eq!(Arc::strong_count(&repos.state), 1);
    }
}
