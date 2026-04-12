//! Auto-trust helper for Claude Code workspaces.
//!
//! Claude Code persists workspace trust state in `~/.claude.json` under
//! `projects.<absolute-path>.hasTrustDialogAccepted: true`. The key is an
//! exact absolute path — no globs, no parent inheritance.
//!
//! Every Allele session creates a fresh APFS clone at
//! `~/.allele/workspaces/<project>/<short-id>/`, and without intervention
//! the user sees Claude Code's "Do you trust the files in this workspace?"
//! prompt on first entry. This module suppresses that prompt by stamping
//! a trusted entry into `~/.claude.json` at clone creation time.
//!
//! Design notes:
//! - Read-modify-write via `serde_json::Value` so unknown top-level fields
//!   and sibling project entries round-trip untouched.
//! - Atomic replacement via tempfile in the same directory + `rename(2)`,
//!   so a crash mid-write can't corrupt the file.
//! - Non-fatal: any failure is logged to stderr and swallowed — clone
//!   creation is not blocked by trust stamping.
//! - TOCTOU race with Claude Code's own writes is accepted. Worst case
//!   the user sees the trust prompt once, which is current behaviour.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Resolve the path to `~/.claude.json`.
fn claude_json_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(home.join(".claude.json"))
}

/// Load `~/.claude.json` as a `serde_json::Value`, returning an empty
/// object if the file does not exist. Parse errors are propagated so the
/// caller can log the specific corruption reason.
fn load_claude_json(path: &Path) -> anyhow::Result<Value> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let value: Value = serde_json::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
            Ok(value)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Object(Map::new()))
        }
        Err(e) => Err(anyhow::anyhow!("read {}: {e}", path.display())),
    }
}

/// Mutate `root` to set `projects.<key>.hasTrustDialogAccepted = true`,
/// preserving any existing sibling fields on the project entry and any
/// other top-level fields. Returns an error only if `root` is not an
/// object (which would indicate a corrupt `~/.claude.json`).
fn stamp_trust(root: &mut Value, key: &str) -> anyhow::Result<()> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("~/.claude.json top-level is not an object"))?;

    // Ensure `projects` exists and is an object.
    let projects = root_obj
        .entry("projects".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let projects_obj = projects
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("~/.claude.json `projects` is not an object"))?;

    // Ensure the per-path entry exists and is an object.
    let entry = projects_obj
        .entry(key.to_string())
        .or_insert_with(|| json!({}));
    let entry_obj = entry
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("projects[{key}] is not an object"))?;

    entry_obj.insert(
        "hasTrustDialogAccepted".to_string(),
        Value::Bool(true),
    );

    Ok(())
}

/// Atomically write `value` to `target`, using a sibling tempfile and
/// `rename(2)`. Both files must live in the same directory so the rename
/// is a single filesystem operation.
fn atomic_write(target: &Path, value: &Value) -> anyhow::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("target has no parent: {}", target.display()))?;

    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(".claude.json.allele-tmp-{pid}-{nanos}");
    let tmp_path = parent.join(tmp_name);

    let serialized = serde_json::to_string_pretty(value)
        .map_err(|e| anyhow::anyhow!("serialize claude.json: {e}"))?;

    fs::write(&tmp_path, serialized)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp_path.display()))?;

    fs::rename(&tmp_path, target).map_err(|e| {
        // Best-effort cleanup of the tempfile on rename failure.
        let _ = fs::remove_file(&tmp_path);
        anyhow::anyhow!("rename {} -> {}: {e}", tmp_path.display(), target.display())
    })?;

    Ok(())
}

/// Canonicalise `path` so the JSON key matches Claude Code's realpath
/// behaviour (Claude Code uses absolute, symlink-resolved paths).
fn canonical_key(path: &Path) -> anyhow::Result<String> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| anyhow::anyhow!("canonicalize {}: {e}", path.display()))?;
    Ok(canonical.to_string_lossy().into_owned())
}

/// Pre-register `path` as a trusted workspace in `~/.claude.json`, so
/// Claude Code does not prompt the user on first entry.
///
/// Returns `Ok(())` on success. On any failure (missing home, parse
/// error, write error) returns `Err` — callers are expected to log and
/// continue, never propagate as fatal.
pub fn trust_workspace(path: &Path) -> anyhow::Result<()> {
    let target = claude_json_path()?;
    let key = canonical_key(path)?;
    let mut root = load_claude_json(&target)?;
    stamp_trust(&mut root, &key)?;
    atomic_write(&target, &root)?;
    Ok(())
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    /// Exercise `load_claude_json` + `stamp_trust` + `atomic_write` against
    /// a scratch file in a tempdir. We can't point `trust_workspace` at a
    /// custom path (it's hardcoded to `~/.claude.json`), so tests operate
    /// on the inner helpers directly — still covers all failure-mode logic.
    fn round_trip(initial: Option<Value>, key: &str) -> Value {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join(".claude.json");
        if let Some(v) = initial {
            fs::write(&target, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }
        let mut root = load_claude_json(&target).unwrap();
        stamp_trust(&mut root, key).unwrap();
        atomic_write(&target, &root).unwrap();
        let raw = fs::read_to_string(&target).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn stamps_fresh_missing_file() {
        let result = round_trip(None, "/tmp/fake/workspace");
        assert_eq!(
            result["projects"]["/tmp/fake/workspace"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
    }

    #[test]
    fn preserves_unrelated_top_level_fields() {
        let initial = json!({
            "userID": "abc-123",
            "numStartups": 42,
            "projects": {}
        });
        let result = round_trip(Some(initial), "/tmp/fake/ws");
        assert_eq!(result["userID"], Value::String("abc-123".into()));
        assert_eq!(result["numStartups"], Value::Number(42.into()));
        assert_eq!(
            result["projects"]["/tmp/fake/ws"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
    }

    #[test]
    fn preserves_other_project_entries() {
        let initial = json!({
            "projects": {
                "/other/path": {
                    "hasTrustDialogAccepted": true,
                    "allowedTools": ["Read", "Edit"],
                    "lastCost": 1.23
                }
            }
        });
        let result = round_trip(Some(initial), "/tmp/fake/ws");
        assert_eq!(
            result["projects"]["/other/path"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
        assert_eq!(
            result["projects"]["/other/path"]["allowedTools"],
            json!(["Read", "Edit"])
        );
        assert_eq!(
            result["projects"]["/other/path"]["lastCost"],
            json!(1.23)
        );
        assert_eq!(
            result["projects"]["/tmp/fake/ws"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
    }

    #[test]
    fn updates_existing_project_entry_without_wiping_it() {
        let initial = json!({
            "projects": {
                "/tmp/fake/ws": {
                    "hasTrustDialogAccepted": false,
                    "allowedTools": ["Bash"],
                    "lastSessionId": "session-xyz"
                }
            }
        });
        let result = round_trip(Some(initial), "/tmp/fake/ws");
        assert_eq!(
            result["projects"]["/tmp/fake/ws"]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
        assert_eq!(
            result["projects"]["/tmp/fake/ws"]["allowedTools"],
            json!(["Bash"])
        );
        assert_eq!(
            result["projects"]["/tmp/fake/ws"]["lastSessionId"],
            Value::String("session-xyz".into())
        );
    }

    #[test]
    fn canonical_key_resolves_real_directory() {
        // Use the tempdir itself (guaranteed to exist and be canonicalisable).
        let tmp = TempDir::new().unwrap();
        let key = canonical_key(tmp.path()).unwrap();
        // The key should be an absolute path string.
        assert!(Path::new(&key).is_absolute(), "key must be absolute: {key}");
        // And the canonicalised form should match std::fs::canonicalize.
        let expected = fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(key, expected.to_string_lossy());
    }

    #[test]
    fn rejects_non_object_root() {
        let mut root = json!([1, 2, 3]);
        let err = stamp_trust(&mut root, "/tmp/x").unwrap_err();
        assert!(err.to_string().contains("not an object"));
    }
}
