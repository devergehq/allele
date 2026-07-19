//! Local sync ledger (DEV-192).
//!
//! Per-session bookkeeping that Phase 3's divergence guard builds on. For every
//! session this device has synced, it records the `base_revision` — the bundle
//! revision this device last pushed or pulled. Divergence (Phase 3) is then
//! *computed* by comparing the bucket's current revision against this base,
//! rather than guessed from wall-clock timestamps that drift across machines.
//!
//! Stored at `~/.allele/sync-ledger.json`, alongside `state.json`. Loads are
//! defensive (missing/corrupt → empty); saves are atomic (temp + rename), the
//! same discipline as [`crate::state::PersistedState`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// One session's sync bookkeeping.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// The bundle revision this device last synced (pushed or pulled) for the
    /// session. The next push is one past `max(base, remote)`.
    pub base_revision: u64,
}

/// Per-session `base_revision` map, persisted to `~/.allele/sync-ledger.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncLedger {
    #[serde(default)]
    entries: HashMap<String, LedgerEntry>,
}

impl SyncLedger {
    /// Path to `~/.allele/sync-ledger.json`.
    pub fn path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".allele").join("sync-ledger.json"))
    }

    /// Load the ledger from its default path. Missing or unparseable → empty.
    pub fn load() -> Self {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => {
                warn!("sync-ledger: no home directory — starting empty");
                Self::default()
            }
        }
    }

    fn load_from(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
                warn!(
                    "sync-ledger at {} failed to parse ({e}) — starting empty",
                    path.display()
                );
                Self::default()
            }),
            Err(e) => {
                warn!(
                    "sync-ledger at {} could not be read ({e}) — starting empty",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Atomically persist the ledger to its default path.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The revision this device last synced for `session_id`, if any.
    pub fn base_revision(&self, session_id: &str) -> Option<u64> {
        self.entries.get(session_id).map(|e| e.base_revision)
    }

    /// Compute the revision a fresh push should carry: one past the greater of
    /// what is already in the bucket (`remote_revision`) and what this device
    /// last synced. Never moves a revision backwards.
    pub fn next_push_revision(&self, session_id: &str, remote_revision: Option<u64>) -> u64 {
        let base = self.base_revision(session_id).unwrap_or(0);
        base.max(remote_revision.unwrap_or(0)) + 1
    }

    /// Record that this device has synced `session_id` at `revision` (call after
    /// a successful push or pull). In-memory only — call [`save`](Self::save) to
    /// persist.
    pub fn record_synced(&mut self, session_id: &str, revision: u64) {
        self.entries.insert(
            session_id.to_string(),
            LedgerEntry {
                base_revision: revision,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_push_revision_math() {
        let mut ledger = SyncLedger::default();
        // First push, no bucket object yet → revision 1.
        assert_eq!(ledger.next_push_revision("s", None), 1);
        // Bucket already ahead → one past it.
        assert_eq!(ledger.next_push_revision("s", Some(5)), 6);

        ledger.record_synced("s", 4);
        assert_eq!(ledger.base_revision("s"), Some(4));
        // No remote known → base + 1.
        assert_eq!(ledger.next_push_revision("s", None), 5);
        // Stale remote (behind our base) → base still wins.
        assert_eq!(ledger.next_push_revision("s", Some(2)), 5);
        // Remote ahead of base → remote wins.
        assert_eq!(ledger.next_push_revision("s", Some(9)), 10);
    }

    #[test]
    fn record_overwrites_base() {
        let mut ledger = SyncLedger::default();
        assert_eq!(ledger.base_revision("x"), None);
        ledger.record_synced("x", 7);
        assert_eq!(ledger.base_revision("x"), Some(7));
        ledger.record_synced("x", 8);
        assert_eq!(ledger.base_revision("x"), Some(8));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync-ledger.json");

        let mut ledger = SyncLedger::default();
        ledger.record_synced("a", 3);
        ledger.record_synced("b", 1);
        ledger.save_to(&path).unwrap();

        let loaded = SyncLedger::load_from(&path);
        assert_eq!(loaded.base_revision("a"), Some(3));
        assert_eq!(loaded.base_revision("b"), Some(1));
        assert_eq!(loaded.base_revision("missing"), None);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = SyncLedger::load_from(&dir.path().join("does-not-exist.json"));
        assert_eq!(ledger.base_revision("a"), None);
    }
}
