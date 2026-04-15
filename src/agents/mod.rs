//! Coding agent registry.
//!
//! Each supported agent is backed by an `AgentAdapter` that knows how to
//! probe for the binary and how to turn a spawn context into the right
//! command line (new-session vs resume). Settings stores a list of
//! configured agents keyed by a stable `id`; an adapter kind drives the
//! command-building behaviour. `allele.json` can override the active
//! agent per project via an `"agent"` field.
//!
//! Built-in adapters: `claude`, `opencode`. Unknown ids fall back to the
//! `generic` adapter, which just runs the configured binary with the
//! user's extra args and has no resume semantics.

use std::path::PathBuf;

use crate::settings::{AgentConfig, AgentKind};
use crate::terminal::ShellCommand;

/// Inputs needed to build a spawn command.
pub struct SpawnCtx<'a> {
    pub session_id: &'a str,
    pub label: &'a str,
    pub hooks_settings_path: Option<&'a str>,
    /// True when the underlying agent has on-disk history for `session_id`
    /// and the caller wants a resume. Ignored by adapters that don't
    /// distinguish between fresh and resumed sessions.
    pub has_history: bool,
}

/// Per-kind command-building behaviour.
#[allow(dead_code)]
pub trait AgentAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn default_display_name(&self) -> &'static str;
    fn binary_name(&self) -> &'static str;
    /// Ordered list of absolute paths to probe for the binary. Checked
    /// before falling back to `which::which(binary_name)`.
    fn probe_paths(&self) -> Vec<PathBuf>;
    /// Build args for a brand-new session.
    fn build_new_session_args(&self, ctx: &SpawnCtx, extra: &[String]) -> Vec<String>;
    /// Build args for resuming an existing session. Adapters that don't
    /// support resume should return the same args as `build_new_session_args`.
    fn build_resume_args(&self, ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        self.build_new_session_args(ctx, extra)
    }
    /// Whether this adapter knows how to resume a session from an id.
    fn supports_resume(&self) -> bool {
        false
    }
}

pub struct ClaudeAdapter;
impl AgentAdapter for ClaudeAdapter {
    fn kind(&self) -> AgentKind { AgentKind::Claude }
    fn default_display_name(&self) -> &'static str { "Claude" }
    fn binary_name(&self) -> &'static str { "claude" }
    fn probe_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(h) = dirs::home_dir() {
            out.push(h.join(".local/bin/claude"));
            out.push(h.join(".npm/bin/claude"));
        }
        out.push(PathBuf::from("/opt/homebrew/bin/claude"));
        out.push(PathBuf::from("/usr/local/bin/claude"));
        out
    }
    fn supports_resume(&self) -> bool { true }
    fn build_new_session_args(&self, ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        let mut args = vec![
            "--session-id".into(),
            ctx.session_id.into(),
            "--name".into(),
            ctx.label.into(),
        ];
        if let Some(hooks) = ctx.hooks_settings_path {
            args.push("--settings".into());
            args.push(hooks.into());
        }
        args.extend(extra.iter().cloned());
        args
    }
    fn build_resume_args(&self, ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        let mut args = if ctx.has_history {
            vec!["--resume".into(), ctx.session_id.into()]
        } else {
            vec!["--session-id".into(), ctx.session_id.into()]
        };
        args.push("--name".into());
        args.push(ctx.label.into());
        if let Some(hooks) = ctx.hooks_settings_path {
            args.push("--settings".into());
            args.push(hooks.into());
        }
        args.extend(extra.iter().cloned());
        args
    }
}

pub struct OpencodeAdapter;
impl AgentAdapter for OpencodeAdapter {
    fn kind(&self) -> AgentKind { AgentKind::Opencode }
    fn default_display_name(&self) -> &'static str { "opencode" }
    fn binary_name(&self) -> &'static str { "opencode" }
    fn probe_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(h) = dirs::home_dir() {
            out.push(h.join(".local/bin/opencode"));
            out.push(h.join(".npm/bin/opencode"));
        }
        out.push(PathBuf::from("/opt/homebrew/bin/opencode"));
        out.push(PathBuf::from("/usr/local/bin/opencode"));
        out
    }
    fn supports_resume(&self) -> bool { true }
    fn build_new_session_args(&self, _ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        extra.to_vec()
    }
    fn build_resume_args(&self, _ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        // opencode's `--continue` picks up the most recent session in cwd —
        // which matches the per-clone isolation model: each session has its
        // own clone, so "most recent in cwd" always resolves to this session.
        let mut args = vec!["--continue".into()];
        args.extend(extra.iter().cloned());
        args
    }
}

pub struct GenericAdapter;
impl AgentAdapter for GenericAdapter {
    fn kind(&self) -> AgentKind { AgentKind::Generic }
    fn default_display_name(&self) -> &'static str { "Custom" }
    fn binary_name(&self) -> &'static str { "" }
    fn probe_paths(&self) -> Vec<PathBuf> { Vec::new() }
    fn build_new_session_args(&self, _ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        extra.to_vec()
    }
}

pub fn adapter_for(kind: AgentKind) -> Box<dyn AgentAdapter> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAdapter),
        AgentKind::Opencode => Box::new(OpencodeAdapter),
        AgentKind::Generic => Box::new(GenericAdapter),
    }
}

/// Probe the filesystem for an adapter's binary. Returns the first hit in
/// the probe list, else whatever `which` finds on PATH.
pub fn detect_path(kind: AgentKind) -> Option<PathBuf> {
    let adapter = adapter_for(kind);
    for cand in adapter.probe_paths() {
        if cand.exists() {
            return Some(cand);
        }
    }
    let name = adapter.binary_name();
    if name.is_empty() {
        return None;
    }
    which::which(name).ok()
}

/// Build an initial agents list by probing each built-in kind. `claude`
/// is listed first and set as the default; opencode follows. Only the
/// `claude` entry is enabled by default — opencode is listed disabled so
/// its path is discoverable without activating it.
pub fn seed_agents() -> Vec<AgentConfig> {
    let claude_path = detect_path(AgentKind::Claude).map(|p| p.to_string_lossy().to_string());
    let opencode_path = detect_path(AgentKind::Opencode).map(|p| p.to_string_lossy().to_string());
    vec![
        AgentConfig {
            id: "claude".to_string(),
            kind: AgentKind::Claude,
            display_name: "Claude".to_string(),
            path: claude_path,
            extra_args: Vec::new(),
            enabled: true,
        },
        AgentConfig {
            id: "opencode".to_string(),
            kind: AgentKind::Opencode,
            display_name: "opencode".to_string(),
            path: opencode_path,
            extra_args: Vec::new(),
            enabled: false,
        },
    ]
}

/// Resolve an agent to a spawnable command. Returns `None` when the agent
/// has no path (not installed / not overridden) or is disabled.
pub fn build_command(
    agent: &AgentConfig,
    ctx: &SpawnCtx,
    resume: bool,
) -> Option<ShellCommand> {
    if !agent.enabled {
        return None;
    }
    let path = agent.path.as_ref()?.clone();
    if path.trim().is_empty() {
        return None;
    }
    let adapter = adapter_for(agent.kind);
    let args = if resume && adapter.supports_resume() {
        adapter.build_resume_args(ctx, &agent.extra_args)
    } else {
        adapter.build_new_session_args(ctx, &agent.extra_args)
    };
    Some(ShellCommand::with_args(path, args))
}

/// Pick the agent that should run for a given project, respecting the
/// `allele.json` override. Falls back to the settings default, then to
/// the first enabled agent. Returns `None` if nothing is available.
#[allow(clippy::needless_lifetimes)]
pub fn resolve<'a>(
    agents: &'a [AgentConfig],
    default_id: Option<&str>,
    project_override: Option<&str>,
    explicit_id: Option<&str>,
) -> Option<&'a AgentConfig> {
    let find = |id: &str| agents.iter().find(|a| a.id == id && a.enabled && a.path.is_some());
    if let Some(id) = explicit_id {
        if let Some(a) = find(id) { return Some(a); }
    }
    if let Some(id) = project_override {
        if let Some(a) = find(id) { return Some(a); }
    }
    if let Some(id) = default_id {
        if let Some(a) = find(id) { return Some(a); }
    }
    agents.iter().find(|a| a.enabled && a.path.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(id: &str, kind: AgentKind, path: Option<&str>, enabled: bool) -> AgentConfig {
        AgentConfig {
            id: id.into(),
            kind,
            display_name: id.into(),
            path: path.map(String::from),
            extra_args: Vec::new(),
            enabled,
        }
    }

    #[test]
    fn claude_new_session_builds_session_id_args() {
        let agent = cfg("claude", AgentKind::Claude, Some("/bin/true"), true);
        let ctx = SpawnCtx {
            session_id: "abc",
            label: "Claude 1",
            hooks_settings_path: Some("/tmp/hooks.json"),
            has_history: false,
        };
        let cmd = build_command(&agent, &ctx, false).expect("agent has path");
        assert_eq!(cmd.program, "/bin/true");
        assert_eq!(
            cmd.args,
            vec![
                "--session-id", "abc", "--name", "Claude 1",
                "--settings", "/tmp/hooks.json",
            ]
        );
    }

    #[test]
    fn claude_resume_with_history_uses_resume_flag() {
        let agent = cfg("claude", AgentKind::Claude, Some("/bin/true"), true);
        let ctx = SpawnCtx {
            session_id: "abc",
            label: "Claude 1",
            hooks_settings_path: None,
            has_history: true,
        };
        let cmd = build_command(&agent, &ctx, true).expect("agent has path");
        assert_eq!(cmd.args, vec!["--resume", "abc", "--name", "Claude 1"]);
    }

    #[test]
    fn claude_resume_without_history_falls_back_to_session_id() {
        let agent = cfg("claude", AgentKind::Claude, Some("/bin/true"), true);
        let ctx = SpawnCtx {
            session_id: "abc",
            label: "Claude 1",
            hooks_settings_path: None,
            has_history: false,
        };
        let cmd = build_command(&agent, &ctx, true).expect("agent has path");
        assert_eq!(cmd.args, vec!["--session-id", "abc", "--name", "Claude 1"]);
    }

    #[test]
    fn extra_args_are_appended() {
        let mut agent = cfg("claude", AgentKind::Claude, Some("/bin/true"), true);
        agent.extra_args = vec!["--dangerously-skip-permissions".into()];
        let ctx = SpawnCtx {
            session_id: "abc",
            label: "C",
            hooks_settings_path: None,
            has_history: false,
        };
        let cmd = build_command(&agent, &ctx, false).expect("agent has path");
        assert_eq!(cmd.args.last().map(String::as_str), Some("--dangerously-skip-permissions"));
    }

    #[test]
    fn generic_adapter_runs_binary_with_args_only() {
        let mut agent = cfg("shell", AgentKind::Generic, Some("/bin/bash"), true);
        agent.extra_args = vec!["-l".into()];
        let ctx = SpawnCtx {
            session_id: "abc",
            label: "Custom",
            hooks_settings_path: Some("/ignored"),
            has_history: false,
        };
        let cmd = build_command(&agent, &ctx, false).expect("agent has path");
        assert_eq!(cmd.program, "/bin/bash");
        assert_eq!(cmd.args, vec!["-l"]);
    }

    #[test]
    fn disabled_or_missing_path_returns_none() {
        let disabled = cfg("claude", AgentKind::Claude, Some("/bin/true"), false);
        let ctx = SpawnCtx { session_id: "a", label: "l", hooks_settings_path: None, has_history: false };
        assert!(build_command(&disabled, &ctx, false).is_none());
        let no_path = cfg("claude", AgentKind::Claude, None, true);
        assert!(build_command(&no_path, &ctx, false).is_none());
    }

    #[test]
    fn resolve_prefers_explicit_then_project_then_default() {
        let agents = vec![
            cfg("claude", AgentKind::Claude, Some("/a"), true),
            cfg("opencode", AgentKind::Opencode, Some("/b"), true),
        ];
        // Default fallback.
        let a = resolve(&agents, Some("claude"), None, None).unwrap();
        assert_eq!(a.id, "claude");
        // Project override wins over default.
        let a = resolve(&agents, Some("claude"), Some("opencode"), None).unwrap();
        assert_eq!(a.id, "opencode");
        // Explicit (stored on session) wins over project override.
        let a = resolve(&agents, Some("claude"), Some("opencode"), Some("claude")).unwrap();
        assert_eq!(a.id, "claude");
    }

    #[test]
    fn resolve_skips_disabled_or_unpathed_entries() {
        let agents = vec![
            cfg("claude", AgentKind::Claude, None, true),      // no path
            cfg("opencode", AgentKind::Opencode, Some("/b"), false), // disabled
            cfg("mystery", AgentKind::Generic, Some("/c"), true),
        ];
        let a = resolve(&agents, Some("claude"), None, None).unwrap();
        assert_eq!(a.id, "mystery");
    }
}
