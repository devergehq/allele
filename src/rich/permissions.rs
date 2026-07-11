//! Permission & decision model for the narrative (DEV-34).
//!
//! When an agent blocks on a permission prompt, the narrative should show a
//! *card* the user can understand and act on — what tool wants to run, on what
//! target, why, and how risky it is — and it should keep a durable record of
//! what the user decided. This module is the view-agnostic core of that:
//!
//!   * [`PermissionRequest`] — a normalized prompt (tool, target, purpose,
//!     [`RiskLevel`]) built from a tool name + input.
//!   * [`PermissionAction`] — allow / reject / open-terminal, with
//!     [`available_actions`] deciding which apply to a given request.
//!   * [`DecisionLog`] — the retained history of resolved requests.
//!   * [`requires_input`] — distinguishes prompts that need a human from
//!     routine notifications.
//!
//! Pure and unit-tested; the card rendering and hook wiring live in the view
//! and app-state layers.

/// How dangerous granting a request is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn label(self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }
}

/// A normalized permission prompt ready to render as a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool_name: String,
    /// The thing being acted on: a file path, a shell command, a URL.
    pub target: Option<String>,
    /// A short human-readable purpose ("edit a file", "run a shell command").
    pub purpose: String,
    pub risk: RiskLevel,
}

impl PermissionRequest {
    /// Build a request from a tool name and its JSON input.
    pub fn from_tool(tool_name: &str, input: &serde_json::Value) -> Self {
        let target = extract_target(tool_name, input);
        let risk = assess_risk(tool_name, target.as_deref());
        let purpose = describe_purpose(tool_name);
        PermissionRequest {
            tool_name: tool_name.to_string(),
            target,
            purpose,
            risk,
        }
    }
}

/// An action the user can take on a permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    Allow,
    Reject,
    /// Drop into the underlying terminal to handle it manually.
    OpenTerminal,
}

/// Which actions apply to a request. Allow/Reject are always offered;
/// open-terminal is offered only when a real terminal is attached (so an
/// isolated/headless session doesn't advertise a dead button).
pub fn available_actions(
    _req: &PermissionRequest,
    terminal_available: bool,
) -> Vec<PermissionAction> {
    let mut actions = vec![PermissionAction::Allow, PermissionAction::Reject];
    if terminal_available {
        actions.push(PermissionAction::OpenTerminal);
    }
    actions
}

/// A resolved request plus what the user did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub request: PermissionRequest,
    pub action: PermissionAction,
    /// Ledger sequence at which the decision was recorded (audit anchor).
    pub at_seq: usize,
}

/// Durable history of permission decisions, retained alongside the ledger so
/// the audit trail survives even as the live prompt card is dismissed.
#[derive(Debug, Default, Clone)]
pub struct DecisionLog {
    decisions: Vec<Decision>,
}

impl DecisionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, request: PermissionRequest, action: PermissionAction, at_seq: usize) {
        self.decisions.push(Decision {
            request,
            action,
            at_seq,
        });
    }

    pub fn decisions(&self) -> &[Decision] {
        &self.decisions
    }

    pub fn len(&self) -> usize {
        self.decisions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
    }

    /// The most recent decision for a given tool, if any — lets the UI show
    /// "you allowed this last time".
    pub fn last_for_tool(&self, tool_name: &str) -> Option<&Decision> {
        self.decisions
            .iter()
            .rev()
            .find(|d| d.request.tool_name == tool_name)
    }
}

/// Whether a hook/notification actually needs human input, versus routine
/// status noise. Only genuine permission/idle waits should raise a card.
pub fn requires_input(hook_event: &str) -> bool {
    matches!(
        hook_event,
        "Notification" | "PermissionRequest" | "AwaitingInput"
    )
}

/// Extract the acted-on target from a tool input.
fn extract_target(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let key = match tool_name {
        "Bash" | "bash" => "command",
        "WebFetch" | "WebSearch" => "url",
        _ => "file_path",
    };
    input
        .get(key)
        .and_then(|v| v.as_str())
        .or_else(|| input.get("path").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn describe_purpose(tool_name: &str) -> String {
    match tool_name {
        "Read" | "read_file" => "read a file",
        "Write" | "write_file" => "create or overwrite a file",
        "Edit" | "edit_file" | "MultiEdit" => "modify a file",
        "Bash" | "bash" => "run a shell command",
        "WebFetch" => "fetch a URL",
        "WebSearch" => "search the web",
        _ => "use a tool",
    }
    .to_string()
}

/// Assess how risky granting a request is. Destructive shell commands and
/// writes/edits carry more risk than reads and searches.
pub fn assess_risk(tool_name: &str, target: Option<&str>) -> RiskLevel {
    match tool_name {
        "Read" | "read_file" | "Grep" | "Glob" | "LS" | "WebSearch" | "WebFetch" => RiskLevel::Low,
        "Write" | "write_file" | "Edit" | "edit_file" | "MultiEdit" | "apply_patch" => {
            RiskLevel::Medium
        }
        "Bash" | "bash" => match target {
            Some(cmd) if is_destructive_command(cmd) => RiskLevel::High,
            Some(_) => RiskLevel::Medium,
            None => RiskLevel::Medium,
        },
        _ => RiskLevel::Medium,
    }
}

/// Heuristic: does this shell command look destructive / irreversible?
fn is_destructive_command(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    const DANGER: &[&str] = &[
        "rm -rf",
        "rm -r",
        "rm -f",
        "sudo ",
        "mkfs",
        "dd if=",
        ":(){",
        "> /dev/",
        "git push --force",
        "git push -f",
        "git reset --hard",
        "git clean -",
        "chmod -r",
        "chown -r",
        "killall",
        "shutdown",
        "reboot",
        "truncate ",
    ];
    DANGER.iter().any(|d| c.contains(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_are_low_writes_are_medium() {
        assert_eq!(assess_risk("Read", Some("/a")), RiskLevel::Low);
        assert_eq!(assess_risk("Write", Some("/a")), RiskLevel::Medium);
        assert_eq!(assess_risk("Edit", Some("/a")), RiskLevel::Medium);
    }

    #[test]
    fn destructive_shell_is_high_risk() {
        assert_eq!(assess_risk("Bash", Some("rm -rf build/")), RiskLevel::High);
        assert_eq!(
            assess_risk("Bash", Some("git push --force origin main")),
            RiskLevel::High
        );
        assert_eq!(
            assess_risk("Bash", Some("sudo rm /etc/hosts")),
            RiskLevel::High
        );
        // A benign command is only medium.
        assert_eq!(assess_risk("Bash", Some("ls -la")), RiskLevel::Medium);
    }

    #[test]
    fn request_from_tool_extracts_target_and_risk() {
        let req = PermissionRequest::from_tool("Edit", &json!({"file_path": "/src/main.rs"}));
        assert_eq!(req.target.as_deref(), Some("/src/main.rs"));
        assert_eq!(req.risk, RiskLevel::Medium);
        assert_eq!(req.purpose, "modify a file");

        let bash = PermissionRequest::from_tool("Bash", &json!({"command": "rm -rf node_modules"}));
        assert_eq!(bash.target.as_deref(), Some("rm -rf node_modules"));
        assert_eq!(bash.risk, RiskLevel::High);
    }

    #[test]
    fn open_terminal_only_when_available() {
        let req = PermissionRequest::from_tool("Bash", &json!({"command": "make"}));
        let with = available_actions(&req, true);
        assert!(with.contains(&PermissionAction::OpenTerminal));
        let without = available_actions(&req, false);
        assert!(!without.contains(&PermissionAction::OpenTerminal));
        assert!(without.contains(&PermissionAction::Allow));
        assert!(without.contains(&PermissionAction::Reject));
    }

    #[test]
    fn decision_log_retains_history_and_finds_last() {
        let mut log = DecisionLog::new();
        assert!(log.is_empty());
        let r1 = PermissionRequest::from_tool("Bash", &json!({"command": "ls"}));
        log.record(r1, PermissionAction::Allow, 10);
        let r2 = PermissionRequest::from_tool("Write", &json!({"file_path": "/a"}));
        log.record(r2, PermissionAction::Reject, 20);
        let r3 = PermissionRequest::from_tool("Bash", &json!({"command": "pwd"}));
        log.record(r3, PermissionAction::Allow, 30);

        assert_eq!(log.len(), 3);
        let last_bash = log.last_for_tool("Bash").unwrap();
        assert_eq!(last_bash.at_seq, 30);
        assert_eq!(last_bash.action, PermissionAction::Allow);
    }

    #[test]
    fn requires_input_distinguishes_prompts_from_noise() {
        assert!(requires_input("Notification"));
        assert!(requires_input("PermissionRequest"));
        assert!(!requires_input("PostToolUse"));
        assert!(!requires_input("Stop"));
    }
}
