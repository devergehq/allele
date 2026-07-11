//! Lossless session event ledger (DEV-33).
//!
//! The [`StreamParser`] turns wire-format lines into `RichEvent`s for
//! rendering, but normalisation is inherently lossy: unknown event types,
//! unrecognised content blocks, and fields we don't model all disappear once
//! a line has been projected. The ledger is the counterweight — a durable,
//! append-only record that stores, for **every** source line:
//!
//!   * the exact raw bytes of the line (never mutated, never filtered),
//!   * the parsed JSON value (when the line was valid JSON),
//!   * the normalised `RichEvent`s produced (possibly empty, possibly a
//!     `Fallback`),
//!   * a [`Coverage`] classification, and
//!   * per-line diagnostics.
//!
//! Its central guarantee is a **corpus round-trip**: concatenating every
//! entry's raw form reproduces the original input exactly, even when
//! normalisation was unsupported. Filters and views may hide entries, but the
//! ledger itself never destroys information.

use super::parser::{Coverage, ParsedLine, StreamParser};
use super::types::RichEvent;

/// Where an ingested line came from. Subagent lines carry their agent id and
/// whether they were historical (written by a *previous* agent invocation) so
/// the ledger can retain them even though the live view may choose to skip
/// them to avoid anchoring the viewport in the past.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSource {
    Main,
    Subagent { agent_id: String, historical: bool },
}

/// One ledger row: a single source line and everything derived from it.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    /// Monotonic ingestion index, unique within a ledger.
    pub seq: u64,
    pub source: EventSource,
    /// The exact raw line as ingested (no trailing newline).
    pub raw: String,
    /// Parsed JSON, or `None` when `raw` was not valid JSON.
    pub json: Option<serde_json::Value>,
    /// Normalised events emitted for this line (may be empty or a `Fallback`).
    pub events: Vec<RichEvent>,
    pub coverage: Coverage,
    pub diagnostics: Vec<String>,
}

/// Aggregate parser-coverage counters over everything ingested so far.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CoverageStats {
    pub total_lines: u64,
    pub full: u64,
    pub partial: u64,
    pub ignored: u64,
    pub fallback: u64,
    pub unparsed: u64,
    /// Total normalised events emitted across all lines.
    pub total_events: u64,
    /// Total diagnostic messages recorded across all lines.
    pub total_diagnostics: u64,
}

impl CoverageStats {
    fn record(&mut self, entry: &LedgerEntry) {
        self.total_lines += 1;
        self.total_events += entry.events.len() as u64;
        self.total_diagnostics += entry.diagnostics.len() as u64;
        match entry.coverage {
            Coverage::Full => self.full += 1,
            Coverage::Partial => self.partial += 1,
            Coverage::Ignored => self.ignored += 1,
            Coverage::Fallback => self.fallback += 1,
            Coverage::Unparsed => self.unparsed += 1,
        }
    }

    /// Lines that produced at least one `Fallback` event or failed to parse —
    /// the surface area a human might want to inspect for parser gaps.
    pub fn needs_attention(&self) -> u64 {
        self.partial + self.fallback + self.unparsed
    }
}

/// Append-only lossless record of a session's event stream.
#[derive(Default)]
pub struct SessionLedger {
    entries: Vec<LedgerEntry>,
    stats: CoverageStats,
    next_seq: u64,
}

impl SessionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest one raw line, driving `parser` to normalise it, and append the
    /// resulting entry. Returns a clone of the normalised events so callers
    /// can forward them to a renderer without borrowing the ledger.
    ///
    /// `parser` is threaded in (rather than owned) because the tailer keeps a
    /// distinct parser per file to preserve per-file `init`/session state.
    pub fn ingest(
        &mut self,
        parser: &mut StreamParser,
        source: EventSource,
        raw_line: &str,
    ) -> Vec<RichEvent> {
        let ParsedLine {
            events,
            coverage,
            diagnostics,
        } = parser.feed_line_detailed(raw_line);
        let entry = LedgerEntry {
            seq: self.next_seq,
            source,
            raw: raw_line.to_string(),
            json: serde_json::from_str(raw_line).ok(),
            events: events.clone(),
            coverage,
            diagnostics,
        };
        self.next_seq += 1;
        self.stats.record(&entry);
        self.entries.push(entry);
        events
    }

    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn stats(&self) -> &CoverageStats {
        &self.stats
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Reconstruct the exact original input from the retained raw lines. Used
    /// to prove losslessness: this must equal the joined source lines byte for
    /// byte, regardless of how each line was (or wasn't) normalised.
    pub fn reconstruct(&self) -> String {
        let mut out = String::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&entry.raw);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative corpus mixing recognised, ignored, unknown, and
    /// malformed lines — plus an unknown content block inside a good line.
    const CORPUS: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude","tools":["Read"]}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}],"stop_reason":null},"session_id":"s1"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Grep","input":{"pattern":"x"}}],"stop_reason":null}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"match"}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking","data":"opaque"}],"stop_reason":null}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"remaining":5}}"#,
        r#"{"type":"totally_new_event","payload":42}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"num_turns":1,"total_cost_usd":0.01}"#,
        r#"{ this is not json"#,
    ];

    fn ingest_corpus() -> SessionLedger {
        let mut ledger = SessionLedger::new();
        let mut parser = StreamParser::new();
        for line in CORPUS {
            ledger.ingest(&mut parser, EventSource::Main, line);
        }
        ledger
    }

    #[test]
    fn corpus_round_trip_is_lossless() {
        let ledger = ingest_corpus();
        let original = CORPUS.join("\n");
        assert_eq!(
            ledger.reconstruct(),
            original,
            "raw lines must reconstruct the source exactly"
        );
    }

    #[test]
    fn every_line_becomes_exactly_one_entry() {
        let ledger = ingest_corpus();
        assert_eq!(ledger.len(), CORPUS.len());
        assert_eq!(ledger.stats().total_lines as usize, CORPUS.len());
        // Sequence numbers are dense and monotonic.
        for (i, entry) in ledger.entries().iter().enumerate() {
            assert_eq!(entry.seq as usize, i);
        }
    }

    #[test]
    fn unsupported_lines_remain_inspectable() {
        let ledger = ingest_corpus();
        // The unknown top-level type and the invalid-JSON line both survive as
        // raw, inspectable entries carrying Fallback events.
        let unknown = ledger
            .entries()
            .iter()
            .find(|e| e.raw.contains("totally_new_event"))
            .expect("unknown event retained");
        assert_eq!(unknown.coverage, Coverage::Fallback);
        assert!(matches!(
            unknown.events.first(),
            Some(RichEvent::Fallback { .. })
        ));

        let bad = ledger
            .entries()
            .iter()
            .find(|e| e.raw.starts_with("{ this"))
            .expect("malformed line retained");
        assert_eq!(bad.coverage, Coverage::Unparsed);
        assert!(bad.json.is_none());
        assert!(matches!(
            bad.events.first(),
            Some(RichEvent::Fallback { .. })
        ));
    }

    #[test]
    fn coverage_stats_are_accurate() {
        let ledger = ingest_corpus();
        let s = ledger.stats();
        assert_eq!(s.total_lines, CORPUS.len() as u64);
        assert_eq!(s.unparsed, 1, "one malformed line");
        assert_eq!(s.fallback, 1, "one unknown top-level type");
        assert_eq!(s.partial, 1, "one line with an unrecognised content block");
        assert_eq!(
            s.ignored, 1,
            "the rate-limit line emits nothing but is retained"
        );
        // init + 2 assistant(text/tool) + user + result all fully covered.
        assert_eq!(s.full, 5);
        assert_eq!(s.needs_attention(), 3);
    }

    #[test]
    fn historical_subagent_lines_are_ingested() {
        let mut ledger = SessionLedger::new();
        let mut parser = StreamParser::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"sub"}],"stop_reason":null}}"#;
        ledger.ingest(
            &mut parser,
            EventSource::Subagent {
                agent_id: "a1".into(),
                historical: true,
            },
            line,
        );
        let entry = &ledger.entries()[0];
        assert_eq!(
            entry.source,
            EventSource::Subagent {
                agent_id: "a1".into(),
                historical: true
            }
        );
        assert_eq!(entry.coverage, Coverage::Full);
    }
}
