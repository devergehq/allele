//! Narrative projection (DEV-29).
//!
//! The document model records *what happened* (text, tools, diffs). The
//! narrative projection is the interpretive layer on top: it reads the same
//! event stream and annotates each event with
//!
//!   * the **conversational turn** it belongs to (a turn opens on each user
//!     prompt),
//!   * the **producer stage** in effect, taken from the versioned status
//!     contract when a producer emits one (DEV-514), and otherwise recovered
//!     from phase headers in assistant text and carried forward,
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

use crate::rich::locus_status::{FrameOutcome, Stage, StatusTracker};
use crate::stream::RichEvent;

/// Where the stage shown for an event came from.
///
/// This distinction is the point of DEV-514. A stage read from the versioned
/// status contract is authoritative; one inferred from English words in
/// model-generated prose is a guess, and the UI must be able to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageSource {
    /// From an `agent.status` frame — typed, versioned, trustworthy.
    Contract,
    /// Inferred from a phase header in assistant prose. Legacy fallback for
    /// producers that have not adopted the contract.
    Prose,
}

/// The seven Locus algorithm phases, in order.
///
/// **This enum is the *prose fallback's* vocabulary, and nothing more.** It is
/// the only place in Allele that may name a Locus phase, because prose parsing
/// is inherently a guess against a fixed word list. Everything downstream
/// handles [`Stage`], whose label is free-form — so Locus can rename, add, or
/// drop phases without breaking the contract path.
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

    /// Project onto the generic stage the rest of Allele renders.
    fn to_stage(self) -> Stage {
        Stage {
            id: Some(self.label().to_ascii_lowercase()),
            label: Some(self.label().to_string()),
            ordinal: Some(self.ordinal()),
            total: Some(7),
        }
    }

    fn ordinal(self) -> u32 {
        match self {
            LocusPhase::Observe => 1,
            LocusPhase::Think => 2,
            LocusPhase::Plan => 3,
            LocusPhase::Build => 4,
            LocusPhase::Execute => 5,
            LocusPhase::Verify => 6,
            LocusPhase::Learn => 7,
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
    /// A stage announcement: either a contract frame, or a phase header
    /// (`Phase 1: OBSERVE (1/7)`) recovered from prose.
    PhaseHeader(Stage),
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
    /// Producer stage in effect for this event, if any is active.
    pub phase: Option<Stage>,
    /// Where `phase` came from. `None` when there is no stage.
    pub stage_source: Option<StageSource>,
    pub role: NarrativeRole,
    /// Delegated subagent id, if this event came from a subagent.
    pub agent: Option<String>,
}

/// Streaming narrative projector. Feed events in order.
#[derive(Default)]
pub struct NarrativeProjector {
    turn: usize,
    /// Authoritative stage, from the status contract.
    tracker: StatusTracker,
    /// Best-effort stage, inferred from prose. Only consulted while the
    /// contract has produced nothing.
    prose_phase: Option<LocusPhase>,
}

impl NarrativeProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stage currently in effect, and where it came from, for callers that
    /// render a sticky stage indicator.
    ///
    /// The contract wins whenever a producer has sent a frame; prose is only
    /// consulted for producers that have not adopted it.
    pub fn current_stage(&self) -> Option<(Stage, StageSource)> {
        if let Some(stage) = self.tracker.stage() {
            return Some((stage.clone(), StageSource::Contract));
        }
        self.prose_phase.map(|p| (p.to_stage(), StageSource::Prose))
    }

    /// Text for a visible degraded badge when a producer is speaking a contract
    /// version Allele does not implement.
    ///
    /// Rendering this is **not optional**. Absence of a stage already means "no
    /// producer running"; if a version mismatch also rendered nothing, the
    /// silent cross-repository break this contract exists to prevent would
    /// simply return in a new form.
    pub fn degraded_badge(&self) -> Option<String> {
        self.tracker.degraded_badge()
    }

    /// Whether a status frame has been accepted in this session. Once true,
    /// prose parsing is no longer consulted for the stage.
    pub fn has_contract(&self) -> bool {
        self.tracker.has_contract()
    }

    fn stage_fields(&self) -> (Option<Stage>, Option<StageSource>) {
        match self.current_stage() {
            Some((stage, source)) => (Some(stage), Some(source)),
            None => (None, None),
        }
    }

    /// Annotate a user prompt, opening a new turn.
    pub fn on_user_prompt(&mut self) -> Annotation {
        self.turn += 1;
        let (phase, stage_source) = self.stage_fields();
        Annotation {
            turn: self.turn,
            phase,
            stage_source,
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
            // A notice punctuates the narrative rather than advancing it —
            // Activity keeps it out of the prose/decision classification.
            RichEvent::Notice { .. } => NarrativeRole::Activity,
            RichEvent::Fallback { .. } => NarrativeRole::Unsupported,
            RichEvent::Init { .. } | RichEvent::HookStatus { .. } => NarrativeRole::Prose,
            // The typed stage channel. Ingesting may promote the sticky
            // indicator from prose to contract, or raise a degraded badge.
            RichEvent::AgentStatus { outcome } => {
                self.tracker.ingest(outcome.clone());
                match outcome {
                    FrameOutcome::Accepted(frame) => match frame.run.stage.clone() {
                        Some(stage) => NarrativeRole::PhaseHeader(stage),
                        None => NarrativeRole::Activity,
                    },
                    FrameOutcome::Unsupported(_) => NarrativeRole::Activity,
                }
            }
        };
        let (phase, stage_source) = self.stage_fields();
        Annotation {
            turn: self.turn,
            phase,
            stage_source,
            role,
            agent,
        }
    }

    /// Classify a text block, updating `self.phase` if it announces one.
    fn classify_text(&mut self, text: &str) -> NarrativeRole {
        let trimmed = text.trim_start();

        if let Some(phase) = detect_phase_header(trimmed) {
            self.prose_phase = Some(phase);
            // A header still marks the passage even once the contract is
            // authoritative — it just no longer *drives* the indicator.
            return NarrativeRole::PhaseHeader(phase.to_stage());
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
        | RichEvent::Notice {
            parent_agent_id, ..
        }
        | RichEvent::Fallback {
            parent_agent_id, ..
        } => parent_agent_id.clone(),
        RichEvent::Init { .. }
        | RichEvent::SessionResult { .. }
        | RichEvent::HookStatus { .. }
        | RichEvent::AgentStatus { .. } => None,
    }
}

/// Recognise a Locus phase header at the start of a text block. Handles both
/// the formal `Phase 1: OBSERVE (1/7)` form and a bare heading like
/// `## OBSERVE` or `**OBSERVE**` on its own line.
pub fn detect_phase_header(text: &str) -> Option<LocusPhase> {
    let first_line = text.lines().next().unwrap_or("").trim();
    // Strip common Markdown heading / emphasis / list markers.
    let cleaned: String = first_line
        .trim_start_matches(['#', '*', '-', ' ', '>'])
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

    /// The stage label an annotation is showing, whatever its source.
    fn shown(a: &Annotation) -> Option<String> {
        a.phase.as_ref()?.display_label()
    }

    /// Build an accepted status frame carrying `label`.
    fn frame_event(label: &str) -> RichEvent {
        let json = format!(
            r#"{{"agent.status":{{"contract":"agent.status/1","run":{{"stage":{{"id":"{}","label":"{label}"}}}}}}}}"#,
            label.to_ascii_lowercase()
        );
        let outcome = crate::rich::locus_status::parse_stdout(&json).expect("a valid frame");
        RichEvent::AgentStatus { outcome }
    }

    fn unsupported_event(version: &str) -> RichEvent {
        let json = format!(r#"{{"agent.status":{{"contract":"agent.status/{version}"}}}}"#);
        let outcome = crate::rich::locus_status::parse_stdout(&json).expect("an outcome");
        RichEvent::AgentStatus { outcome }
    }

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
        assert_eq!(
            a.role,
            NarrativeRole::PhaseHeader(LocusPhase::Observe.to_stage())
        );
        // Subsequent prose inherits the active phase.
        let b = p.on_event(&text("Looking at the parser now."));
        assert_eq!(shown(&b).as_deref(), Some("OBSERVE"));
        assert_eq!(b.role, NarrativeRole::Prose);
        // Without a contract frame, the stage is attributed to prose.
        assert_eq!(b.stage_source, Some(StageSource::Prose));
        assert!(!p.has_contract());
        // A new header switches the phase.
        let c = p.on_event(&text("Phase 4: BUILD (4/7)"));
        assert_eq!(shown(&c).as_deref(), Some("BUILD"));
    }

    #[test]
    fn contract_frame_overrides_prose() {
        let mut p = NarrativeProjector::new();
        // Prose gets us a guess...
        p.on_event(&text("Phase 1: OBSERVE (1/7)"));
        assert_eq!(p.current_stage().map(|(_, s)| s), Some(StageSource::Prose));

        // ...and a frame supersedes it, even one prose could never produce.
        let a = p.on_event(&frame_event("TRIAGE"));
        assert_eq!(shown(&a).as_deref(), Some("TRIAGE"));
        assert_eq!(a.stage_source, Some(StageSource::Contract));
        assert!(p.has_contract());

        // Later prose headers no longer drive the indicator.
        let b = p.on_event(&text("Phase 7: LEARN (7/7)"));
        assert_eq!(shown(&b).as_deref(), Some("TRIAGE"));
        assert_eq!(b.stage_source, Some(StageSource::Contract));
    }

    #[test]
    fn stage_labels_locus_never_had_still_render() {
        // The regression this ticket exists to prevent: Locus renaming or
        // inventing a phase must not blank the indicator.
        let mut p = NarrativeProjector::new();
        let a = p.on_event(&frame_event("RECONNAISSANCE"));
        assert_eq!(shown(&a).as_deref(), Some("RECONNAISSANCE"));
        assert_eq!(a.stage_source, Some(StageSource::Contract));
    }

    #[test]
    fn version_mismatch_degrades_visibly_not_silently() {
        let mut p = NarrativeProjector::new();
        p.on_event(&frame_event("OBSERVE"));
        p.on_event(&unsupported_event("2"));

        // A badge with real text, and the last good stage still on screen.
        let badge = p.degraded_badge().expect("a visible badge");
        assert!(badge.contains("unsupported"), "{badge}");
        assert_eq!(
            p.current_stage()
                .and_then(|(s, _)| s.display_label())
                .as_deref(),
            Some("OBSERVE")
        );
    }

    #[test]
    fn prose_remains_the_fallback_for_unupgraded_producers() {
        // No frame ever arrives: the old path still works, attributed.
        let mut p = NarrativeProjector::new();
        p.on_event(&text("Phase 6: VERIFY (6/7)"));
        let (stage, source) = p.current_stage().expect("a prose stage");
        assert_eq!(stage.display_label().as_deref(), Some("VERIFY"));
        assert_eq!(source, StageSource::Prose);
        assert_eq!(p.degraded_badge(), None);
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
        assert!(NarrativeRole::PhaseHeader(LocusPhase::Plan.to_stage()).is_emphasised());
        assert!(NarrativeRole::Decision.is_emphasised());
        assert!(!NarrativeRole::Prose.is_emphasised());
        assert!(!NarrativeRole::Activity.is_emphasised());
    }
}
