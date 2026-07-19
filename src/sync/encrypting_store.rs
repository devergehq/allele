//! `EncryptingStore` — a [`SyncStore`] decorator that client-side encrypts every
//! object payload (DEV-189, §2.9).
//!
//! `put` encrypts before delegating; `get` decrypts after. `list`/`delete` pass
//! straight through because object *keys* are intentionally not encrypted. Every
//! layer above sees a plain `SyncStore` and never handles ciphertext.

use async_trait::async_trait;

use super::crypto::DataKey;
use super::store::SyncStore;

/// Wraps any [`SyncStore`], transparently encrypting payloads with an age data
/// key (§2.9). `build_store()` puts one of these around the S3/Mem/FS backend.
pub struct EncryptingStore<S: SyncStore> {
    inner: S,
    key: DataKey,
}

impl<S: SyncStore> EncryptingStore<S> {
    pub fn new(inner: S, key: DataKey) -> Self {
        Self { inner, key }
    }

    /// Borrow the wrapped store (used by tests to inspect ciphertext at rest).
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

#[async_trait]
impl<S: SyncStore> SyncStore for EncryptingStore<S> {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        let ciphertext = self.key.encrypt(&bytes)?;
        self.inner.put(key, ciphertext).await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match self.inner.get(key).await? {
            Some(ciphertext) => Ok(Some(self.key.decrypt(&ciphertext)?)),
            None => Ok(None),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        self.inner.list(prefix).await
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.inner.delete(key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::store::MemStore;

    #[tokio::test]
    async fn payload_is_ciphertext_at_rest_but_roundtrips() {
        let enc = EncryptingStore::new(MemStore::new(), DataKey::generate());
        let plaintext = b"PLAINTEXT-SECRET-conversation".to_vec();
        enc.put("sessions/a/meta.json", plaintext.clone())
            .await
            .unwrap();

        // What's actually stored in the inner (bucket) is ciphertext.
        let at_rest = enc
            .inner()
            .get("sessions/a/meta.json")
            .await
            .unwrap()
            .unwrap();
        assert_ne!(at_rest, plaintext, "payload stored in the clear");
        assert!(
            !at_rest.windows(9).any(|w| w == b"PLAINTEXT"),
            "plaintext leaked into stored ciphertext"
        );

        // Reading back through the decorator returns the plaintext.
        assert_eq!(
            enc.get("sessions/a/meta.json").await.unwrap(),
            Some(plaintext)
        );
    }

    #[tokio::test]
    async fn missing_object_stays_none() {
        let enc = EncryptingStore::new(MemStore::new(), DataKey::generate());
        assert_eq!(enc.get("sessions/missing/meta.json").await.unwrap(), None);
    }

    #[tokio::test]
    async fn list_and_delete_pass_through() {
        let enc = EncryptingStore::new(MemStore::new(), DataKey::generate());
        enc.put("sessions/a/meta.json", b"one".to_vec())
            .await
            .unwrap();
        enc.put("sessions/b/meta.json", b"two".to_vec())
            .await
            .unwrap();

        let keys = enc.list("sessions/").await.unwrap();
        assert_eq!(keys, vec!["sessions/a/meta.json", "sessions/b/meta.json"]);

        enc.delete("sessions/a/meta.json").await.unwrap();
        assert_eq!(enc.get("sessions/a/meta.json").await.unwrap(), None);
        assert_eq!(
            enc.get("sessions/b/meta.json").await.unwrap(),
            Some(b"two".to_vec())
        );
    }

    #[tokio::test]
    async fn wrong_key_cannot_read() {
        // Encrypt with one key, then try to read the same inner store through a
        // decorator holding a different key — must fail closed.
        let key_a = DataKey::generate();
        let ciphertext = key_a.encrypt(b"secret").unwrap();
        let inner = MemStore::new();
        inner.put("sessions/x/meta.json", ciphertext).await.unwrap();

        let enc_b = EncryptingStore::new(inner, DataKey::generate());
        assert!(
            enc_b.get("sessions/x/meta.json").await.is_err(),
            "decrypting with the wrong key must fail, not return garbage"
        );
    }
}
