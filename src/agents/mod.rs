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

mod opencode_plugin;

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

// ── Canonical event vocabulary ───────────────────────────────────────────
//
// This is the contract boundary between allele core and the agent adapters.
// Allele core speaks `Lifecycle`; each adapter translates its agent's native
// event vocabulary (Claude hook names, opencode plugin event types, …) into
// these canonical signals. `apply_hook_event` maps `Lifecycle` onto
// `SessionStatus`. Adding a new agent means implementing an adapter — never
// touching the core transition logic.

/// Canonical, agent-independent lifecycle signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Session started or its context was reset — becomes `Idle`.
    Start,
    /// Agent is actively working (a tool ran, the user submitted, the model
    /// is streaming) — becomes `Running` and clears any attention state.
    Busy,
    /// Agent is blocked on a permission prompt or an idle wait and needs the
    /// user to act — becomes `AwaitingInput`.
    AwaitingInput,
    /// Agent finished a response turn — becomes `ResponseReady`.
    TurnComplete,
    /// Session ended / context reset (real PTY exit is handled separately by
    /// the exit watcher) — becomes `Idle`.
    End,
    /// No status change.
    Ignore,
}

/// Directive for the per-session tool-context cache. Claude's `Notification`
/// hook doesn't carry the tool it wants to run (the preceding `PreToolUse`
/// does), so Claude caches on `PreToolUse` (`Set`) and clears on
/// `PostToolUse` (`Clear`). opencode carries the tool inline on its
/// permission event, so it never needs the cache (`Leave`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOp {
    Set,
    Clear,
    Leave,
}

/// An interpreted event: the canonical transition plus a cache directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSignal {
    pub lifecycle: Lifecycle,
    pub cache_op: CacheOp,
}

impl EventSignal {
    /// A no-op signal — unknown / uninteresting events resolve to this.
    pub const IGNORE: Self = Self {
        lifecycle: Lifecycle::Ignore,
        cache_op: CacheOp::Leave,
    };

    const fn just(lifecycle: Lifecycle) -> Self {
        Self {
            lifecycle,
            cache_op: CacheOp::Leave,
        }
    }

    const fn with_cache(lifecycle: Lifecycle, cache_op: CacheOp) -> Self {
        Self {
            lifecycle,
            cache_op,
        }
    }
}

/// Spawn-time event wiring for an agent: extra CLI args and env vars that
/// activate event emission into allele's shared events directory.
#[derive(Debug, Clone, Default)]
pub struct EventIntegration {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
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

    // ── Event integration ────────────────────────────────────────────────

    /// One-time, idempotent install of any on-disk assets this agent needs
    /// to emit lifecycle events into allele's shared events directory
    /// (Claude's `hooks.json`, opencode's plugin file, …). Called once at
    /// app startup. Default: nothing to install.
    fn install_integration(&self) -> std::io::Result<()> {
        Ok(())
    }

    /// Spawn-time wiring that activates event emission: extra CLI args
    /// and/or env vars applied to the session's PTY. Default: none.
    fn event_integration(&self, _ctx: &SpawnCtx) -> EventIntegration {
        EventIntegration::default()
    }

    /// Translate one native event `kind` string (as written to the shared
    /// events file) into a canonical [`EventSignal`]. Default: `Ignore`,
    /// so agents with no event integration never move a session's status.
    fn interpret_event(&self, _kind: &str) -> EventSignal {
        EventSignal::IGNORE
    }
}

pub struct ClaudeAdapter;
impl AgentAdapter for ClaudeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }
    fn default_display_name(&self) -> &'static str {
        "Claude"
    }
    fn binary_name(&self) -> &'static str {
        "claude"
    }
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
    fn supports_resume(&self) -> bool {
        true
    }
    fn build_new_session_args(&self, ctx: &SpawnCtx, extra: &[String]) -> Vec<String> {
        let mut args = vec![
            "--session-id".into(),
            ctx.session_id.into(),
            "--name".into(),
            ctx.label.into(),
        ];
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
        args.extend(extra.iter().cloned());
        args
    }

    /// Claude's hook receiver + `hooks.json` are installed by
    /// [`crate::hooks::install_if_missing`], driven from the startup path
    /// (it also returns the settings path the caller threads into
    /// [`SpawnCtx`]). Nothing extra to do here.
    fn install_integration(&self) -> std::io::Result<()> {
        Ok(())
    }

    /// Event emission is activated by pointing `claude --settings` at the
    /// installed hooks file. The path is threaded in via `SpawnCtx` so tests
    /// (and callers) stay in control of it.
    fn event_integration(&self, ctx: &SpawnCtx) -> EventIntegration {
        let mut integ = EventIntegration::default();
        if let Some(hooks) = ctx.hooks_settings_path {
            integ.args = vec!["--settings".into(), hooks.into()];
        }
        integ
    }

    /// Map Claude Code hook names onto canonical lifecycle signals. This is
    /// the single source of truth for Claude's status transitions.
    fn interpret_event(&self, kind: &str) -> EventSignal {
        match kind {
            "session_start" => EventSignal::just(Lifecycle::Start),
            "user_prompt_submit" => EventSignal::just(Lifecycle::Busy),
            // PreToolUse: Claude is executing a tool, which clears any prior
            // permission prompt. Cache the tool context to enrich a later
            // Notification (which doesn't carry the tool name itself).
            "pre_tool_use" => EventSignal::with_cache(Lifecycle::Busy, CacheOp::Set),
            // PostToolUse: tool finished — belt-and-suspenders clearing signal.
            // Drop the cached context so a later Notification doesn't inherit
            // a resolved tool.
            "post_tool_use" => EventSignal::with_cache(Lifecycle::Busy, CacheOp::Clear),
            "notification" => EventSignal::just(Lifecycle::AwaitingInput),
            "stop" => EventSignal::just(Lifecycle::TurnComplete),
            "session_end" => EventSignal::just(Lifecycle::End),
            _ => EventSignal::IGNORE,
        }
    }
}

pub struct OpencodeAdapter;
impl AgentAdapter for OpencodeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Opencode
    }
    fn default_display_name(&self) -> &'static str {
        "opencode"
    }
    fn binary_name(&self) -> &'static str {
        "opencode"
    }
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
    fn supports_resume(&self) -> bool {
        true
    }
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

    /// Install the allele events plugin into opencode's global plugin dir.
    /// Opencode auto-loads every file there at startup, so no per-spawn arg
    /// is needed — only the env bridge below.
    fn install_integration(&self) -> std::io::Result<()> {
        opencode_plugin::install()
    }

    /// opencode has no `--settings`-style hook flag; instead its plugin runs
    /// inside the opencode process and reads these env vars to know which
    /// allele session it belongs to and where to write events. Each session
    /// runs in its own clone/process, so the id is unambiguous per PTY.
    fn event_integration(&self, ctx: &SpawnCtx) -> EventIntegration {
        let mut env = vec![("ALLELE_SESSION_ID".to_string(), ctx.session_id.to_string())];
        if let Some(dir) = crate::hooks::events_dir() {
            env.push((
                "ALLELE_EVENTS_DIR".to_string(),
                dir.to_string_lossy().to_string(),
            ));
        }
        EventIntegration {
            args: Vec::new(),
            env,
        }
    }

    /// Map the canonical `kind` strings the allele opencode plugin writes
    /// onto lifecycle signals. The plugin already normalises opencode's
    /// native event types (`session.idle`, `permission.asked`, …) to these
    /// names, so this mapping is deliberately thin. opencode carries tool
    /// context inline on its permission event, so no cache dance is needed.
    fn interpret_event(&self, kind: &str) -> EventSignal {
        match kind {
            "session_start" => EventSignal::just(Lifecycle::Start),
            "user_prompt_submit" => EventSignal::just(Lifecycle::Busy),
            "busy" => EventSignal::just(Lifecycle::Busy),
            "awaiting_input" => EventSignal::just(Lifecycle::AwaitingInput),
            "turn_complete" => EventSignal::just(Lifecycle::TurnComplete),
            "session_end" => EventSignal::just(Lifecycle::End),
            _ => EventSignal::IGNORE,
        }
    }
}

pub struct GenericAdapter;
impl AgentAdapter for GenericAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Generic
    }
    fn default_display_name(&self) -> &'static str {
        "Custom"
    }
    fn binary_name(&self) -> &'static str {
        ""
    }
    fn probe_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }
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
pub fn build_command(agent: &AgentConfig, ctx: &SpawnCtx, resume: bool) -> Option<ShellCommand> {
    if !agent.enabled {
        return None;
    }
    let path = agent.path.as_ref()?.clone();
    if path.trim().is_empty() {
        return None;
    }
    let adapter = adapter_for(agent.kind);
    let mut args = if resume && adapter.supports_resume() {
        adapter.build_resume_args(ctx, &agent.extra_args)
    } else {
        adapter.build_new_session_args(ctx, &agent.extra_args)
    };
    // Layer in the adapter's event-integration wiring (Claude's --settings,
    // opencode's ALLELE_SESSION_ID env, …) so status reporting works.
    let integration = adapter.event_integration(ctx);
    args.extend(integration.args);
    Some(ShellCommand::with_args_env(path, args, integration.env))
}

/// Install every built-in adapter's on-disk event integration. Idempotent;
/// called once at app startup alongside [`crate::hooks::install_if_missing`].
/// Failures are logged and swallowed — a missing integration degrades status
/// reporting for that agent but must never block the app from launching.
pub fn install_integrations() {
    for kind in [AgentKind::Claude, AgentKind::Opencode, AgentKind::Generic] {
        if let Err(e) = adapter_for(kind).install_integration() {
            tracing::warn!("agent integration install failed for {kind:?}: {e}");
        }
    }
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
    let find = |id: &str| {
        agents
            .iter()
            .find(|a| a.id == id && a.enabled && a.path.is_some())
    };
    if let Some(id) = explicit_id {
        if let Some(a) = find(id) {
            return Some(a);
        }
    }
    if let Some(id) = project_override {
        if let Some(a) = find(id) {
            return Some(a);
        }
    }
    if let Some(id) = default_id {
        if let Some(a) = find(id) {
            return Some(a);
        }
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
                "--session-id",
                "abc",
                "--name",
                "Claude 1",
                "--settings",
                "/tmp/hooks.json",
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
        assert_eq!(
            cmd.args.last().map(String::as_str),
            Some("--dangerously-skip-permissions")
        );
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
        let ctx = SpawnCtx {
            session_id: "a",
            label: "l",
            hooks_settings_path: None,
            has_history: false,
        };
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
    fn claude_interpret_event_maps_hook_names() {
        let a = ClaudeAdapter;
        assert_eq!(
            a.interpret_event("session_start").lifecycle,
            Lifecycle::Start
        );
        assert_eq!(
            a.interpret_event("user_prompt_submit").lifecycle,
            Lifecycle::Busy
        );
        assert_eq!(
            a.interpret_event("notification").lifecycle,
            Lifecycle::AwaitingInput
        );
        assert_eq!(a.interpret_event("stop").lifecycle, Lifecycle::TurnComplete);
        assert_eq!(a.interpret_event("session_end").lifecycle, Lifecycle::End);
        // PreToolUse caches tool context; PostToolUse clears it.
        let pre = a.interpret_event("pre_tool_use");
        assert_eq!(pre.lifecycle, Lifecycle::Busy);
        assert_eq!(pre.cache_op, CacheOp::Set);
        let post = a.interpret_event("post_tool_use");
        assert_eq!(post.lifecycle, Lifecycle::Busy);
        assert_eq!(post.cache_op, CacheOp::Clear);
        // Unknown kinds are ignored.
        assert_eq!(a.interpret_event("wat").lifecycle, Lifecycle::Ignore);
    }

    #[test]
    fn opencode_interpret_event_maps_canonical_names() {
        let a = OpencodeAdapter;
        assert_eq!(
            a.interpret_event("session_start").lifecycle,
            Lifecycle::Start
        );
        assert_eq!(a.interpret_event("busy").lifecycle, Lifecycle::Busy);
        assert_eq!(
            a.interpret_event("awaiting_input").lifecycle,
            Lifecycle::AwaitingInput
        );
        assert_eq!(
            a.interpret_event("turn_complete").lifecycle,
            Lifecycle::TurnComplete
        );
        assert_eq!(a.interpret_event("session_end").lifecycle, Lifecycle::End);
        // opencode never uses the tool-context cache.
        assert_eq!(a.interpret_event("busy").cache_op, CacheOp::Leave);
        assert_eq!(a.interpret_event("nope").lifecycle, Lifecycle::Ignore);
    }

    #[test]
    fn opencode_event_integration_sets_session_env() {
        let a = OpencodeAdapter;
        let ctx = SpawnCtx {
            session_id: "sid-123",
            label: "opencode 1",
            hooks_settings_path: None,
            has_history: false,
        };
        let integ = a.event_integration(&ctx);
        assert!(integ.args.is_empty(), "opencode needs no extra CLI args");
        let sid = integ
            .env
            .iter()
            .find(|(k, _)| k == "ALLELE_SESSION_ID")
            .map(|(_, v)| v.as_str());
        assert_eq!(sid, Some("sid-123"));
    }

    #[test]
    fn opencode_command_carries_session_env() {
        let agent = cfg("opencode", AgentKind::Opencode, Some("/bin/true"), true);
        let ctx = SpawnCtx {
            session_id: "sid-xyz",
            label: "opencode 1",
            hooks_settings_path: None,
            has_history: false,
        };
        let cmd = build_command(&agent, &ctx, false).expect("agent has path");
        assert!(cmd
            .env
            .iter()
            .any(|(k, v)| k == "ALLELE_SESSION_ID" && v == "sid-xyz"));
    }

    #[test]
    fn generic_adapter_ignores_all_events() {
        let a = GenericAdapter;
        assert_eq!(a.interpret_event("stop").lifecycle, Lifecycle::Ignore);
        assert_eq!(
            a.interpret_event("session_start").lifecycle,
            Lifecycle::Ignore
        );
        assert!(a
            .event_integration(&SpawnCtx {
                session_id: "x",
                label: "y",
                hooks_settings_path: None,
                has_history: false,
            })
            .env
            .is_empty());
    }

    #[test]
    fn resolve_skips_disabled_or_unpathed_entries() {
        let agents = vec![
            cfg("claude", AgentKind::Claude, None, true), // no path
            cfg("opencode", AgentKind::Opencode, Some("/b"), false), // disabled
            cfg("mystery", AgentKind::Generic, Some("/c"), true),
        ];
        let a = resolve(&agents, Some("claude"), None, None).unwrap();
        assert_eq!(a.id, "mystery");
    }
}
