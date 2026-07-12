//! Transcript reading & navigation (DEV-31).
//!
//! Rendering (typography, speaker/turn cards, scroll preservation) lives in the
//! view layer; this module is the navigational spine underneath it:
//!
//!   * [`NarrativeIndex`] — a lightweight, searchable index of the narrative
//!     built from the projector's [`Annotation`]s. It answers "find every
//!     segment mentioning X" and "list the jump points" (phases, decisions,
//!     outcomes, errors, artifacts) so the reader can offer search and
//!     jump-to-next-of-kind without re-scanning the whole document.
//!   * [`UnreadTracker`] — the "Since last viewed" model: it remembers where
//!     the reader last caught up to and computes what is unread.
//!
//! Pure and view-agnostic, so both are unit-tested directly.

use super::narrative::{Annotation, LocusPhase, NarrativeRole};

/// A navigable point in the narrative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpTarget {
    /// Index of the entry in the recorded stream.
    pub seq: usize,
    pub turn: usize,
    pub kind: JumpKind,
    /// Short label for the navigation list.
    pub label: String,
}

/// Tally of navigable points by kind (for the navigation strip).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NavCounts {
    pub phases: usize,
    pub decisions: usize,
    pub outcomes: usize,
    pub errors: usize,
    pub artifacts: usize,
}

impl NavCounts {
    /// Whether there is anything worth showing a navigation strip for.
    pub fn any(&self) -> bool {
        self.phases + self.decisions + self.outcomes + self.errors + self.artifacts > 0
    }
}

/// The categories a reader can jump between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpKind {
    Phase(LocusPhase),
    Decision,
    Outcome,
    Error,
    Artifact,
}

/// One indexed narrative entry.
#[derive(Debug, Clone)]
struct Entry {
    seq: usize,
    turn: usize,
    role: NarrativeRole,
    /// Searchable/display text (lowercased copy kept separately for search).
    text: String,
    text_lower: String,
    /// A file path this entry produced, if any (drives Artifact jumps).
    artifact: Option<String>,
}

/// Searchable index over the projected narrative.
#[derive(Default)]
pub struct NarrativeIndex {
    entries: Vec<Entry>,
}

impl NarrativeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one narrative segment. `text` is the human-visible content (may
    /// be empty for non-text events); `artifact` is a file path when the event
    /// created or edited one.
    pub fn record(&mut self, seq: usize, ann: &Annotation, text: &str, artifact: Option<&str>) {
        self.entries.push(Entry {
            seq,
            turn: ann.turn,
            role: ann.role.clone(),
            text: text.to_string(),
            text_lower: text.to_lowercase(),
            artifact: artifact.map(|s| s.to_string()),
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Case-insensitive substring search. Returns matching entry `seq`s in
    /// document order. An empty query matches nothing (not everything).
    pub fn search(&self, query: &str) -> Vec<usize> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|e| e.text_lower.contains(&q))
            .map(|e| e.seq)
            .collect()
    }

    /// All jump targets in document order.
    pub fn jump_targets(&self) -> Vec<JumpTarget> {
        self.entries
            .iter()
            .filter_map(|e| self.target_for(e))
            .collect()
    }

    /// Tally of navigable points by kind, for a compact navigation strip.
    pub fn counts(&self) -> NavCounts {
        let mut c = NavCounts::default();
        for t in self.jump_targets() {
            match t.kind {
                JumpKind::Phase(_) => c.phases += 1,
                JumpKind::Decision => c.decisions += 1,
                JumpKind::Outcome => c.outcomes += 1,
                JumpKind::Error => c.errors += 1,
                JumpKind::Artifact => c.artifacts += 1,
            }
        }
        c
    }

    /// Jump targets of a single kind — for "next phase", "next decision", etc.
    pub fn jump_targets_of(&self, kind: JumpKind) -> Vec<JumpTarget> {
        self.jump_targets()
            .into_iter()
            .filter(|t| t.kind == kind)
            .collect()
    }

    /// The next target matching `pred` strictly after `after`, **wrapping** to
    /// the first match when none follows (so repeated "next" clicks cycle).
    /// `after: None` starts from the first match. Powers the clickable
    /// navigation strip (jump to next phase / decision / error / …).
    pub fn jump_after(
        &self,
        after: Option<usize>,
        pred: impl Fn(&JumpKind) -> bool,
    ) -> Option<JumpTarget> {
        let all: Vec<JumpTarget> = self
            .jump_targets()
            .into_iter()
            .filter(|t| pred(&t.kind))
            .collect();
        match after {
            Some(s) => all
                .iter()
                .find(|t| t.seq > s)
                .or_else(|| all.first())
                .cloned(),
            None => all.first().cloned(),
        }
    }

    /// The first jump target at or after `from_seq` matching `pred` — the
    /// primitive behind "jump to next error / next decision".
    pub fn next_target(
        &self,
        from_seq: usize,
        pred: impl Fn(&JumpKind) -> bool,
    ) -> Option<JumpTarget> {
        self.jump_targets()
            .into_iter()
            .find(|t| t.seq > from_seq && pred(&t.kind))
    }

    fn target_for(&self, e: &Entry) -> Option<JumpTarget> {
        let kind = match &e.role {
            NarrativeRole::PhaseHeader(p) => JumpKind::Phase(*p),
            NarrativeRole::Decision => JumpKind::Decision,
            NarrativeRole::Outcome => JumpKind::Outcome,
            NarrativeRole::Unsupported => JumpKind::Error,
            _ if e.artifact.is_some() => JumpKind::Artifact,
            _ => return None,
        };
        // Artifact classification wins a label from the path; otherwise use a
        // trimmed one-line preview of the text.
        let label = match (&kind, &e.artifact) {
            (JumpKind::Artifact, Some(path)) => path.clone(),
            (JumpKind::Phase(p), _) => p.label().to_string(),
            _ => first_line_preview(&e.text, 60),
        };
        Some(JumpTarget {
            seq: e.seq,
            turn: e.turn,
            kind,
            label,
        })
    }
}

fn first_line_preview(text: &str, max: usize) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() > max {
        let truncated: String = line.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        line.to_string()
    }
}

/// "Since last viewed" model. Tracks the highest sequence the reader has caught
/// up to; everything strictly after it is unread.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnreadTracker {
    /// Highest `seq` marked viewed, or `None` if nothing has been viewed.
    last_viewed: Option<usize>,
}

impl UnreadTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore a persisted marker (e.g. from session metadata).
    pub fn restore(last_viewed: Option<usize>) -> Self {
        Self { last_viewed }
    }

    pub fn last_viewed(&self) -> Option<usize> {
        self.last_viewed
    }

    /// Whether `seq` is unread (strictly after the last-viewed marker).
    pub fn is_unread(&self, seq: usize) -> bool {
        match self.last_viewed {
            Some(v) => seq > v,
            None => true,
        }
    }

    /// The first unread `seq` given the current highest sequence in the doc,
    /// or `None` when fully caught up. `highest_seq` is the last valid seq.
    pub fn first_unread(&self, highest_seq: Option<usize>) -> Option<usize> {
        let highest = highest_seq?;
        match self.last_viewed {
            Some(v) if v >= highest => None,
            Some(v) => Some(v + 1),
            None => Some(0),
        }
    }

    /// Count of unread entries given the highest seq present.
    pub fn unread_count(&self, highest_seq: Option<usize>) -> usize {
        let Some(highest) = highest_seq else { return 0 };
        match self.last_viewed {
            Some(v) if v >= highest => 0,
            Some(v) => highest - v,
            None => highest + 1,
        }
    }

    /// Mark everything up to and including `seq` as viewed. Never moves the
    /// marker backwards.
    pub fn mark_viewed(&mut self, seq: usize) {
        self.last_viewed = Some(match self.last_viewed {
            Some(v) => v.max(seq),
            None => seq,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(turn: usize, role: NarrativeRole) -> Annotation {
        Annotation {
            turn,
            phase: None,
            role,
            agent: None,
        }
    }

    fn sample_index() -> NarrativeIndex {
        let mut idx = NarrativeIndex::new();
        idx.record(
            0,
            &ann(1, NarrativeRole::Prompt),
            "fix the parser bug",
            None,
        );
        idx.record(
            1,
            &ann(1, NarrativeRole::PhaseHeader(LocusPhase::Observe)),
            "Phase 1: OBSERVE",
            None,
        );
        idx.record(
            2,
            &ann(1, NarrativeRole::Decision),
            "Decision: add a ledger",
            None,
        );
        idx.record(
            3,
            &ann(1, NarrativeRole::Activity),
            "wrote file",
            Some("src/stream/ledger.rs"),
        );
        idx.record(4, &ann(1, NarrativeRole::Unsupported), "{unknown}", None);
        idx.record(
            5,
            &ann(1, NarrativeRole::Outcome),
            "Summary: shipped it",
            None,
        );
        idx
    }

    #[test]
    fn search_is_case_insensitive_substring() {
        let idx = sample_index();
        assert_eq!(idx.search("PARSER"), vec![0]);
        assert_eq!(idx.search("ledger"), vec![2]); // "add a ledger" text; path not searched
        assert!(idx.search("").is_empty(), "empty query matches nothing");
        assert!(idx.search("nonexistent").is_empty());
    }

    #[test]
    fn jump_targets_cover_all_navigable_kinds() {
        let idx = sample_index();
        let kinds: Vec<JumpKind> = idx.jump_targets().iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&JumpKind::Phase(LocusPhase::Observe)));
        assert!(kinds.contains(&JumpKind::Decision));
        assert!(kinds.contains(&JumpKind::Artifact));
        assert!(kinds.contains(&JumpKind::Error));
        assert!(kinds.contains(&JumpKind::Outcome));
    }

    #[test]
    fn counts_tally_navigable_kinds() {
        let idx = sample_index();
        let c = idx.counts();
        assert_eq!(c.phases, 1);
        assert_eq!(c.decisions, 1);
        assert_eq!(c.outcomes, 1);
        assert_eq!(c.errors, 1);
        assert_eq!(c.artifacts, 1);
        assert!(c.any());
        assert!(!NavCounts::default().any());
    }

    #[test]
    fn artifact_target_labels_with_path() {
        let idx = sample_index();
        let art = idx.jump_targets_of(JumpKind::Artifact);
        assert_eq!(art.len(), 1);
        assert_eq!(art[0].label, "src/stream/ledger.rs");
        assert_eq!(art[0].seq, 3);
    }

    #[test]
    fn jump_after_cycles_through_a_category() {
        // Two phases at seq 1 and (add another) — extend the sample with a
        // second phase to exercise wrapping.
        let mut idx = sample_index();
        idx.record(
            6,
            &ann(2, NarrativeRole::PhaseHeader(LocusPhase::Verify)),
            "Phase 6: VERIFY",
            None,
        );
        let is_phase = |k: &JumpKind| matches!(k, JumpKind::Phase(_));

        // From the start → first phase (seq 1).
        let first = idx.jump_after(None, is_phase).unwrap();
        assert_eq!(first.seq, 1);
        // After seq 1 → next phase (seq 6).
        let second = idx.jump_after(Some(1), is_phase).unwrap();
        assert_eq!(second.seq, 6);
        // After the last → wraps back to the first.
        let wrapped = idx.jump_after(Some(6), is_phase).unwrap();
        assert_eq!(wrapped.seq, 1);
        // A category with no targets → None.
        assert!(idx
            .jump_after(None, |k| matches!(k, JumpKind::Outcome) && false)
            .is_none());
    }

    #[test]
    fn next_target_finds_following_error() {
        let idx = sample_index();
        let t = idx
            .next_target(0, |k| matches!(k, JumpKind::Error))
            .unwrap();
        assert_eq!(t.seq, 4);
        // Nothing after the error is another error.
        assert!(idx
            .next_target(4, |k| matches!(k, JumpKind::Error))
            .is_none());
    }

    #[test]
    fn unread_tracker_defaults_everything_unread() {
        let t = UnreadTracker::new();
        assert!(t.is_unread(0));
        assert_eq!(t.unread_count(Some(4)), 5); // seqs 0..=4
        assert_eq!(t.first_unread(Some(4)), Some(0));
    }

    #[test]
    fn unread_tracker_marks_and_counts() {
        let mut t = UnreadTracker::new();
        t.mark_viewed(2);
        assert!(!t.is_unread(2));
        assert!(t.is_unread(3));
        assert_eq!(t.unread_count(Some(5)), 3); // 3,4,5
        assert_eq!(t.first_unread(Some(5)), Some(3));
        // Fully caught up.
        t.mark_viewed(5);
        assert_eq!(t.unread_count(Some(5)), 0);
        assert_eq!(t.first_unread(Some(5)), None);
    }

    #[test]
    fn mark_viewed_never_moves_backwards() {
        let mut t = UnreadTracker::new();
        t.mark_viewed(5);
        t.mark_viewed(2);
        assert_eq!(t.last_viewed(), Some(5));
    }
}
