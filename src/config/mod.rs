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

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::net::TcpListener;
use std::path::Path;
use tracing::warn;

use crate::settings::ProjectSettings;

const PORT_RANGE_START: u16 = 40000;
const PORT_RANGE_END: u16 = 49999;
const PLACEHOLDER_PORT: &str = "{{unique_port}}";
const PLACEHOLDER_FOLDER: &str = "{{folder}}";

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Literal environment variables for every process the session spawns.
    /// See `ProjectSettings::env`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Directories prepended to `PATH`, in declared order.
    /// See `ProjectSettings::path_prepend`.
    #[serde(default)]
    pub path_prepend: Vec<String>,
}

impl ProjectConfig {
    /// Load `<project_root>/allele.json`. Returns `None` for missing,
    /// unreadable, or malformed files — in the malformed case a single
    /// warning line is written to stderr.
    ///
    /// This is the backwards-compatible path. The preferred source is
    /// `from_settings()` which reads from `~/.config/allele/settings.json`.
    pub fn load(project_root: &Path) -> Option<Self> {
        let path = project_root.join("allele.json");
        let contents = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Self>(&contents) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                warn!(
                    "allele.json at {} failed to parse ({e}) — ignoring",
                    path.display()
                );
                None
            }
        }
    }

    /// Build a `ProjectConfig` from the orchestration fields stored in
    /// `ProjectSettings` (persisted in `~/.config/allele/settings.json`).
    ///
    /// Returns `None` when the user hasn't configured any orchestration
    /// for this project (no terminals and no startup command).
    pub fn from_settings(settings: &ProjectSettings) -> Option<Self> {
        if settings.terminals.is_empty()
            && settings.startup.is_none()
            && settings.env.is_empty()
            && settings.path_prepend.is_empty()
        {
            return None;
        }
        Some(Self {
            terminals: settings.terminals.clone(),
            preview: None,
            agent: None,
            startup: settings.startup.clone(),
            shutdown: settings.shutdown.clone(),
            env: settings.env.clone(),
            path_prepend: settings.path_prepend.clone(),
        })
    }
}

/// The environment a project declares for every process its sessions spawn.
///
/// Kept separate from the rest of `ProjectConfig` because it applies to more
/// than orchestration: a session created with orchestration disabled still
/// needs its toolchain resolved, so this is read at every spawn site rather
/// than behind the `runs_startup()` gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectEnv {
    pub vars: BTreeMap<String, String>,
    pub path_prepend: Vec<String>,
}

impl ProjectEnv {
    /// Resolve from a per-repo `allele.json` if present, else from the
    /// project's settings — the same precedence the `agent` override uses,
    /// so a repo can pin its own toolchain for everyone who clones it.
    pub fn resolve(source_path: &Path, settings: &ProjectSettings) -> Self {
        match ProjectConfig::load(source_path) {
            Some(cfg) => Self {
                vars: cfg.env,
                path_prepend: cfg.path_prepend,
            },
            None => Self {
                vars: settings.env.clone(),
                path_prepend: settings.path_prepend.clone(),
            },
        }
    }

    /// Take the environment out of an already-loaded config, for the call
    /// sites that resolve `ProjectConfig` for other reasons anyway.
    pub fn from_config(cfg: &ProjectConfig) -> Self {
        Self {
            vars: cfg.env.clone(),
            path_prepend: cfg.path_prepend.clone(),
        }
    }

    /// Materialise into `(key, value)` pairs ready for a PTY or `Command`.
    ///
    /// `path_prepend` is folded into a single `PATH` entry built from
    /// `inherited_path` — passed in rather than read from the process so this
    /// stays testable. An explicit `PATH` in `vars` is treated as the base to
    /// prepend onto, so declaring both cannot silently discard one of them.
    /// Blank entries are dropped and exact duplicates collapse to their first
    /// occurrence, keeping a hand-edited settings.json from bloating PATH.
    pub fn materialise(
        &self,
        port: Option<u16>,
        folder: &Path,
        inherited_path: Option<&str>,
    ) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .vars
            .iter()
            .filter(|(key, _)| !key.trim().is_empty() && *key != "PATH")
            .map(|(key, value)| (key.clone(), substitute(value, port, folder)))
            .collect();

        if self.path_prepend.is_empty() {
            if let Some(path) = self.vars.get("PATH") {
                out.push(("PATH".to_string(), substitute(path, port, folder)));
            }
            return out;
        }

        let base = self
            .vars
            .get("PATH")
            .map(|p| substitute(p, port, folder))
            .or_else(|| inherited_path.map(str::to_string))
            .unwrap_or_default();

        let mut seen = HashSet::new();
        let mut entries: Vec<String> = Vec::new();
        for dir in &self.path_prepend {
            let dir = substitute(dir, port, folder);
            if dir.trim().is_empty() || !seen.insert(dir.clone()) {
                continue;
            }
            entries.push(dir);
        }
        if !base.is_empty() {
            entries.push(base);
        }
        out.push(("PATH".to_string(), entries.join(":")));
        out
    }
}

/// Find a free TCP port in `40000..=49999` by trying to bind each in turn.
/// The listener is dropped before returning, so the caller races with
/// anything else on the machine to claim the port — fine for dev servers.
///
/// `reserved` ports are skipped even if nothing is currently listening on
/// them. A TCP bind probe only sees *currently-listening* servers, but a
/// suspended session keeps its claim on a port (its Traefik route file
/// survives, since suspend doesn't run session-stop) while its dev server
/// is down. Passing those claimed ports here keeps a freshly-resumed
/// session from being handed a port another session already owns.
pub fn allocate_port(reserved: &HashSet<u16>) -> Option<u16> {
    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if reserved.contains(&port) {
            continue;
        }
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    warn!(
        "allele: no free port in {PORT_RANGE_START}..={PORT_RANGE_END} — \
         {{unique_port}} will be left unsubstituted"
    );
    None
}

/// Resolve a startup/shutdown command path. If the command starts with
/// a relative path component (no leading `/` or `~`), prepend the
/// project's script directory at `~/.allele/projects/{name}/scripts/`.
pub fn resolve_script_command(cmd: &str, project_name: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('~') {
        return trimmed.to_string();
    }
    let scripts_dir = crate::paths::project_scripts_dir(project_name)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // If the first token looks like a relative path to a script, resolve it
    let first_token = trimmed.split_whitespace().next().unwrap_or("");
    if first_token.contains('/') || first_token.ends_with(".sh") {
        let resolved = format!("{scripts_dir}/{trimmed}");
        return resolved;
    }
    trimmed.to_string()
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
        let port = allocate_port(&HashSet::new()).expect("should find a free port");
        assert!((PORT_RANGE_START..=PORT_RANGE_END).contains(&port));
    }

    #[test]
    fn allocate_port_skips_reserved() {
        // Reserve the bottom of the range; allocation must hop past it.
        let reserved: HashSet<u16> = (PORT_RANGE_START..PORT_RANGE_START + 3).collect();
        let port = allocate_port(&reserved).expect("should find a free port");
        assert!(!reserved.contains(&port));
        assert!((PORT_RANGE_START..=PORT_RANGE_END).contains(&port));
    }

    fn env_of(pairs: &[(&str, &str)], prepend: &[&str]) -> ProjectEnv {
        ProjectEnv {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            path_prepend: prepend.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn lookup<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn materialise_empty_env_yields_nothing() {
        let out = ProjectEnv::default().materialise(None, Path::new("/tmp/c"), Some("/usr/bin"));
        assert!(out.is_empty());
    }

    #[test]
    fn materialise_prepends_path_in_declared_order() {
        let env = env_of(&[], &["/opt/php/bin", "/opt/php/sbin"]);
        let out = env.materialise(None, Path::new("/tmp/c"), Some("/usr/bin:/bin"));
        assert_eq!(
            lookup(&out, "PATH"),
            Some("/opt/php/bin:/opt/php/sbin:/usr/bin:/bin")
        );
    }

    #[test]
    fn materialise_passes_vars_through_untouched() {
        let env = env_of(&[("APP_ENV", "local")], &[]);
        let out = env.materialise(None, Path::new("/tmp/c"), Some("/usr/bin"));
        assert_eq!(out, vec![("APP_ENV".to_string(), "local".to_string())]);
    }

    #[test]
    fn materialise_substitutes_in_values_and_paths() {
        let env = env_of(
            &[("APP_URL", "http://localhost:{{unique_port}}")],
            &["{{folder}}/bin"],
        );
        let out = env.materialise(Some(42000), Path::new("/tmp/clone"), Some("/usr/bin"));
        assert_eq!(lookup(&out, "APP_URL"), Some("http://localhost:42000"));
        assert_eq!(lookup(&out, "PATH"), Some("/tmp/clone/bin:/usr/bin"));
    }

    #[test]
    fn materialise_treats_explicit_path_as_the_base() {
        // Declaring both must not silently drop either one.
        let env = env_of(&[("PATH", "/custom")], &["/opt/php/bin"]);
        let out = env.materialise(None, Path::new("/tmp/c"), Some("/usr/bin"));
        assert_eq!(lookup(&out, "PATH"), Some("/opt/php/bin:/custom"));
    }

    #[test]
    fn materialise_keeps_explicit_path_when_nothing_is_prepended() {
        let env = env_of(&[("PATH", "/custom")], &[]);
        let out = env.materialise(None, Path::new("/tmp/c"), Some("/usr/bin"));
        assert_eq!(lookup(&out, "PATH"), Some("/custom"));
    }

    #[test]
    fn materialise_drops_blank_and_duplicate_path_entries() {
        let env = env_of(
            &[],
            &["/opt/php/bin", "  ", "/opt/php/bin", "/opt/node/bin"],
        );
        let out = env.materialise(None, Path::new("/tmp/c"), Some("/usr/bin"));
        assert_eq!(
            lookup(&out, "PATH"),
            Some("/opt/php/bin:/opt/node/bin:/usr/bin")
        );
    }

    #[test]
    fn materialise_without_inherited_path_emits_only_the_prepends() {
        let env = env_of(&[], &["/opt/php/bin"]);
        let out = env.materialise(None, Path::new("/tmp/c"), None);
        assert_eq!(lookup(&out, "PATH"), Some("/opt/php/bin"));
    }

    #[test]
    fn materialise_ignores_blank_keys() {
        let env = env_of(&[("", "ignored"), ("KEEP", "yes")], &[]);
        let out = env.materialise(None, Path::new("/tmp/c"), Some("/usr/bin"));
        assert_eq!(out, vec![("KEEP".to_string(), "yes".to_string())]);
    }

    #[test]
    fn load_parses_env_and_path_prepend() {
        let tmp = std::env::temp_dir().join("allele-test-env-cfg");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("allele.json"),
            r#"{"env":{"APP_ENV":"local"},"path_prepend":["/opt/php/bin"]}"#,
        )
        .unwrap();
        let cfg = ProjectConfig::load(&tmp).expect("should parse");
        assert_eq!(cfg.env.get("APP_ENV").map(String::as_str), Some("local"));
        assert_eq!(cfg.path_prepend, vec!["/opt/php/bin".to_string()]);
    }

    #[test]
    fn from_settings_carries_env_even_with_no_orchestration() {
        // A project that only pins a toolchain still needs a config back.
        let settings = ProjectSettings {
            path_prepend: vec!["/opt/php/bin".to_string()],
            ..Default::default()
        };
        let cfg = ProjectConfig::from_settings(&settings).expect("env alone is enough");
        assert_eq!(cfg.path_prepend, vec!["/opt/php/bin".to_string()]);
        assert!(cfg.terminals.is_empty());
    }

    #[test]
    fn from_settings_still_none_when_truly_empty() {
        assert!(ProjectConfig::from_settings(&ProjectSettings::default()).is_none());
    }
}
