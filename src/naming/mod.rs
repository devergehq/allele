//! LLM-powered session branch naming.
//!
//! Calls a fast/cheap model (Haiku for Claude agents, GPT-4o-mini for OpenCode)
//! to generate a meaningful 2-4 word branch name from the session's first prompt.
//! Falls back to keyword extraction when no API key is available or the call fails.

use serde::{Deserialize, Serialize};
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
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            NamingMode::Auto => "Auto",
            NamingMode::Interactive => "Interactive",
            NamingMode::Legacy => "Legacy",
        }
    }

    #[allow(dead_code)]
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
    pub model: String,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub api_key_env: String,
}

fn default_claude_config() -> NamingModelConfig {
    NamingModelConfig {
        model: "claude-haiku-4-5-20251001".to_string(),
        api_base: None,
        api_key_env: "ANTHROPIC_API_KEY".to_string(),
    }
}

fn default_opencode_config() -> NamingModelConfig {
    NamingModelConfig {
        model: "gpt-4o-mini".to_string(),
        api_base: None,
        api_key_env: "OPENAI_API_KEY".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Naming request/result
// ---------------------------------------------------------------------------

pub struct NamingRequest<'a> {
    pub prompt_text: &'a str,
    pub agent_kind: AgentKind,
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

/// Generate a branch name from the session's first prompt using the configured LLM.
/// Returns `Ok(NamingResult)` on success, or `Err(reason)` on failure (caller falls back).
pub fn generate(config: &NamingConfig, request: &NamingRequest) -> Result<NamingResult, String> {
    let model_config = match request.agent_kind {
        AgentKind::Claude => &config.claude,
        AgentKind::Opencode => &config.opencode,
        AgentKind::Generic => &config.claude,
    };

    let api_key = resolve_api_key(model_config)?;
    let prompt_snippet = truncate_prompt(request.prompt_text, 500);

    let raw_response = match request.agent_kind {
        AgentKind::Claude | AgentKind::Generic => {
            call_anthropic(&prompt_snippet, model_config, &api_key, request.suggestions_count)?
        }
        AgentKind::Opencode => {
            call_openai_compatible(&prompt_snippet, model_config, &api_key, request.suggestions_count)?
        }
    };

    let suggestions = parse_suggestions(&raw_response, request.suggestions_count);
    if suggestions.is_empty() {
        return Err("LLM returned no valid branch names".to_string());
    }

    let slug = &suggestions[0];
    let branch_name = format!("{}-{}", slug, request.short_id);
    let display_label = slug_to_label(slug);

    info!("naming: LLM generated branch_name={branch_name:?}, label={display_label:?}");

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

fn resolve_api_key(config: &NamingModelConfig) -> Result<String, String> {
    std::env::var(&config.api_key_env).map_err(|_| {
        format!(
            "naming: {} not set in environment",
            config.api_key_env
        )
    })
}

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

fn build_agent() -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .build();
    config.new_agent()
}

fn call_anthropic(
    prompt: &str,
    config: &NamingModelConfig,
    api_key: &str,
    count: usize,
) -> Result<String, String> {
    let base = config
        .api_base
        .as_deref()
        .unwrap_or("https://api.anthropic.com");
    let url = format!("{base}/v1/messages");

    let body = serde_json::json!({
        "model": config.model,
        "max_tokens": 60,
        "system": system_prompt(count),
        "messages": [{"role": "user", "content": prompt}]
    });

    info!("naming: calling Anthropic API model={}", config.model);

    let agent = build_agent();
    let mut response = agent
        .post(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|e| format!("naming: Anthropic API error: {e}"))?;

    let resp_body: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|e| format!("naming: failed to parse Anthropic response: {e}"))?;

    resp_body["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "naming: no text in Anthropic response".to_string())
}

fn call_openai_compatible(
    prompt: &str,
    config: &NamingModelConfig,
    api_key: &str,
    count: usize,
) -> Result<String, String> {
    let base = config
        .api_base
        .as_deref()
        .unwrap_or("https://api.openai.com");
    let url = format!("{base}/v1/chat/completions");

    let body = serde_json::json!({
        "model": config.model,
        "max_tokens": 60,
        "messages": [
            {"role": "system", "content": system_prompt(count)},
            {"role": "user", "content": prompt}
        ]
    });

    info!("naming: calling OpenAI-compatible API model={}", config.model);

    let agent = build_agent();
    let mut response = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(|e| format!("naming: OpenAI API error: {e}"))?;

    let resp_body: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|e| format!("naming: failed to parse OpenAI response: {e}"))?;

    resp_body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "naming: no content in OpenAI response".to_string())
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
    // Strip common LLM noise: quotes, backticks, numbering, bullet points
    let cleaned = raw
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '-' || c == '*')
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ')' || c == ' ');

    // Normalize to lowercase kebab-case
    let slug: String = cleaned
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    // Validate length
    if slug.len() < 3 || slug.len() > 50 {
        return None;
    }

    // Must contain at least one hyphen (2+ words)
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
}
