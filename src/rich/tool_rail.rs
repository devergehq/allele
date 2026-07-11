//! Tool-activity classification for the routine rail (DEV-35).
//!
//! Most tool calls in a session are routine reads (Read, Grep, Glob, LS) and
//! shell probes (Bash) — individually low-signal but collectively noisy.
//! Mutations (Write, Edit, diffs) are the opposite: few but high-signal, the
//! things a reviewer actually cares about. This module classifies a tool by
//! name so the renderer can:
//!
//!   * collapse routine calls into a compact, expandable rail,
//!   * keep mutations prominent (never rail-collapsed),
//!   * auto-expand anything that errored, and
//!   * aggregate a run of routine calls into a one-line summary.
//!
//! Pure and name-driven so it is trivially unit-testable and shared between
//! the live document and any future transcript reader.

/// How prominent a tool call should be in the narrative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Low-signal reads/searches/shell — collapse into the rail by default.
    Routine,
    /// Writes/edits/diffs — file mutations, always kept prominent.
    Mutation,
    /// Everything else (task delegation, MCP tools, unknown) — shown, but not
    /// forced prominent.
    Notable,
}

impl ToolClass {
    /// Whether calls of this class collapse into the routine rail by default.
    pub fn is_routine(self) -> bool {
        matches!(self, ToolClass::Routine)
    }

    /// Whether calls of this class must stay prominent (never rail-collapsed).
    #[allow(dead_code)]
    pub fn is_prominent(self) -> bool {
        matches!(self, ToolClass::Mutation)
    }
}

/// Classify a tool by its name. Recognises both Claude's PascalCase names and
/// OpenCode-style snake_case aliases.
pub fn classify_tool(tool_name: &str) -> ToolClass {
    match tool_name {
        // Reads, searches, listings, and shell probes: routine.
        "Read" | "read_file" | "Grep" | "grep" | "Glob" | "glob" | "LS" | "ls" | "Bash"
        | "bash" | "NotebookRead" | "WebFetch" | "WebSearch" => ToolClass::Routine,
        // File mutations: prominent.
        "Write" | "write_file" | "Edit" | "edit_file" | "MultiEdit" | "NotebookEdit" | "Apply"
        | "apply_patch" => ToolClass::Mutation,
        // Delegation, MCP, and anything unknown: notable but not forced.
        _ => ToolClass::Notable,
    }
}

/// Whether a tool block should start collapsed, given its class and whether its
/// result (if any) was an error. Errors always start expanded so failures are
/// never hidden in the rail.
pub fn default_collapsed(class: ToolClass, is_error: bool) -> bool {
    if is_error {
        return false;
    }
    class.is_routine()
}

/// Running summary of a contiguous run of routine tool calls, for the rail
/// header (e.g. "12 reads, 3 searches · parser.rs, ledger.rs, …").
///
/// Consumed by the reader rail UI (DEV-31); allow dead_code until then.
#[allow(dead_code)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RoutineRailSummary {
    pub reads: u32,
    pub searches: u32,
    pub shell: u32,
    pub other_routine: u32,
    /// Notable targets (file names, patterns) in call order, de-duplicated.
    targets: Vec<String>,
}

#[allow(dead_code)]
impl RoutineRailSummary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one routine call into the summary.
    pub fn record(&mut self, tool_name: &str, target: Option<&str>) {
        match tool_name {
            "Read" | "read_file" | "NotebookRead" => self.reads += 1,
            "Grep" | "grep" | "Glob" | "glob" | "WebSearch" | "WebFetch" => self.searches += 1,
            "Bash" | "bash" => self.shell += 1,
            _ => self.other_routine += 1,
        }
        if let Some(t) = target {
            let t = t.trim();
            if !t.is_empty() && !self.targets.iter().any(|x| x == t) {
                self.targets.push(t.to_string());
            }
        }
    }

    pub fn total(&self) -> u32 {
        self.reads + self.searches + self.shell + self.other_routine
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// One-line rail header. `max_targets` caps how many targets are named
    /// before eliding with "…".
    pub fn headline(&self, max_targets: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.reads > 0 {
            parts.push(format!("{} read{}", self.reads, plural(self.reads)));
        }
        if self.searches > 0 {
            parts.push(format!(
                "{} search{}",
                self.searches,
                if self.searches == 1 { "" } else { "es" }
            ));
        }
        if self.shell > 0 {
            parts.push(format!("{} shell", self.shell));
        }
        if self.other_routine > 0 {
            parts.push(format!("{} other", self.other_routine));
        }
        let mut head = parts.join(", ");
        if !self.targets.is_empty() && max_targets > 0 {
            let shown = self.targets.len().min(max_targets);
            let mut names = self.targets[..shown].join(", ");
            if self.targets.len() > shown {
                names.push_str(", …");
            }
            head.push_str(" · ");
            head.push_str(&names);
        }
        head
    }
}

#[allow(dead_code)]
fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_searches_are_routine() {
        for name in [
            "Read",
            "read_file",
            "Grep",
            "Glob",
            "LS",
            "Bash",
            "WebFetch",
        ] {
            assert!(classify_tool(name).is_routine(), "{name} should be routine");
        }
    }

    #[test]
    fn mutations_are_prominent() {
        for name in ["Write", "Edit", "edit_file", "MultiEdit", "apply_patch"] {
            let c = classify_tool(name);
            assert!(c.is_prominent(), "{name} should be prominent");
            assert!(!c.is_routine());
        }
    }

    #[test]
    fn unknown_tools_are_notable_not_routine() {
        let c = classify_tool("mcp__linear__create_issue");
        assert_eq!(c, ToolClass::Notable);
        assert!(!c.is_routine() && !c.is_prominent());
    }

    #[test]
    fn errors_always_start_expanded() {
        // A routine read normally collapses…
        assert!(default_collapsed(ToolClass::Routine, false));
        // …but not when it errored.
        assert!(!default_collapsed(ToolClass::Routine, true));
        // Mutations never rail-collapse.
        assert!(!default_collapsed(ToolClass::Mutation, false));
    }

    #[test]
    fn summary_counts_by_category() {
        let mut s = RoutineRailSummary::new();
        s.record("Read", Some("parser.rs"));
        s.record("Read", Some("ledger.rs"));
        s.record("Grep", Some("TODO"));
        s.record("Bash", None);
        assert_eq!(s.reads, 2);
        assert_eq!(s.searches, 1);
        assert_eq!(s.shell, 1);
        assert_eq!(s.total(), 4);
    }

    #[test]
    fn summary_dedupes_targets_and_elides() {
        let mut s = RoutineRailSummary::new();
        s.record("Read", Some("a.rs"));
        s.record("Read", Some("a.rs")); // dup
        s.record("Read", Some("b.rs"));
        s.record("Read", Some("c.rs"));
        let head = s.headline(2);
        assert!(head.starts_with("4 reads"));
        assert!(head.contains("a.rs, b.rs, …"), "got: {head}");
    }

    #[test]
    fn headline_singularises() {
        let mut s = RoutineRailSummary::new();
        s.record("Read", None);
        assert_eq!(s.headline(0), "1 read");
    }
}
