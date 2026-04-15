//! Per-project `allele.json` config — declarative session setup.
//!
//! Reading `<project-root>/allele.json` lets a project pin a named set of
//! drawer terminals + an optional preview URL. On every session creation or
//! cold-resume Allele allocates one free local port, substitutes
//! `{{unique_port}}` in every command and the preview URL, spawns the tabs,
//! and opens the preview in the system browser.
//!
//! A missing file is not an error — callers should treat `None` as "do
//! nothing extra". A malformed file is also returned as `None`, with a
//! warning on stderr so the author can see why it was ignored.

use serde::Deserialize;
use std::net::TcpListener;
use std::path::Path;

const PORT_RANGE_START: u16 = 40000;
const PORT_RANGE_END: u16 = 49999;
const PLACEHOLDER_PORT: &str = "{{unique_port}}";
const PLACEHOLDER_FOLDER: &str = "{{folder}}";

#[derive(Debug, Clone, Deserialize)]
pub struct TerminalCfg {
    pub label: String,
    #[serde(default)]
    pub command: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreviewCfg {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub terminals: Vec<TerminalCfg>,
    #[serde(default)]
    pub preview: Option<PreviewCfg>,
    /// Overrides the globally-configured default coding agent for this
    /// project. Matches `AgentConfig.id` in settings.json. Missing or
    /// unknown ids fall back to the global default.
    #[serde(default)]
    pub agent: Option<String>,
    /// One-shot command run before terminals/preview are spawned. Must
    /// complete before the rest of the session materialises. Empty or
    /// whitespace-only is treated as absent.
    #[serde(default)]
    pub startup: Option<String>,
    /// One-shot command run when the session is discarded, before the
    /// clone is archived/trashed. Empty or whitespace-only is absent.
    #[serde(default)]
    pub shutdown: Option<String>,
}

impl ProjectConfig {
    /// Load `<project_root>/allele.json`. Returns `None` for missing,
    /// unreadable, or malformed files — in the malformed case a single
    /// warning line is written to stderr.
    pub fn load(project_root: &Path) -> Option<Self> {
        let path = project_root.join("allele.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Self>(&contents) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                eprintln!(
                    "allele.json at {} failed to parse ({e}) — ignoring",
                    path.display()
                );
                None
            }
        }
    }
}

/// Find a free TCP port in `40000..=49999` by trying to bind each in turn.
/// The listener is dropped before returning, so the caller races with
/// anything else on the machine to claim the port — fine for dev servers.
pub fn allocate_port() -> Option<u16> {
    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    eprintln!(
        "allele: no free port in {PORT_RANGE_START}..={PORT_RANGE_END} — \
         {{unique_port}} will be left unsubstituted"
    );
    None
}

/// Replace every occurrence of `{{unique_port}}` with `port` (when
/// allocated) and `{{folder}}` with the session's clone path.
pub fn substitute(text: &str, port: Option<u16>, folder: &Path) -> String {
    let mut out = text.to_string();
    if let Some(p) = port {
        out = out.replace(PLACEHOLDER_PORT, &p.to_string());
    }
    out = out.replace(PLACEHOLDER_FOLDER, &folder.to_string_lossy());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_replaces_all_occurrences() {
        let out = substitute(
            "a={{unique_port}} b={{unique_port}}",
            Some(42000),
            Path::new("/tmp/x"),
        );
        assert_eq!(out, "a=42000 b=42000");
    }

    #[test]
    fn substitute_no_placeholder_is_identity() {
        assert_eq!(
            substitute("no port here", Some(42000), Path::new("/tmp/x")),
            "no port here"
        );
    }

    #[test]
    fn substitute_replaces_folder() {
        let out = substitute(
            "cd {{folder}} && ls {{folder}}/bin",
            None,
            Path::new("/tmp/clone"),
        );
        assert_eq!(out, "cd /tmp/clone && ls /tmp/clone/bin");
    }

    #[test]
    fn substitute_replaces_both() {
        let out = substitute(
            "{{folder}}/bin/dev -p {{unique_port}}",
            Some(42000),
            Path::new("/tmp/clone"),
        );
        assert_eq!(out, "/tmp/clone/bin/dev -p 42000");
    }

    #[test]
    fn substitute_without_port_leaves_placeholder() {
        // When port allocation fails, the port placeholder is left intact.
        let out = substitute("-p {{unique_port}}", None, Path::new("/tmp"));
        assert_eq!(out, "-p {{unique_port}}");
    }

    #[test]
    fn load_missing_file_is_none() {
        let tmp = std::env::temp_dir().join("allele-test-missing");
        std::fs::create_dir_all(&tmp).unwrap();
        // Ensure there's no allele.json.
        let _ = std::fs::remove_file(tmp.join("allele.json"));
        assert!(ProjectConfig::load(&tmp).is_none());
    }

    #[test]
    fn load_parses_valid_config() {
        let tmp = std::env::temp_dir().join("allele-test-valid");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("allele.json"),
            r#"{
                "terminals": [
                    { "label": "Server", "command": "./bin/dev -p {{unique_port}}" },
                    { "label": "Terminal", "command": "" }
                ],
                "preview": { "url": "http://127.0.0.1:{{unique_port}}" }
            }"#,
        )
        .unwrap();
        let cfg = ProjectConfig::load(&tmp).expect("should parse");
        assert_eq!(cfg.terminals.len(), 2);
        assert_eq!(cfg.terminals[0].label, "Server");
        assert_eq!(cfg.terminals[1].command, "");
        assert_eq!(
            cfg.preview.as_ref().map(|p| p.url.as_str()),
            Some("http://127.0.0.1:{{unique_port}}")
        );
    }

    #[test]
    fn load_parses_startup_and_shutdown() {
        let tmp = std::env::temp_dir().join("allele-test-lifecycle");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("allele.json"),
            r#"{
                "startup": "bin/setup",
                "shutdown": "docker compose down"
            }"#,
        )
        .unwrap();
        let cfg = ProjectConfig::load(&tmp).expect("should parse");
        assert_eq!(cfg.startup.as_deref(), Some("bin/setup"));
        assert_eq!(cfg.shutdown.as_deref(), Some("docker compose down"));
    }

    #[test]
    fn load_without_lifecycle_fields_defaults_to_none() {
        let tmp = std::env::temp_dir().join("allele-test-no-lifecycle");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("allele.json"), r#"{ "terminals": [] }"#).unwrap();
        let cfg = ProjectConfig::load(&tmp).expect("should parse");
        assert!(cfg.startup.is_none());
        assert!(cfg.shutdown.is_none());
    }

    #[test]
    fn load_malformed_returns_none() {
        let tmp = std::env::temp_dir().join("allele-test-malformed");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("allele.json"), "not json").unwrap();
        assert!(ProjectConfig::load(&tmp).is_none());
    }

    #[test]
    fn allocate_port_returns_port_in_range() {
        let port = allocate_port().expect("should find a free port");
        assert!((PORT_RANGE_START..=PORT_RANGE_END).contains(&port));
    }
}
