//! Typed adapter for the agent status contract (DEV-514).
//!
//! Allele used to recover the active Locus Algorithm phase by matching English
//! words (`OBSERVE`…`LEARN`) in model-generated assistant prose — see
//! [`crate::rich::narrative`]. That made another repository's *prompt wording*
//! an undocumented API: renaming a phase silently broke this view.
//!
//! This module consumes the versioned replacement instead. The contract is
//! specified in `docs/locus-status-contract.md` and is **owned by Locus**;
//! Allele implements the consumer side only.
//!
//! Three properties matter, and the types below exist to enforce them:
//!
//!   * **Framed** — a status frame is a single literal `agent.status` key *in*
//!     the JSON object a hook already writes to stdout. Prose can no longer be
//!     mistaken for a signal.
//!   * **Versioned** — [`SUPPORTED_MAJOR`] gates acceptance, and a mismatch
//!     produces [`FrameOutcome::Unsupported`] rather than `None`, so the UI can
//!     degrade *visibly*. Absence already means "no producer running";
//!     collapsing the two would recreate the silent break.
//!   * **Generic** — the stage is a free-form label, never an enum. Locus may
//!     rename, add, or drop phases without touching Allele.
//!
//! Nothing here reads the filesystem. Everything Allele renders arrives in the
//! frame; `~/.locus/data` is deliberately not a seam.

use serde::Deserialize;

/// The contract name Allele consumes. A frame naming anything else is not this
/// contract and is ignored as though it were ordinary stdout.
pub const CONTRACT_NAME: &str = "agent.status";

/// The contract major version Allele implements. Frames declaring a different
/// major are rejected *visibly* — see [`FrameOutcome::Unsupported`].
pub const SUPPORTED_MAJOR: u32 = 1;

/// The literal top-level JSON key carrying a frame. One key whose name contains
/// a dot — not a nested `{"agent": {"status": …}}`.
const ENVELOPE_KEY: &str = "agent.status";

/// Payloads larger than this are skipped without parsing, so a runaway producer
/// cannot make the reader do unbounded work.
const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Display budget for a stage label. The contract asks producers to stay under
/// this; consumers truncate rather than let a long label break layout.
const MAX_LABEL_CHARS: usize = 24;

/// Display budget for the free-form detail line.
const MAX_DETAIL_CHARS: usize = 120;

// ── Wire types ────────────────────────────────────────────────────
//
// Every field but `contract` is optional at every level, and unknown fields are
// ignored rather than rejected — that is how the contract evolves additively
// without a major bump. Do not add `deny_unknown_fields` here.

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "agent.status")]
    status: Option<StatusFrame>,
}

/// A well-formed status frame, exactly as it arrived on the wire.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct StatusFrame {
    /// `"<name>/<major>"`. The only required field.
    pub contract: String,
    pub emitted: Option<String>,
    #[serde(default)]
    pub producer: Producer,
    #[serde(default)]
    pub session: Session,
    #[serde(default)]
    pub run: Run,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Producer {
    pub name: Option<String>,
    /// The producer's own version — *not* the contract version.
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Session {
    /// Display only. Never used to route a frame to another session.
    pub id: Option<String>,
    pub parent: Option<String>,
    pub backend: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Run {
    /// `idle` | `running` | `blocked` | `done` | `failed`. Unknown values are
    /// rendered verbatim rather than rejected, so a producer can add a state
    /// without a major bump.
    pub status: Option<String>,
    pub stage: Option<Stage>,
    #[serde(default)]
    pub progress: Progress,
    pub detail: Option<String>,
}

/// A producer stage. **`label` is deliberately a string, not an enum.**
/// Enumerating Locus's phases here would reproduce the very coupling this
/// contract removes.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Stage {
    /// Machine-stable slug, e.g. `"observe"`. Never switched on.
    pub id: Option<String>,
    /// The human label, displayed verbatim (after sanitising).
    pub label: Option<String>,
    /// 1-based position, when the producer orders its stages.
    pub ordinal: Option<u32>,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Progress {
    pub done: Option<u64>,
    pub total: Option<u64>,
    /// What is being counted, e.g. `"ISC"`.
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub tokens: Tokens,
    #[serde(default)]
    pub context: Context,
    /// Cumulative session cost. Not a delta — never sum successive frames.
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Tokens {
    /// Cumulative session totals, not deltas.
    pub input: Option<u64>,
    pub output: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Context {
    pub used: Option<u64>,
    pub limit: Option<u64>,
}

impl Stage {
    /// The label to show, sanitised and truncated. `None` when the stage
    /// carries no displayable label at all.
    pub fn display_label(&self) -> Option<String> {
        let raw = self.label.as_deref().or(self.id.as_deref())?;
        let clean = sanitise(raw, MAX_LABEL_CHARS);
        (!clean.is_empty()).then_some(clean)
    }
}

impl Run {
    /// The detail line to show, sanitised and truncated.
    pub fn display_detail(&self) -> Option<String> {
        let clean = sanitise(self.detail.as_deref()?, MAX_DETAIL_CHARS);
        (!clean.is_empty()).then_some(clean)
    }
}

/// Render a producer-controlled string safely: drop control characters and
/// ANSI escapes, collapse to one line, truncate to a display budget.
///
/// Every string in a frame is producer-controlled and, in practice, often
/// model-generated. A newline in `stage.label` must not become a layout break.
fn sanitise(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars * 4));
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        // Drop CSI/OSC escape sequences wholesale rather than just the ESC.
        if c == '\u{1b}' {
            for follow in chars.by_ref() {
                if follow.is_ascii_alphabetic() || follow == '\u{7}' {
                    break;
                }
            }
            continue;
        }
        // C0 and C1 control characters, including newline and tab.
        if c.is_control() {
            continue;
        }
        if out.chars().count() >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// Why a frame was refused. Carried into the UI so the refusal is *visible*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// The frame declares a contract major Allele does not implement. This is
    /// the case the contract exists to make loud.
    UnsupportedMajor { declared: String, major: u32 },
    /// The `contract` string was not `"<name>/<major>"`.
    UnparseableVersion { declared: String },
    /// The envelope key was present but its contents did not deserialise.
    Malformed { detail: String },
}

impl RejectReason {
    /// A short line for a degraded badge. Never empty — the whole point is that
    /// the user sees *something* rather than a blank indicator.
    pub fn badge_text(&self) -> String {
        match self {
            RejectReason::UnsupportedMajor { major, .. } => {
                format!("status v{major} unsupported")
            }
            RejectReason::UnparseableVersion { .. } => "status version unreadable".to_string(),
            RejectReason::Malformed { .. } => "status frame malformed".to_string(),
        }
    }
}

/// The result of inspecting one line of producer output.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameOutcome {
    /// A frame Allele implements. Render it.
    Accepted(Box<StatusFrame>),
    /// A frame Allele cannot implement. Render a degraded badge — and do *not*
    /// clear whatever was already accepted. Rejection is additive.
    Unsupported(RejectReason),
}

/// Parse the `<name>/<major>` contract string.
fn parse_contract(declared: &str) -> Option<(&str, u32)> {
    let (name, major) = declared.rsplit_once('/')?;
    let major = major.trim().parse::<u32>().ok()?;
    Some((name.trim(), major))
}

/// Inspect one hook-stdout payload.
///
/// Hook stdout is a **single JSON object** and the hook protocol already owns
/// it — a Locus `PreToolUse` handler returns the `decision` object implementing
/// the delegation guardrail there. The frame is therefore an additional
/// top-level key in that same object, never a second object appended after it,
/// which would produce `{…}{…}` and invalidate the whole payload.
///
/// Returns `None` for anything that is not a frame at all — an ordinary hook
/// response, a frame naming a different contract, an oversized payload. Those
/// are not errors and must not surface in the UI.
pub fn parse_stdout(stdout: &str) -> Option<FrameOutcome> {
    let trimmed = stdout.trim();
    // Cheap rejections first: a hook response is a JSON object, and ours
    // mentions the key.
    if trimmed.len() > MAX_FRAME_BYTES
        || !trimmed.starts_with('{')
        || !trimmed.contains(ENVELOPE_KEY)
    {
        return None;
    }

    // The key may appear in a payload that isn't a well-formed envelope — a
    // handler could legitimately mention the string. That is silence, not a
    // visible rejection.
    let envelope: Envelope = serde_json::from_str(trimmed).ok()?;
    let frame = envelope.status?;

    let Some((name, major)) = parse_contract(&frame.contract) else {
        return Some(FrameOutcome::Unsupported(
            RejectReason::UnparseableVersion {
                declared: frame.contract.clone(),
            },
        ));
    };
    // A different contract entirely is not ours to complain about.
    if name != CONTRACT_NAME {
        return None;
    }
    if major != SUPPORTED_MAJOR {
        return Some(FrameOutcome::Unsupported(RejectReason::UnsupportedMajor {
            declared: frame.contract.clone(),
            major,
        }));
    }
    Some(FrameOutcome::Accepted(Box::new(frame)))
}

/// Holds the status a session is currently displaying.
///
/// Two behaviours live here rather than in the view, because both are contract
/// requirements rather than presentation choices:
///
///   * **Rejection is additive.** An unsupported frame raises a visible badge
///     but must not clear the last frame Allele accepted. Blanking the
///     indicator would be indistinguishable from "no producer running" — the
///     silent break this contract exists to prevent, in a new costume.
///   * **Identical consecutive frames are a no-op.** A hook registered twice
///     delivers each frame twice; re-rendering must not flicker.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusTracker {
    frame: Option<StatusFrame>,
    degraded: Option<RejectReason>,
}

impl StatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Absorb one outcome. Returns `true` when the displayed state changed and
    /// the view needs repainting.
    pub fn ingest(&mut self, outcome: FrameOutcome) -> bool {
        match outcome {
            FrameOutcome::Accepted(frame) => {
                // A good frame clears any standing degradation.
                let cleared = self.degraded.take().is_some();
                if self.frame.as_ref() == Some(frame.as_ref()) {
                    return cleared;
                }
                self.frame = Some(*frame);
                true
            }
            FrameOutcome::Unsupported(reason) => {
                if self.degraded.as_ref() == Some(&reason) {
                    return false;
                }
                self.degraded = Some(reason);
                true
            }
        }
    }

    /// The last frame Allele accepted, if any.
    pub fn frame(&self) -> Option<&StatusFrame> {
        self.frame.as_ref()
    }

    /// The stage to display, and `None` when no frame has ever been accepted —
    /// which is the signal for the caller to fall back to prose parsing.
    pub fn stage(&self) -> Option<&Stage> {
        self.frame.as_ref()?.run.stage.as_ref()
    }

    /// Text for a degraded badge, when the producer is speaking a version
    /// Allele does not implement. Rendering this is not optional: it is how a
    /// version mismatch stays visible.
    pub fn degraded_badge(&self) -> Option<String> {
        Some(self.degraded.as_ref()?.badge_text())
    }

    /// Whether any frame has been accepted in this session. Once true, prose
    /// parsing is no longer the primary path.
    pub fn has_contract(&self) -> bool {
        self.frame.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{"agent.status":{"contract":"agent.status/1","emitted":"2026-08-30T12:04:11Z","producer":{"name":"locus","version":"1.1.0"},"session":{"id":"332f9e66","parent":null,"backend":"claude-code","model":"claude-opus-5"},"run":{"status":"running","stage":{"id":"observe","label":"OBSERVE","ordinal":1,"total":7},"progress":{"done":4,"total":18,"label":"ISC"},"detail":"extended"},"usage":{"tokens":{"input":120345,"output":8123},"context":{"used":128468,"limit":1000000},"cost_usd":0.42}}}"#;

    fn accepted(line: &str) -> StatusFrame {
        match parse_stdout(line) {
            Some(FrameOutcome::Accepted(f)) => *f,
            other => panic!("expected an accepted frame, got {other:?}"),
        }
    }

    #[test]
    fn deserialises_a_full_v1_frame() {
        let f = accepted(FULL);
        assert_eq!(f.contract, "agent.status/1");
        assert_eq!(f.producer.name.as_deref(), Some("locus"));
        assert_eq!(f.session.backend.as_deref(), Some("claude-code"));
        assert_eq!(f.session.parent, None);
        assert_eq!(f.run.status.as_deref(), Some("running"));
        let stage = f.run.stage.clone().expect("stage");
        assert_eq!(stage.display_label().as_deref(), Some("OBSERVE"));
        assert_eq!(stage.ordinal, Some(1));
        assert_eq!(f.run.progress.done, Some(4));
        assert_eq!(f.usage.tokens.input, Some(120345));
        assert_eq!(f.usage.context.limit, Some(1_000_000));
        assert_eq!(f.usage.cost_usd, Some(0.42));
    }

    #[test]
    fn contract_is_the_only_required_field() {
        // "Producer alive, nothing further to report" is a valid frame.
        let f = accepted(r#"{"agent.status":{"contract":"agent.status/1"}}"#);
        assert!(f.run.stage.is_none());
        assert!(f.usage.cost_usd.is_none());
    }

    #[test]
    fn unknown_fields_within_major_1_are_ignored() {
        // Additive evolution must not need a major bump.
        let f = accepted(
            r#"{"agent.status":{"contract":"agent.status/1","run":{"status":"running","futureField":42,"stage":{"label":"OBSERVE","newThing":"x"}},"somethingElse":{"a":1}}}"#,
        );
        assert_eq!(f.run.status.as_deref(), Some("running"));
        assert_eq!(
            f.run.stage.and_then(|s| s.display_label()).as_deref(),
            Some("OBSERVE")
        );
    }

    #[test]
    fn unknown_run_status_renders_rather_than_rejecting() {
        let f = accepted(
            r#"{"agent.status":{"contract":"agent.status/1","run":{"status":"hibernating"}}}"#,
        );
        assert_eq!(f.run.status.as_deref(), Some("hibernating"));
    }

    #[test]
    fn unsupported_major_is_visible_not_silent() {
        let out = parse_stdout(r#"{"agent.status":{"contract":"agent.status/2","run":{}}}"#);
        let Some(FrameOutcome::Unsupported(reason)) = out else {
            panic!("a v2 frame must be rejected visibly, got {out:?}");
        };
        assert_eq!(
            reason,
            RejectReason::UnsupportedMajor {
                declared: "agent.status/2".into(),
                major: 2
            }
        );
        // The badge must carry text; a blank badge is the silent failure again.
        assert!(reason.badge_text().contains("unsupported"));
        assert!(!reason.badge_text().is_empty());
    }

    #[test]
    fn unparseable_version_is_also_visible() {
        let out = parse_stdout(r#"{"agent.status":{"contract":"lolwhat"}}"#);
        assert!(matches!(
            out,
            Some(FrameOutcome::Unsupported(
                RejectReason::UnparseableVersion { .. }
            ))
        ));
    }

    #[test]
    fn a_different_contract_name_is_not_ours() {
        // Matching on major alone would wrongly claim this frame.
        assert_eq!(
            parse_stdout(r#"{"agent.status":{"contract":"other.thing/1"}}"#),
            None
        );
    }

    #[test]
    fn non_frame_stdout_is_silently_ignored() {
        for line in [
            "",
            "Locus: checkpoint written",
            "{not json at all",
            r#"{"hookSpecificOutput":{"hookEventName":"Stop"}}"#,
            r#"{"unrelated":{"contract":"agent.status/1"}}"#,
            // A nested shape is NOT the framing rule — the key is literal.
            r#"{"agent":{"status":{"contract":"agent.status/1"}}}"#,
        ] {
            assert_eq!(
                parse_stdout(line),
                None,
                "payload should be ignored: {line}"
            );
        }
    }

    #[test]
    fn oversized_payloads_are_skipped_without_parsing() {
        let huge = format!(
            r#"{{"agent.status":{{"contract":"agent.status/1","run":{{"detail":"{}"}}}}}}"#,
            "x".repeat(MAX_FRAME_BYTES)
        );
        assert_eq!(parse_stdout(&huge), None);
    }

    #[test]
    fn frame_rides_alongside_the_hooks_own_response() {
        // The load-bearing transport case: the guardrail's `decision` object
        // and the frame share ONE JSON object. Appending a second object would
        // make the payload invalid and silently disable the guardrail.
        let stdout = r#"{"decision":{"permissionDecision":"deny","permissionDecisionReason":"use locus delegate run"},"agent.status":{"contract":"agent.status/1","run":{"stage":{"label":"BUILD"}}}}"#;
        let f = accepted(stdout);
        assert_eq!(
            f.run.stage.and_then(|s| s.display_label()).as_deref(),
            Some("BUILD")
        );
    }

    #[test]
    fn hook_response_without_a_frame_is_ignored() {
        let stdout = r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"Locus is active"}}"#;
        assert_eq!(parse_stdout(stdout), None);
    }

    #[test]
    fn concatenated_objects_are_rejected_not_half_read() {
        // `{…}{…}` is what the *wrong* transport would produce. It must not
        // parse, so the mistake surfaces in testing rather than in the field.
        let bad = format!("{FULL}{FULL}");
        assert_eq!(parse_stdout(&bad), None);
    }

    #[test]
    fn tracker_ignores_identical_consecutive_frames() {
        // Locus hooks are currently registered twice (DEV-504), so every frame
        // arrives in duplicate. Re-rendering must be a no-op.
        let mut t = StatusTracker::new();
        assert!(t.ingest(parse_stdout(FULL).unwrap()));
        assert!(!t.ingest(parse_stdout(FULL).unwrap()));
        assert_eq!(
            t.stage().and_then(|s| s.display_label()).as_deref(),
            Some("OBSERVE")
        );
    }

    #[test]
    fn rejection_never_clears_an_accepted_frame() {
        let mut t = StatusTracker::new();
        t.ingest(parse_stdout(FULL).unwrap());
        let v2 = r#"{"agent.status":{"contract":"agent.status/2"}}"#;
        assert!(t.ingest(parse_stdout(v2).unwrap()));
        // Badge visible AND the last good stage still shown.
        assert!(t.degraded_badge().is_some());
        assert_eq!(
            t.stage().and_then(|s| s.display_label()).as_deref(),
            Some("OBSERVE")
        );
        assert!(t.has_contract());
    }

    #[test]
    fn a_good_frame_clears_a_standing_degradation() {
        let mut t = StatusTracker::new();
        t.ingest(parse_stdout(r#"{"agent.status":{"contract":"agent.status/9"}}"#).unwrap());
        assert!(t.degraded_badge().is_some());
        assert!(!t.has_contract());
        t.ingest(parse_stdout(FULL).unwrap());
        assert_eq!(t.degraded_badge(), None);
    }

    #[test]
    fn no_frame_means_fall_back_to_prose() {
        let t = StatusTracker::new();
        assert!(!t.has_contract());
        assert_eq!(t.stage(), None);
        assert_eq!(t.degraded_badge(), None);
    }

    #[test]
    fn stage_labels_are_free_form_not_an_enum() {
        // The whole point: Locus can rename or invent phases freely.
        for label in ["OBSERVE", "TRIAGE", "Phase Nine", "réflexion"] {
            let line = format!(
                r#"{{"agent.status":{{"contract":"agent.status/1","run":{{"stage":{{"label":"{label}"}}}}}}}}"#
            );
            let f = accepted(&line);
            assert_eq!(
                f.run.stage.and_then(|s| s.display_label()).as_deref(),
                Some(label)
            );
        }
    }

    #[test]
    fn control_characters_and_escapes_are_stripped_from_labels() {
        let stage = Stage {
            label: Some("OB\u{1b}[31mSER\nVE\t!".into()),
            ..Default::default()
        };
        assert_eq!(stage.display_label().as_deref(), Some("OBSERVE!"));
    }

    #[test]
    fn long_labels_are_truncated_not_clipped_by_layout() {
        let stage = Stage {
            label: Some("A".repeat(200)),
            ..Default::default()
        };
        let shown = stage.display_label().expect("label");
        assert!(shown.chars().count() <= MAX_LABEL_CHARS + 1, "{shown}");
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn stage_falls_back_to_id_when_label_is_absent() {
        let stage = Stage {
            id: Some("observe".into()),
            ..Default::default()
        };
        assert_eq!(stage.display_label().as_deref(), Some("observe"));
    }

    #[test]
    fn supported_version_is_pinned() {
        // Drift here should fail a test, not blank a pill. Bumping this means
        // the contract doc's major changed too.
        assert_eq!(SUPPORTED_MAJOR, 1);
        assert_eq!(CONTRACT_NAME, "agent.status");
    }
}
