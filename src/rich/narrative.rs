//! Narrative projection (DEV-29).
//!
//! The document model records *what happened* (text, tools, diffs). The
//! narrative projection is the interpretive layer on top: it reads the same
//! event stream and annotates each event with
//!
//!   * the **conversational turn** it belongs to (a turn opens on each user
//!     prompt),
//!   * the **Locus phase** in effect (OBSERVE…LEARN), recognised from phase
//!     headers in assistant text and carried forward until the next header,
//!   * a **narrative role** (classification banner, phase header, decision,
//!     outcome/summary, reasoning, or plain prose) so the renderer can
//!     prioritise prompts/decisions/outcomes and de-emphasise routine prose,
//!     and
//!   * the **delegated agent** id, if the event came from a subagent.
//!
//! The projector is stateful and streaming — feed it events in order and it
//! returns one [`Annotation`] per event, mirroring how `StreamParser` and the
//! ledger operate. It is deliberately pure (no rendering, no GPUI) so it can
//! be unit-tested against representative sessions.

use crate::stream::RichEvent;

/// The seven Locus algorithm phases, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocusPhase {
    Observe,
    Think,
    Plan,
    Build,
    Execute,
    Verify,
    Learn,
}

impl LocusPhase {
    fn from_keyword(word: &str) -> Option<LocusPhase> {
        match word.to_ascii_uppercase().as_str() {
            "OBSERVE" => Some(LocusPhase::Observe),
            "THINK" => Some(LocusPhase::Think),
            "PLAN" => Some(LocusPhase::Plan),
            "BUILD" => Some(LocusPhase::Build),
            "EXECUTE" => Some(LocusPhase::Execute),
            "VERIFY" => Some(LocusPhase::Verify),
            "LEARN" => Some(LocusPhase::Learn),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LocusPhase::Observe => "OBSERVE",
            LocusPhase::Think => "THINK",
            LocusPhase::Plan => "PLAN",
            LocusPhase::Build => "BUILD",
            LocusPhase::Execute => "EXECUTE",
            LocusPhase::Verify => "VERIFY",
            LocusPhase::Learn => "LEARN",
        }
    }
}

/// What a segment of the narrative *is*, for prioritisation. Ordered loosely
/// from most to least salient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarrativeRole {
    /// A `**Classification: …**` banner opening a Locus response.
    Classification,
    /// A Locus phase header (`Phase 1: OBSERVE (1/7)` or a bare `OBSERVE`).
    PhaseHeader(LocusPhase),
    /// A user prompt that opened this turn.
    Prompt,
    /// An explicit decision the agent recorded ("Decision:", "I'll go with…").
    Decision,
    /// A completion summary / final outcome (session end, or a "Done"/"Summary").
    Outcome,
    /// Reasoning / thinking content.
    Reasoning,
    /// A tool invocation or its result.
    Activity,
    /// An event we couldn't normalise.
    Unsupported,
    /// Ordinary narrative prose.
    Prose,
}

impl NarrativeRole {
    /// Whether this role should be visually emphasised in the narrative
    /// (prompts, phase headers, classifications, decisions, outcomes).
    pub fn is_emphasised(&self) -> bool {
        matches!(
            self,
            NarrativeRole::Classification
                | NarrativeRole::PhaseHeader(_)
                | NarrativeRole::Prompt
                | NarrativeRole::Decision
                | NarrativeRole::Outcome
        )
    }
}

/// The projection result for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// 1-based conversational turn. 0 means "before any user prompt".
    pub turn: usize,
    /// Locus phase in effect for this event, if any is active.
    pub phase: Option<LocusPhase>,
    pub role: NarrativeRole,
    /// Delegated subagent id, if this event came from a subagent.
    pub agent: Option<String>,
}

/// Streaming narrative projector. Feed events in order.
#[derive(Default)]
pub struct NarrativeProjector {
    turn: usize,
    phase: Option<LocusPhase>,
}

impl NarrativeProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The Locus phase currently in effect (for callers that render a sticky
    /// phase indicator).
    pub fn current_phase(&self) -> Option<LocusPhase> {
        self.phase
    }

    /// Annotate a user prompt, opening a new turn.
    pub fn on_user_prompt(&mut self) -> Annotation {
        self.turn += 1;
        Annotation {
            turn: self.turn,
            phase: self.phase,
            role: NarrativeRole::Prompt,
            agent: None,
        }
    }

    /// Annotate a rich event. Updates the active phase when the event carries a
    /// phase header, and never lets an event fall through unclassified.
    pub fn on_event(&mut self, event: &RichEvent) -> Annotation {
        let agent = event_agent(event);
        let role = match event {
            RichEvent::TextBlock { text, .. } | RichEvent::TextDelta { text, .. } => {
                self.classify_text(text)
            }
            RichEvent::ThinkingBlock { .. } => NarrativeRole::Reasoning,
            RichEvent::ToolUse { .. }
            | RichEvent::ToolResult { .. }
            | RichEvent::EditDiff { .. } => NarrativeRole::Activity,
            RichEvent::SessionResult { .. } => NarrativeRole::Outcome,
            RichEvent::Fallback { .. } => NarrativeRole::Unsupported,
            RichEvent::Init { .. } | RichEvent::HookStatus { .. } => NarrativeRole::Prose,
        };
        Annotation {
            turn: self.turn,
            phase: self.phase,
            role,
            agent,
        }
    }

    /// Classify a text block, updating `self.phase` if it announces one.
    fn classify_text(&mut self, text: &str) -> NarrativeRole {
        let trimmed = text.trim_start();

        if let Some(phase) = detect_phase_header(trimmed) {
            self.phase = Some(phase);
            return NarrativeRole::PhaseHeader(phase);
        }
        if is_classification_banner(trimmed) {
            return NarrativeRole::Classification;
        }
        if is_decision(trimmed) {
            return NarrativeRole::Decision;
        }
        if is_outcome(trimmed) {
            return NarrativeRole::Outcome;
        }
        NarrativeRole::Prose
    }
}

fn event_agent(event: &RichEvent) -> Option<String> {
    match event {
        RichEvent::TextDelta {
            parent_agent_id, ..
        }
        | RichEvent::TextBlock {
            parent_agent_id, ..
        }
        | RichEvent::ThinkingBlock {
            parent_agent_id, ..
        }
        | RichEvent::ToolUse {
            parent_agent_id, ..
        }
        | RichEvent::ToolResult {
            parent_agent_id, ..
        }
        | RichEvent::EditDiff {
            parent_agent_id, ..
        }
        | RichEvent::Fallback {
            parent_agent_id, ..
        } => parent_agent_id.clone(),
        RichEvent::Init { .. } | RichEvent::SessionResult { .. } | RichEvent::HookStatus { .. } => {
            None
        }
    }
}

/// Recognise a Locus phase header at the start of a text block. Handles both
/// the formal `Phase 1: OBSERVE (1/7)` form and a bare heading like
/// `## OBSERVE` or `**OBSERVE**` on its own line.
pub fn detect_phase_header(text: &str) -> Option<LocusPhase> {
    let first_line = text.lines().next().unwrap_or("").trim();
    // Strip common Markdown heading / emphasis / list markers.
    let cleaned: String = first_line
        .trim_start_matches(|c: char| c == '#' || c == '*' || c == '-' || c == ' ' || c == '>')
        .to_string();
    let upper = cleaned.to_ascii_uppercase();

    // Formal form: "PHASE <n>: <NAME>" possibly followed by "(n/7)".
    if let Some(rest) = upper.strip_prefix("PHASE ") {
        if let Some(colon) = rest.find(':') {
            let name = rest[colon + 1..]
                .split(|c: char| c == '(' || c.is_whitespace())
                .find(|s| !s.is_empty())
                .unwrap_or("");
            if let Some(p) = LocusPhase::from_keyword(name) {
                return Some(p);
            }
        }
    }

    // Bare heading: the whole (short) line is exactly a phase keyword, allowing
    // a trailing "(1/7)" progress marker. Guard on length so a paragraph that
    // merely starts with the word "Plan…" isn't misread as a header.
    let head_word = upper
        .split(|c: char| c == '(' || c.is_whitespace())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    if head_word.len() == upper.trim().len().min(head_word.len())
        && upper.trim_start().starts_with(head_word)
        && upper.trim().len() <= head_word.len() + 6
    {
        if let Some(p) = LocusPhase::from_keyword(head_word) {
            return Some(p);
        }
    }
    None
}

fn is_classification_banner(text: &str) -> bool {
    let head = text.trim_start_matches(['*', '#', ' ']);
    head.to_ascii_lowercase().starts_with("classification:")
}

fn is_decision(text: &str) -> bool {
    let lower = text
        .trim_start_matches(['*', '#', '-', ' ', '>'])
        .to_ascii_lowercase();
    lower.starts_with("decision:")
        || lower.starts_with("decided:")
        || lower.starts_with("i'll ")
        || lower.starts_with("i will ")
        || lower.starts_with("going with ")
}

fn is_outcome(text: &str) -> bool {
    let lower = text
        .trim_start_matches(['*', '#', '-', ' ', '>'])
        .to_ascii_lowercase();
    lower.starts_with("summary:")
        || lower.starts_with("done.")
        || lower.starts_with("done —")
        || lower.starts_with("completed:")
        || lower.starts_with("outcome:")
        || lower.starts_with("result:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> RichEvent {
        RichEvent::TextBlock {
            text: s.to_string(),
            parent_agent_id: None,
        }
    }

    #[test]
    fn detects_formal_phase_header() {
        assert_eq!(
            detect_phase_header("Phase 1: OBSERVE (1/7)"),
            Some(LocusPhase::Observe)
        );
        assert_eq!(
            detect_phase_header("## Phase 6: VERIFY"),
            Some(LocusPhase::Verify)
        );
        assert_eq!(
            detect_phase_header("**Phase 3: PLAN (3/7)**"),
            Some(LocusPhase::Plan)
        );
    }

    #[test]
    fn detects_bare_phase_heading() {
        assert_eq!(detect_phase_header("## OBSERVE"), Some(LocusPhase::Observe));
        assert_eq!(
            detect_phase_header("EXECUTE (5/7)"),
            Some(LocusPhase::Execute)
        );
    }

    #[test]
    fn does_not_misread_prose_as_phase() {
        // A paragraph that merely begins with a phase word is not a header.
        assert_eq!(
            detect_phase_header("Plan the migration carefully before starting."),
            None
        );
        assert_eq!(
            detect_phase_header("Observe that the tests already pass here."),
            None
        );
    }

    #[test]
    fn phase_persists_until_next_header() {
        let mut p = NarrativeProjector::new();
        let a = p.on_event(&text("Phase 1: OBSERVE (1/7)"));
        assert_eq!(a.role, NarrativeRole::PhaseHeader(LocusPhase::Observe));
        // Subsequent prose inherits the active phase.
        let b = p.on_event(&text("Looking at the parser now."));
        assert_eq!(b.phase, Some(LocusPhase::Observe));
        assert_eq!(b.role, NarrativeRole::Prose);
        // A new header switches the phase.
        let c = p.on_event(&text("Phase 4: BUILD (4/7)"));
        assert_eq!(c.phase, Some(LocusPhase::Build));
    }

    #[test]
    fn turns_increment_on_user_prompts() {
        let mut p = NarrativeProjector::new();
        assert_eq!(p.on_event(&text("pre-prompt")).turn, 0);
        assert_eq!(p.on_user_prompt().turn, 1);
        assert_eq!(p.on_event(&text("reply")).turn, 1);
        assert_eq!(p.on_user_prompt().turn, 2);
    }

    #[test]
    fn classifies_salient_roles() {
        let mut p = NarrativeProjector::new();
        assert_eq!(
            p.on_event(&text("**Classification: Non-trivial**")).role,
            NarrativeRole::Classification
        );
        assert_eq!(
            p.on_event(&text("Decision: use a ledger.")).role,
            NarrativeRole::Decision
        );
        assert_eq!(
            p.on_event(&text("I'll stack the PRs bottom-up.")).role,
            NarrativeRole::Decision
        );
        assert_eq!(
            p.on_event(&text("Summary: shipped two tickets.")).role,
            NarrativeRole::Outcome
        );
    }

    #[test]
    fn session_result_is_outcome_and_fallback_is_unsupported() {
        let mut p = NarrativeProjector::new();
        let end = RichEvent::SessionResult {
            duration_ms: 1,
            cost_usd: 0.0,
            num_turns: 1,
            is_error: false,
            result_text: None,
        };
        assert_eq!(p.on_event(&end).role, NarrativeRole::Outcome);
        let fb = RichEvent::Fallback {
            raw: "{}".into(),
            reason: "x".into(),
            parent_agent_id: None,
        };
        assert_eq!(p.on_event(&fb).role, NarrativeRole::Unsupported);
    }

    #[test]
    fn distinguishes_delegated_agents() {
        let mut p = NarrativeProjector::new();
        let ev = RichEvent::ToolUse {
            tool_use_id: "t1".into(),
            tool_name: "Grep".into(),
            input: serde_json::Value::Null,
            parent_agent_id: Some("agent-7".into()),
        };
        let a = p.on_event(&ev);
        assert_eq!(a.agent.as_deref(), Some("agent-7"));
        assert_eq!(a.role, NarrativeRole::Activity);
    }

    #[test]
    fn emphasis_flags_the_right_roles() {
        assert!(NarrativeRole::Prompt.is_emphasised());
        assert!(NarrativeRole::PhaseHeader(LocusPhase::Plan).is_emphasised());
        assert!(NarrativeRole::Decision.is_emphasised());
        assert!(!NarrativeRole::Prose.is_emphasised());
        assert!(!NarrativeRole::Activity.is_emphasised());
    }
}
