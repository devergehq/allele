//! This machine's identity on the network: a persistent secret key per data
//! root, and a human name.
//!
//! The secret key is generated on first use and kept in the OS secret store
//! (the macOS Keychain), **never in a file under `~/.allele`**. The Keychain
//! item is keyed by the data root, so an instance redirected with
//! `ALLELE_HOME` / `--sandbox` gets its own identity and a test instance never
//! impersonates the real one (ARCHITECTURE.md §7.12).

use std::fmt;
use std::path::Path;

use crate::errors::{AlleleError, Result};
use crate::platform::SecretStore;

use super::transport::PeerId;

/// Keychain service under which every data root's secret key is stored.
pub const KEYCHAIN_SERVICE: &str = "com.allele.app.remote-identity";

/// Shown when neither the setting nor the OS gives a machine name.
pub const FALLBACK_MACHINE_NAME: &str = "Unnamed Mac";

/// The Keychain account for a data root: the root's path. Two data roots
/// therefore never share a key.
///
/// The path is used as given, not canonicalised: canonicalising would follow a
/// symlink that might later change, and with it the machine's identity.
pub fn keychain_account(data_root: &Path) -> String {
    data_root.to_string_lossy().into_owned()
}

/// 32 secret-key bytes. `Debug` is redacted so a stray `{:?}` cannot log it.
pub(super) struct SecretBytes([u8; 32]);

impl SecretBytes {
    pub(super) fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes(..)")
    }
}

/// This instance's network identity: the secret key and the [`PeerId`]
/// derived from it.
#[derive(Debug)]
pub struct LocalIdentity {
    id: PeerId,
    secret: SecretBytes,
}

impl LocalIdentity {
    /// Load this data root's identity from `store`, generating and storing one
    /// on first use.
    ///
    /// A stored value of the wrong length is an **error**, not a cue to
    /// generate a new key: silently replacing the identity would orphan every
    /// pairing made with the old one.
    pub(crate) fn load_or_create(store: &dyn SecretStore, data_root: &Path) -> Result<Self> {
        // Check-then-generate must be atomic, or two first-use callers (the
        // Settings pane and the listener at startup) each store a key and the
        // loser hands out an id whose secret was just overwritten.
        static FIRST_USE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = FIRST_USE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let account = keychain_account(data_root);
        if let Some(bytes) = store.load(KEYCHAIN_SERVICE, &account)? {
            let secret: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                AlleleError::Remote(format!(
                    "the machine identity in the Keychain ({KEYCHAIN_SERVICE} / {account}) is \
                     {} bytes, expected 32; refusing to replace it",
                    bytes.len()
                ))
            })?;
            return Ok(Self::from_secret(secret));
        }
        let secret = super::iroh_transport::generate_secret();
        store.store(KEYCHAIN_SERVICE, &account, &secret)?;
        tracing::info!("generated a new machine identity for {account}");
        Ok(Self::from_secret(secret))
    }

    /// This instance's identity, for the data root it is running against.
    pub(crate) fn for_this_instance(store: &dyn SecretStore) -> Result<Self> {
        let root = crate::paths::allele_dir()
            .ok_or_else(|| AlleleError::Remote("no home directory to key the identity".into()))?;
        Self::load_or_create(store, &root)
    }

    fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            id: super::iroh_transport::public_id(&secret),
            secret: SecretBytes(secret),
        }
    }

    /// The id peers dial to reach this machine.
    pub fn id(&self) -> PeerId {
        self.id
    }

    pub(super) fn secret(&self) -> &SecretBytes {
        &self.secret
    }

    /// A fresh identity that is never stored. Tests only.
    #[cfg(test)]
    pub(crate) fn ephemeral() -> Self {
        Self::from_secret(super::iroh_transport::generate_secret())
    }
}

/// The machine name to show peers: the user's setting if it has any text,
/// otherwise the computer name, otherwise [`FALLBACK_MACHINE_NAME`].
///
/// Resolved at read time rather than copied into settings on first launch, so
/// renaming the Mac in System Settings carries through until the user picks a
/// name of their own.
pub fn resolve_machine_name(setting: Option<&str>, computer_name: Option<String>) -> String {
    setting
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            computer_name
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| FALLBACK_MACHINE_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// An in-memory stand-in for the Keychain.
    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<(String, String), Vec<u8>>>);

    impl SecretStore for MemoryStore {
        fn load(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>> {
            let map = self.0.lock().unwrap();
            Ok(map.get(&(service.into(), account.into())).cloned())
        }

        fn store(&self, service: &str, account: &str, secret: &[u8]) -> Result<()> {
            let mut map = self.0.lock().unwrap();
            map.insert((service.into(), account.into()), secret.to_vec());
            Ok(())
        }
    }

    #[test]
    fn identity_is_stable_across_restarts() {
        let store = MemoryStore::default();
        let root = Path::new("/Users/me/.allele");
        let first = LocalIdentity::load_or_create(&store, root).unwrap();
        // A restart is a fresh load against the same Keychain.
        let second = LocalIdentity::load_or_create(&store, root).unwrap();
        assert_eq!(first.id(), second.id());
        assert_eq!(first.secret().expose(), second.secret().expose());
        assert_eq!(store.0.lock().unwrap().len(), 1, "generated exactly once");
    }

    #[test]
    fn two_data_roots_get_two_identities() {
        let store = MemoryStore::default();
        let real = LocalIdentity::load_or_create(&store, Path::new("/Users/me/.allele")).unwrap();
        let sandbox =
            LocalIdentity::load_or_create(&store, Path::new("/tmp/sandbox/.allele")).unwrap();
        assert_ne!(real.id(), sandbox.id());
        assert_ne!(
            keychain_account(Path::new("/Users/me/.allele")),
            keychain_account(Path::new("/tmp/sandbox/.allele"))
        );
    }

    #[test]
    fn the_secret_goes_to_the_store_and_nowhere_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::default();
        let id = LocalIdentity::load_or_create(&store, dir.path()).unwrap();
        // The only copy is the store's, under the service and the root's account.
        let stored = store
            .load(KEYCHAIN_SERVICE, &keychain_account(dir.path()))
            .unwrap()
            .unwrap();
        assert_eq!(stored.as_slice(), id.secret().expose());
        // And the data root is untouched.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_malformed_stored_key_is_an_error_not_a_new_identity() {
        let store = MemoryStore::default();
        let root = Path::new("/Users/me/.allele");
        store
            .store(KEYCHAIN_SERVICE, &keychain_account(root), b"short")
            .unwrap();
        let err = LocalIdentity::load_or_create(&store, root).unwrap_err();
        assert!(matches!(err, AlleleError::Remote(_)), "{err:?}");
        // The bad value is left for a human to look at, not overwritten.
        let still = store
            .load(KEYCHAIN_SERVICE, &keychain_account(root))
            .unwrap();
        assert_eq!(still.as_deref(), Some(&b"short"[..]));
    }

    #[test]
    fn a_store_error_propagates() {
        let err = LocalIdentity::load_or_create(
            &crate::platform::unsupported::NoSecretStore,
            Path::new("/x"),
        )
        .unwrap_err();
        assert!(
            matches!(err, AlleleError::PlatformUnsupported(_)),
            "{err:?}"
        );
    }

    #[test]
    fn debug_output_redacts_the_secret() {
        let id = LocalIdentity::ephemeral();
        let dbg = format!("{id:?}");
        assert!(dbg.contains("SecretBytes(..)"), "{dbg}");
    }

    #[test]
    fn machine_name_defaults_to_the_computer_name() {
        assert_eq!(
            resolve_machine_name(None, Some("Patrick's MacBook Pro".into())),
            "Patrick's MacBook Pro"
        );
    }

    #[test]
    fn blank_machine_name_falls_back_to_the_computer_name() {
        assert_eq!(
            resolve_machine_name(Some("   "), Some("Studio".into())),
            "Studio"
        );
    }

    #[test]
    fn a_chosen_machine_name_wins() {
        assert_eq!(
            resolve_machine_name(Some(" Desk "), Some("Studio".into())),
            "Desk"
        );
    }

    #[test]
    fn no_name_anywhere_uses_the_fallback() {
        assert_eq!(resolve_machine_name(None, None), FALLBACK_MACHINE_NAME);
        assert_eq!(
            resolve_machine_name(None, Some(" ".into())),
            FALLBACK_MACHINE_NAME
        );
    }
}
