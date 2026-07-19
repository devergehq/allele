//! Object-store abstraction for session sync (DEV-187).
//!
//! [`SyncStore`] is the transport-agnostic surface every sync operation goes
//! through. An S3-compatible implementation (S3 / Cloudflare R2 / MinIO) lands
//! behind it in a follow-up; [`MemStore`] backs unit tests and dry runs so the
//! push/pull flows can be developed and tested without a live bucket.
//!
//! ## Key layout
//!
//! Each session is one *bundle* under `sessions/<uuid>/`:
//! - `sessions/<uuid>/meta.json`   — the portable [`crate::sync::SessionBundleMeta`]
//! - `sessions/<uuid>/transcript.jsonl` (+ `subagents/…`) — added in Phase 2
//!
//! Payloads are client-side encrypted before `put` (DEV-189); this layer sees
//! only opaque bytes.

use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

/// Prefix under which all per-session bundles live. `list(SESSIONS_PREFIX)`
/// enumerates every synced session in the store.
pub const SESSIONS_PREFIX: &str = "sessions/";

/// Object key for a session's metadata bundle: `sessions/<id>/meta.json`.
pub fn meta_key(session_id: &str) -> String {
    format!("{SESSIONS_PREFIX}{session_id}/meta.json")
}

/// Object key for a session's Claude transcript: `sessions/<id>/transcript.jsonl`.
pub fn transcript_key(session_id: &str) -> String {
    format!("{SESSIONS_PREFIX}{session_id}/transcript.jsonl")
}

/// Inverse of the `sessions/<id>/…` layout: extract the session id from a key,
/// or `None` if the key is not under [`SESSIONS_PREFIX`] or has an empty id
/// segment.
pub fn session_id_from_key(key: &str) -> Option<&str> {
    key.strip_prefix(SESSIONS_PREFIX)?
        .split('/')
        .next()
        .filter(|id| !id.is_empty())
}

/// A content-addressed blob store, scoped to one bucket/namespace. All methods
/// are idempotent from the caller's perspective and operate on opaque bytes.
#[async_trait]
pub trait SyncStore: Send + Sync {
    /// Write (or overwrite) the object at `key`.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()>;

    /// Read the object at `key`. `Ok(None)` means the object is absent;
    /// `Err` is reserved for transport/permission failures.
    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>>;

    /// List every key beginning with `prefix`, sorted ascending.
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>>;

    /// Remove the object at `key`. Deleting an absent key is not an error.
    async fn delete(&self, key: &str) -> anyhow::Result<()>;
}

/// In-memory [`SyncStore`] for tests and dry runs. Thread-safe; each instance
/// is its own isolated bucket.
#[derive(Default)]
pub struct MemStore {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SyncStore for MemStore {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.objects.lock().await.insert(key.to_string(), bytes);
        Ok(())
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.objects.lock().await.get(key).cloned())
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let guard = self.objects.lock().await;
        let mut keys: Vec<String> = guard
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        self.objects.lock().await.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_key_and_session_id_roundtrip() {
        let key = meta_key("abc-123");
        assert_eq!(key, "sessions/abc-123/meta.json");
        assert_eq!(session_id_from_key(&key), Some("abc-123"));
        assert_eq!(
            session_id_from_key("sessions/xyz/transcript.jsonl"),
            Some("xyz")
        );
        assert_eq!(session_id_from_key("other/x"), None);
        assert_eq!(session_id_from_key("sessions//meta.json"), None);
    }

    #[tokio::test]
    async fn memstore_put_get_list_delete() {
        let store = MemStore::new();

        // Absent key reads as None, not an error.
        assert_eq!(store.get("sessions/a/meta.json").await.unwrap(), None);

        store
            .put("sessions/a/meta.json", b"one".to_vec())
            .await
            .unwrap();
        store
            .put("sessions/b/meta.json", b"two".to_vec())
            .await
            .unwrap();
        store.put("other/x", b"z".to_vec()).await.unwrap();

        assert_eq!(
            store.get("sessions/a/meta.json").await.unwrap(),
            Some(b"one".to_vec())
        );

        // list() filters by prefix and returns sorted keys.
        let listed = store.list(SESSIONS_PREFIX).await.unwrap();
        assert_eq!(listed, vec!["sessions/a/meta.json", "sessions/b/meta.json"]);

        // Overwrite semantics.
        store
            .put("sessions/a/meta.json", b"one-v2".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.get("sessions/a/meta.json").await.unwrap(),
            Some(b"one-v2".to_vec())
        );

        store.delete("sessions/a/meta.json").await.unwrap();
        assert_eq!(store.get("sessions/a/meta.json").await.unwrap(), None);
        // Deleting an absent key is a no-op, not an error.
        store.delete("sessions/a/meta.json").await.unwrap();
    }
}
