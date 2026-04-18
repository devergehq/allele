//! Persistence abstractions.
//!
//! Step 6 of the architecture refactor. Data types (`Settings`,
//! `PersistedState`) own their schema and (de)serialization logic;
//! *where* that bytes land is the repository's concern.
//!
//! Production uses JSON files under `~/.config/allele/`. Tests can
//! swap in `InMemorySettingsRepository` to exercise AppState without
//! touching the filesystem.
//!
//! Ownership: `AppState` holds `Arc<dyn Repository>` so repository
//! objects can be shared with background tasks that schedule saves.

use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use crate::errors::AlleleError;
use crate::settings::Settings;
use crate::state::PersistedState;

pub(crate) type RepoResult<T> = std::result::Result<T, AlleleError>;

// -----------------------------------------------------------------------
// Settings repository
// -----------------------------------------------------------------------

pub(crate) trait SettingsRepository: Send + Sync {
    fn load(&self) -> Settings;
    fn save(&self, settings: &Settings) -> RepoResult<()>;
}

/// Production implementation: JSON at `~/.config/allele/settings.json`.
/// Delegates to the inherent `Settings::{load, save}` methods so agent
/// seeding and atomic-write semantics live in one place.
pub(crate) struct JsonFileSettingsRepo;

impl SettingsRepository for JsonFileSettingsRepo {
    fn load(&self) -> Settings {
        Settings::load()
    }
    fn save(&self, settings: &Settings) -> RepoResult<()> {
        // `Settings::save` swallows errors (writes to stderr). Keeping
        // the trait's Result gives us room to tighten that later
        // without changing call sites.
        settings.save();
        Ok(())
    }
}

/// In-memory implementation for tests. `load` returns whatever was
/// last `save`d (or `Settings::default()` on first access).
#[cfg(test)]
pub(crate) struct InMemorySettingsRepository {
    inner: Mutex<Settings>,
}

#[cfg(test)]
impl InMemorySettingsRepository {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(Settings::default()) }
    }
}

#[cfg(test)]
impl SettingsRepository for InMemorySettingsRepository {
    fn load(&self) -> Settings {
        self.inner.lock().unwrap().clone()
    }
    fn save(&self, settings: &Settings) -> RepoResult<()> {
        *self.inner.lock().unwrap() = settings.clone();
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Persisted-state repository
// -----------------------------------------------------------------------

pub(crate) trait StateRepository: Send + Sync {
    fn load(&self) -> PersistedState;
    fn save(&self, state: &PersistedState) -> RepoResult<()>;
}

/// Production implementation: JSON at `~/.config/allele/state.json`.
pub(crate) struct JsonFileStateRepo;

impl StateRepository for JsonFileStateRepo {
    fn load(&self) -> PersistedState {
        PersistedState::load()
    }
    fn save(&self, state: &PersistedState) -> RepoResult<()> {
        state
            .save()
            .map_err(|e| AlleleError::State(format!("state save: {e}")))
    }
}

#[cfg(test)]
pub(crate) struct InMemoryStateRepository {
    inner: Mutex<PersistedState>,
}

#[cfg(test)]
impl InMemoryStateRepository {
    pub(crate) fn new() -> Self {
        Self { inner: Mutex::new(PersistedState::default()) }
    }
}

#[cfg(test)]
impl StateRepository for InMemoryStateRepository {
    fn load(&self) -> PersistedState {
        self.inner.lock().unwrap().clone()
    }
    fn save(&self, state: &PersistedState) -> RepoResult<()> {
        *self.inner.lock().unwrap() = state.clone();
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Bundle injected into AppState
// -----------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Repositories {
    pub(crate) settings: Arc<dyn SettingsRepository>,
    pub(crate) state: Arc<dyn StateRepository>,
}

impl Repositories {
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

    #[test]
    fn in_memory_settings_round_trip() {
        let repo = InMemorySettingsRepository::new();
        let mut s = Settings::default();
        s.font_size = 17.5;
        repo.save(&s).unwrap();
        assert_eq!(repo.load().font_size, 17.5);
    }

    #[test]
    fn in_memory_state_defaults_empty_sessions() {
        let repo = InMemoryStateRepository::new();
        let loaded = repo.load();
        assert!(loaded.sessions.is_empty());
    }
}
