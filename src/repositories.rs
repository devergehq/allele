//! Persistence backend traits for Settings and PersistedState.
//!
//! The production impls (`JsonFileSettingsRepo`, `JsonFileStateRepo`) delegate
//! to the inherent `load` / `save` methods on `Settings` / `PersistedState`,
//! which read/write JSON files under `~/.config/allele/settings.json` and
//! `~/.allele/state.json` respectively.
//!
//! Tests can swap in `InMemorySettingsRepository` / `InMemoryStateRepository`
//! (in the `#[cfg(test)] fakes` module below) to get deterministic fixtures
//! that never touch the filesystem. `app_state::fixture` bundles them into a
//! ready-to-drive `AppState` — see ARCHITECTURE.md §5.6.
//!
//! Current scope (phase 13): save-side routing only. `AppState::save_state`
//! and `AppState::save_settings` go through `self.repos.*.save(...)`. The
//! load side is still called directly during startup because the loaded
//! values are consumed by the orphan-sweep and window-bounds machinery
//! before `AppState` exists.
//!
//! See ARCHITECTURE.md §3.3 and docs/RE-DECOMPOSITION-PLAN.md §5 phase 13.

use std::sync::Arc;

use std::path::PathBuf;

use crate::errors::{AlleleError, Result};
use crate::remote::PeerList;
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
        state.save().map_err(|e| AlleleError::State(e.to_string()))
    }
}

/// The trusted-peers list, `~/.allele/peers.json` (DEV-1160).
///
/// Owned by `remote::RemoteService` rather than the `Repositories` bundle:
/// nothing on `AppState` reads or writes peers except through that service.
///
/// Unlike settings and state, a change here is a trust decision and is written
/// at once by `remote::PeerRegistry`, not coalesced at the render tick: a
/// revoked peer must stay revoked if the app dies a moment later.
pub(crate) trait PeersRepository: Send + Sync {
    /// The stored list; an empty list when the file does not exist. An
    /// unreadable or corrupt file is an **error**, so the caller can fail
    /// closed (trust nobody) rather than quietly treat it as empty and then
    /// overwrite it.
    fn load(&self) -> Result<PeerList>;
    fn save(&self, peers: &PeerList) -> Result<()>;
}

/// Production peers backend — `peers.json`, written atomically and created
/// owner-only (0600) so no other account on the Mac can read or edit it.
pub(crate) struct JsonFilePeersRepo {
    path: Option<PathBuf>,
}

impl JsonFilePeersRepo {
    /// The file for this instance's data root.
    pub(crate) fn production() -> Self {
        Self {
            path: crate::paths::peers_file(),
        }
    }

    /// A file at an explicit path. Tests only.
    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> Result<&PathBuf> {
        self.path
            .as_ref()
            .ok_or_else(|| AlleleError::Remote("no home directory for peers.json".into()))
    }
}

impl PeersRepository for JsonFilePeersRepo {
    fn load(&self) -> Result<PeerList> {
        let path = self.path()?;
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PeerList::default()),
            Err(e) => return Err(e.into()),
        };
        tighten_permissions(path);
        serde_json::from_str(&text).map_err(|e| {
            AlleleError::Remote(format!("{} is not a valid peer list: {e}", path.display()))
        })
    }

    fn save(&self, peers: &PeerList) -> Result<()> {
        let path = self.path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(peers)
            .map_err(|e| AlleleError::Remote(format!("could not encode the peer list: {e}")))?;
        // Write a sibling then rename over, so a crash never leaves a torn
        // file. The sibling is *created* 0600 rather than chmod-ed after, so
        // there is no moment at which it is readable by others.
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        tighten_permissions(path);
        Ok(())
    }
}

/// Force `path` to 0600 if anything (a hand edit, a restore from backup) left
/// it wider. Logged rather than fatal: the list is still the right list.
fn tighten_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if meta.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                "{} was readable by others; resetting it to 0600",
                path.display()
            );
            if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                tracing::warn!("could not chmod {}: {e}", path.display());
            }
        }
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

/// Test-only fakes. Public to the crate (not just this module's `tests`)
/// so any module can build an `AppState` that never touches the filesystem —
/// see `app_state::fixture` and ARCHITECTURE.md §5.6.
#[cfg(test)]
pub(crate) mod fakes {
    use super::*;
    use std::sync::Mutex;

    /// In-memory settings backend for tests. Holds the current value behind
    /// an `Arc<Mutex<_>>` so multiple clones observe the same store.
    pub(crate) struct InMemorySettingsRepository {
        inner: Arc<Mutex<Settings>>,
        /// How many times `save` has been called. Distinguishes "the
        /// coordinator wrote once" from "it wrote on every mutation".
        saves: Arc<Mutex<usize>>,
    }

    impl InMemorySettingsRepository {
        pub(crate) fn new(initial: Settings) -> Self {
            Self {
                inner: Arc::new(Mutex::new(initial)),
                saves: Arc::new(Mutex::new(0)),
            }
        }

        pub(crate) fn snapshot(&self) -> Settings {
            self.inner.lock().unwrap().clone()
        }

        pub(crate) fn save_count(&self) -> usize {
            *self.saves.lock().unwrap()
        }
    }

    impl SettingsRepository for InMemorySettingsRepository {
        fn load(&self) -> Settings {
            self.inner.lock().unwrap().clone()
        }

        fn save(&self, settings: &Settings) -> Result<()> {
            *self.inner.lock().unwrap() = settings.clone();
            *self.saves.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// In-memory persisted-state backend for tests. Same shape as the
    /// settings fake above.
    pub(crate) struct InMemoryStateRepository {
        inner: Arc<Mutex<PersistedState>>,
        saves: Arc<Mutex<usize>>,
    }

    impl InMemoryStateRepository {
        pub(crate) fn new(initial: PersistedState) -> Self {
            Self {
                inner: Arc::new(Mutex::new(initial)),
                saves: Arc::new(Mutex::new(0)),
            }
        }

        pub(crate) fn snapshot(&self) -> PersistedState {
            self.inner.lock().unwrap().clone()
        }

        pub(crate) fn save_count(&self) -> usize {
            *self.saves.lock().unwrap()
        }
    }

    impl StateRepository for InMemoryStateRepository {
        fn load(&self) -> PersistedState {
            self.inner.lock().unwrap().clone()
        }

        fn save(&self, state: &PersistedState) -> Result<()> {
            *self.inner.lock().unwrap() = state.clone();
            *self.saves.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// In-memory peers backend for tests. Clones share one store, so a test
    /// can hand one to the code under test and read the result from another.
    #[derive(Clone, Default)]
    pub(crate) struct InMemoryPeersRepository {
        inner: Arc<Mutex<PeerList>>,
    }

    impl InMemoryPeersRepository {
        pub(crate) fn snapshot(&self) -> PeerList {
            self.inner.lock().unwrap().clone()
        }
    }

    impl PeersRepository for InMemoryPeersRepository {
        fn load(&self) -> Result<PeerList> {
            Ok(self.inner.lock().unwrap().clone())
        }

        fn save(&self, peers: &PeerList) -> Result<()> {
            *self.inner.lock().unwrap() = peers.clone();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{InMemorySettingsRepository, InMemoryStateRepository};
    use super::*;

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
        assert_eq!(repo.save_count(), 1);
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
        assert_eq!(repo.save_count(), 1);
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

    fn sample_peers() -> PeerList {
        PeerList::new(vec![crate::remote::peers::TrustedPeer::newly_paired(
            crate::remote::LocalIdentity::ephemeral().id(),
            "Laptop",
        )])
    }

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// DEV-1160 acceptance: `peers.json` is created with mode 0600.
    #[cfg(unix)]
    #[test]
    fn peers_json_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("peers.json");
        let repo = JsonFilePeersRepo::at(path.clone());
        repo.save(&sample_peers()).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_widened_peers_json_is_reset_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        let repo = JsonFilePeersRepo::at(path.clone());
        repo.save(&sample_peers()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        repo.load().unwrap();
        assert_eq!(mode_of(&path), 0o600);
        repo.save(&sample_peers()).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn peers_json_round_trips_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JsonFilePeersRepo::at(dir.path().join("peers.json"));
        assert_eq!(repo.load().unwrap(), PeerList::default());
        let list = sample_peers();
        repo.save(&list).unwrap();
        assert_eq!(repo.load().unwrap(), list);
    }

    #[test]
    fn a_corrupt_peers_json_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(JsonFilePeersRepo::at(path).load().is_err());
    }
}
