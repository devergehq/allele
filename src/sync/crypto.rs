//! Client-side encryption for session-sync bundles (DEV-189).
//!
//! Two-tier key hierarchy (see `Plans/SESSION-SYNC-PROPOSAL.md` §2.9):
//! - a random age **X25519 data key** encrypts every object payload (fast, no
//!   per-object password hashing);
//! - a user **passphrase** wraps that data key (age scrypt) into a single blob
//!   stored in the bucket at `keyring/identity.age`, so a second device can
//!   bootstrap from it with one passphrase entry.
//!
//! Object *payloads* are encrypted; object *keys* (paths) are left visible — a
//! documented non-goal (they leak only random UUIDs + a session count).
//!
//! The data key is cached in the macOS Keychain so the passphrase is entered
//! once per device, not per operation.

use age::secrecy::{ExposeSecret, SecretString};
use age::x25519;

/// Keychain object key at which the wrapped/plain data-key secret is cached.
/// Scoped to the app bundle id so the item is Allele's alone.
const KEYCHAIN_SERVICE: &str = "com.allele.app.session-sync";
const KEYCHAIN_ACCOUNT: &str = "data-key";

/// Object key of the passphrase-wrapped data key inside the sync store.
pub const KEYRING_OBJECT_KEY: &str = "keyring/identity.age";

/// The sync **data key** — a random age X25519 identity. Every bundle payload is
/// encrypted to its recipient and decrypted with the identity.
pub struct DataKey {
    identity: x25519::Identity,
}

impl DataKey {
    /// Generate a fresh random data key. Done once, on the first device.
    pub fn generate() -> Self {
        Self {
            identity: x25519::Identity::generate(),
        }
    }

    /// Encrypt a payload to this key. Output is a self-describing age ciphertext
    /// (age handles nonce/stream framing internally).
    pub fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
        age::encrypt(&self.identity.to_public(), plaintext)
            .map_err(|e| anyhow::anyhow!("age encrypt failed: {e}"))
    }

    /// Decrypt an age ciphertext produced by [`Self::encrypt`]. **Fails closed**
    /// on a wrong key or tampered ciphertext — never returns partial plaintext.
    pub fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
        age::decrypt(&self.identity, ciphertext)
            .map_err(|e| anyhow::anyhow!("age decrypt failed (wrong key or corrupted data): {e}"))
    }

    /// Serialize the secret key as an `AGE-SECRET-KEY-1…` string. Handle with
    /// care — this is the key material.
    pub fn to_secret_string(&self) -> SecretString {
        self.identity.to_string()
    }

    /// Parse a data key from its secret-key string.
    pub fn from_secret_string(secret: &str) -> anyhow::Result<Self> {
        let identity = secret
            .trim()
            .parse::<x25519::Identity>()
            .map_err(|e| anyhow::anyhow!("invalid age secret key: {e}"))?;
        Ok(Self { identity })
    }

    /// Wrap this data key with a passphrase (age scrypt) → the blob stored at
    /// [`KEYRING_OBJECT_KEY`]. Scrypt runs once here, at setup — not per object.
    pub fn wrap_with_passphrase(&self, passphrase: &str) -> anyhow::Result<Vec<u8>> {
        let recipient = age::scrypt::Recipient::new(SecretString::from(passphrase.to_owned()));
        let secret = self.to_secret_string();
        age::encrypt(&recipient, secret.expose_secret().as_bytes())
            .map_err(|e| anyhow::anyhow!("failed to wrap data key: {e}"))
    }

    /// Recover a data key from a passphrase-wrapped blob (the second-device
    /// bootstrap). Fails closed on a wrong passphrase.
    pub fn unwrap_with_passphrase(blob: &[u8], passphrase: &str) -> anyhow::Result<Self> {
        let identity = age::scrypt::Identity::new(SecretString::from(passphrase.to_owned()));
        let secret_bytes = age::decrypt(&identity, blob)
            .map_err(|e| anyhow::anyhow!("failed to unwrap data key (wrong passphrase?): {e}"))?;
        let secret = String::from_utf8(secret_bytes)
            .map_err(|_| anyhow::anyhow!("unwrapped data key is not valid UTF-8"))?;
        Self::from_secret_string(&secret)
    }
}

/// macOS Keychain persistence for the data key. Not unit-tested (touches the
/// real login keychain); the crypto above is tested independently.
#[cfg(target_os = "macos")]
pub mod keychain {
    use super::{DataKey, KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};
    use age::secrecy::ExposeSecret;

    /// `errSecItemNotFound` — returned by the Keychain when the item is absent.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    /// Cache the data key in the Keychain (idempotent — overwrites any prior).
    pub fn store(key: &DataKey) -> anyhow::Result<()> {
        let secret = key.to_secret_string();
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
            secret.expose_secret().as_bytes(),
        )
        .map_err(|e| anyhow::anyhow!("failed to store data key in Keychain: {e}"))
    }

    /// Load the cached data key, or `None` if this device hasn't bootstrapped
    /// one yet (caller should prompt for the passphrase to unwrap from the store).
    pub fn load() -> anyhow::Result<Option<DataKey>> {
        match security_framework::passwords::get_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
        ) {
            Ok(bytes) => {
                let secret = String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("Keychain data key is not valid UTF-8"))?;
                Ok(Some(DataKey::from_secret_string(&secret)?))
            }
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "failed to read data key from Keychain: {e}"
            )),
        }
    }

    /// Remove the cached data key (e.g. on "disconnect this device").
    pub fn delete() -> anyhow::Result<()> {
        match security_framework::passwords::delete_generic_password(
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT,
        ) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(anyhow::anyhow!(
                "failed to delete data key from Keychain: {e}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = DataKey::generate();
        let plaintext = b"the quick brown fox";
        let ciphertext = key.encrypt(plaintext).unwrap();

        // Ciphertext must not be the plaintext (no leakage).
        assert_ne!(ciphertext.as_slice(), plaintext.as_slice());
        assert!(
            !ciphertext.windows(3).any(|w| w == b"fox"),
            "plaintext leaked into ciphertext"
        );

        assert_eq!(key.decrypt(&ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn decrypt_with_wrong_key_fails_closed() {
        let a = DataKey::generate();
        let b = DataKey::generate();
        let ciphertext = a.encrypt(b"secret").unwrap();
        assert!(
            b.decrypt(&ciphertext).is_err(),
            "wrong key must fail closed"
        );
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = DataKey::generate();
        let mut ciphertext = key.encrypt(b"secret payload").unwrap();
        // Flip a byte in the body.
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xff;
        assert!(key.decrypt(&ciphertext).is_err(), "tamper must be detected");
    }

    #[test]
    fn secret_string_roundtrip() {
        let key = DataKey::generate();
        let secret = key.to_secret_string();
        let restored = DataKey::from_secret_string(secret.expose_secret()).unwrap();
        // Same key → can decrypt each other's ciphertext.
        let ct = key.encrypt(b"hi").unwrap();
        assert_eq!(restored.decrypt(&ct).unwrap(), b"hi");
    }

    #[test]
    fn passphrase_wrap_unwrap_roundtrip() {
        let key = DataKey::generate();
        let wrapped = key
            .wrap_with_passphrase("correct horse battery staple")
            .unwrap();

        let recovered =
            DataKey::unwrap_with_passphrase(&wrapped, "correct horse battery staple").unwrap();
        // The recovered key decrypts what the original encrypted.
        let ct = key.encrypt(b"cross-device payload").unwrap();
        assert_eq!(recovered.decrypt(&ct).unwrap(), b"cross-device payload");
    }

    #[test]
    fn passphrase_unwrap_wrong_passphrase_fails() {
        let key = DataKey::generate();
        let wrapped = key.wrap_with_passphrase("right-passphrase").unwrap();
        assert!(
            DataKey::unwrap_with_passphrase(&wrapped, "wrong-passphrase").is_err(),
            "wrong passphrase must fail closed"
        );
    }
}
