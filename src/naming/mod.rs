//! LLM-powered session branch naming.
//!
//! Shells out to the coding agent binary (`claude -p` or `opencode run`) to
//! generate a meaningful 2-4 word branch name from the session's first prompt.
//! The agents use their own subscription auth — no separate API keys needed.
//! Falls back to keyword extraction when the binary isn't available or fails.

use serde::{Deserialize, Serialize};
use std::process::Command;
use tracing::info;

use crate::settings::AgentKind;

// ---------------------------------------------------------------------------
// Configuration types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamingConfig {
    #[serde(default)]
    pub mode: NamingMode,
    #[serde(default = "default_claude_config")]
    pub claude: NamingModelConfig,
    #[serde(default = "default_opencode_config")]
    pub opencode: NamingModelConfig,
}

impl Default for NamingConfig {
    fn default() -> Self {
        Self {
            mode: NamingMode::default(),
            claude: default_claude_config(),
            opencode: default_opencode_config(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamingMode {
    Auto,
    Interactive,
    Legacy,
}

impl Default for NamingMode {
    fn default() -> Self {
        Self::Auto
    }
}

impl NamingMode {
    pub fn label(self) -> &'static str {
        match self {
            NamingMode::Auto => "Auto",
            NamingMode::Interactive => "Interactive",
            NamingMode::Legacy => "Legacy",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            NamingMode::Auto => "LLM picks the name automatically",
            NamingMode::Interactive => "Show suggestions, let you choose",
            NamingMode::Legacy => "Keyword extraction (no network)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamingModelConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    api_base: Option<String>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    api_key_env: Option<String>,
}

fn default_claude_config() -> NamingModelConfig {
    NamingModelConfig {
        model: Some("claude-haiku-4-5-20251001".to_string()),
        api_base: None,
        api_key_env: None,
    }
}

fn default_opencode_config() -> NamingModelConfig {
    NamingModelConfig {
        model: Some("openai/gpt-4o-mini".to_string()),
        api_base: None,
        api_key_env: None,
    }
}

// ---------------------------------------------------------------------------
// Naming request/result
// ---------------------------------------------------------------------------

pub struct NamingRequest<'a> {
    pub prompt_text: &'a str,
    pub agent_kind: AgentKind,
    pub agent_binary: &'a str,
    pub short_id: &'a str,
    pub suggestions_count: usize,
}

pub struct NamingResult {
    pub suggestions: Vec<String>,
    pub branch_name: String,
    pub display_label: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate a branch name from the session's first prompt by shelling out to
/// the coding agent. Returns `Ok(NamingResult)` on success, or `Err(reason)`
/// on failure (caller falls back to keyword extraction).
pub fn generate(config: &NamingConfig, request: &NamingRequest) -> Result<NamingResult, String> {
    let model_config = match request.agent_kind {
        AgentKind::Claude => &config.claude,
        AgentKind::Opencode => &config.opencode,
        AgentKind::Generic => &config.claude,
    };

    let prompt_snippet = truncate_prompt(request.prompt_text, 500);
    let system = system_prompt(request.suggestions_count);
    let full_prompt = format!("{system}\n\nTask description:\n{prompt_snippet}");

    let raw_response = match request.agent_kind {
        AgentKind::Claude | AgentKind::Generic => {
            call_claude(request.agent_binary, model_config, &full_prompt)?
        }
        AgentKind::Opencode => {
            call_opencode(request.agent_binary, model_config, &full_prompt)?
        }
    };

    let suggestions = parse_suggestions(&raw_response, request.suggestions_count);
    if suggestions.is_empty() {
        return Err("Agent returned no valid branch names".to_string());
    }

    let slug = &suggestions[0];
    let branch_name = format!("{}-{}", slug, request.short_id);
    let display_label = slug_to_label(slug);

    info!("naming: agent generated branch_name={branch_name:?}, label={display_label:?}");

    Ok(NamingResult {
        suggestions,
        branch_name,
        display_label,
    })
}

/// Build the final branch name from a chosen slug and short ID.
pub fn branch_name_from_slug(slug: &str, short_id: &str) -> String {
    format!("{slug}-{short_id}")
}

/// Convert a slug like "fix-auth-redirect" to a display label like "Fix Auth Redirect".
pub fn slug_to_label(slug: &str) -> String {
    let full: String = slug
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.as_str())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if full.len() > 40 {
        let mut truncated = full[..40].to_string();
        if let Some(last_space) = truncated.rfind(' ') {
            truncated.truncate(last_space);
        }
        truncated
    } else {
        full
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn truncate_prompt(prompt: &str, max_chars: usize) -> String {
    if prompt.len() <= max_chars {
        prompt.to_string()
    } else {
        prompt[..max_chars].to_string()
    }
}

fn system_prompt(count: usize) -> String {
    if count <= 1 {
        "Generate a 2-4 word kebab-case git branch name for this coding task. \
         Return ONLY the slug. No quotes, no explanation, no backticks.\n\
         Examples: fix-auth-redirect, add-dark-mode, refactor-state-machine"
            .to_string()
    } else {
        format!(
            "Generate exactly {count} different 2-4 word kebab-case git branch name options \
             for this coding task. Return ONLY the slugs, one per line. \
             No quotes, no explanation, no backticks, no numbering.\n\
             Examples: fix-auth-redirect, add-dark-mode, refactor-state-machine"
        )
    }
}

fn call_claude(binary: &str, config: &NamingModelConfig, prompt: &str) -> Result<String, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("-p");
    if let Some(model) = &config.model {
        cmd.arg("--model").arg(model);
    }
    cmd.arg("--max-tokens").arg("60");
    cmd.arg(prompt);

    info!("naming: spawning claude -p (model={:?})", config.model);

    let output = cmd
        .output()
        .map_err(|e| format!("naming: failed to spawn claude: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("naming: claude exited with {}: {stderr}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err("naming: claude produced no output".to_string());
    }

    Ok(stdout)
}

fn call_opencode(binary: &str, config: &NamingModelConfig, prompt: &str) -> Result<String, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("run");
    if let Some(model) = &config.model {
        cmd.arg("--model").arg(model);
    }
    cmd.arg(prompt);

    info!("naming: spawning opencode run (model={:?})", config.model);

    let output = cmd
        .output()
        .map_err(|e| format!("naming: failed to spawn opencode: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("naming: opencode exited with {}: {stderr}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err("naming: opencode produced no output".to_string());
    }

    Ok(stdout)
}

/// Parse and validate LLM output into clean branch name slugs.
fn parse_suggestions(raw: &str, max_count: usize) -> Vec<String> {
    raw.lines()
        .filter_map(|line| validate_slug(line.trim()))
        .take(max_count)
        .collect()
}

/// Validate and clean a single slug candidate.
/// Returns `None` if the input is invalid.
fn validate_slug(raw: &str) -> Option<String> {
    let cleaned = raw
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '-' || c == '*')
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')' || c == ' ');

    let slug: String = cleaned
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.len() < 3 || slug.len() > 50 {
        return None;
    }

    if !slug.contains('-') {
        return None;
    }

    Some(slug)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_slug_basic() {
        assert_eq!(validate_slug("fix-auth-redirect"), Some("fix-auth-redirect".to_string()));
    }

    #[test]
    fn validate_slug_strips_quotes() {
        assert_eq!(validate_slug("\"fix-auth-bug\""), Some("fix-auth-bug".to_string()));
        assert_eq!(validate_slug("`add-dark-mode`"), Some("add-dark-mode".to_string()));
    }

    #[test]
    fn validate_slug_strips_numbering() {
        assert_eq!(validate_slug("1. fix-auth-redirect"), Some("fix-auth-redirect".to_string()));
        assert_eq!(validate_slug("2) add-dark-mode"), Some("add-dark-mode".to_string()));
    }

    #[test]
    fn validate_slug_normalizes_case() {
        assert_eq!(validate_slug("Fix-Auth-Redirect"), Some("fix-auth-redirect".to_string()));
    }

    #[test]
    fn validate_slug_rejects_too_short() {
        assert_eq!(validate_slug("ab"), None);
    }

    #[test]
    fn validate_slug_rejects_single_word() {
        assert_eq!(validate_slug("fix"), None);
    }

    #[test]
    fn validate_slug_converts_spaces() {
        assert_eq!(validate_slug("fix auth redirect"), Some("fix-auth-redirect".to_string()));
    }

    #[test]
    fn parse_suggestions_multiple() {
        let raw = "fix-auth-redirect\nadd-dark-mode\nrefactor-state-machine\n";
        let results = parse_suggestions(raw, 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], "fix-auth-redirect");
        assert_eq!(results[1], "add-dark-mode");
        assert_eq!(results[2], "refactor-state-machine");
    }

    #[test]
    fn parse_suggestions_with_noise() {
        let raw = "1. `fix-auth-redirect`\n2. \"add-dark-mode\"\n3. refactor-state-machine";
        let results = parse_suggestions(raw, 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], "fix-auth-redirect");
    }

    #[test]
    fn slug_to_label_basic() {
        assert_eq!(slug_to_label("fix-auth-redirect"), "Fix Auth Redirect");
    }

    #[test]
    fn branch_name_from_slug_format() {
        assert_eq!(branch_name_from_slug("fix-auth", "5dc47535"), "fix-auth-5dc47535");
    }

    #[test]
    fn legacy_config_with_api_key_env_deserializes() {
        let json = r#"{
            "mode": "auto",
            "claude": {
                "model": "claude-haiku-4-5-20251001",
                "api_base": null,
                "api_key_env": "ANTHROPIC_API_KEY"
            },
            "opencode": {
                "model": "gpt-4o-mini",
                "api_base": null,
                "api_key_env": "OPENAI_API_KEY"
            }
        }"#;
        let config: NamingConfig = serde_json::from_str(json).expect("should deserialize legacy config");
        assert_eq!(config.mode, NamingMode::Auto);
        assert_eq!(config.claude.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn new_config_without_legacy_fields_deserializes() {
        let json = r#"{
            "mode": "interactive",
            "claude": { "model": "claude-haiku-4-5-20251001" },
            "opencode": { "model": "openai/gpt-4o-mini" }
        }"#;
        let config: NamingConfig = serde_json::from_str(json).expect("should deserialize new config");
        assert_eq!(config.mode, NamingMode::Interactive);
        assert_eq!(config.opencode.model.as_deref(), Some("openai/gpt-4o-mini"));
    }
}
