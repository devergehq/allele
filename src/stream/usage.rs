//! Account quota and per-session context use (DEV-1253).
//!
//! Claude Code reports two different kinds of budget, and they belong to two
//! different owners:
//!
//! * **Quota** (`rate_limit_event`) is the *account's* rolling window — the
//!   5-hour limit, the weekly one. Every session signed in to one account sees
//!   the same number, so it lives in one [`AccountQuota`] per account, never on
//!   a session. A per-session copy is how two panes on one account end up
//!   showing 8% and 95%.
//! * **Context use** (per-call `usage`) is the *session's* — how much of the
//!   model's context window the last turn's prompt filled.
//!
//! A quota reading is only true until its window resets. Past that instant it
//! is reported as stale and its number is withheld, so a frozen figure can
//! never be mistaken for a live one.

use std::collections::BTreeMap;

use super::types::Usage;

/// Context window assumed for a model that does not say otherwise.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
/// Context window of the extended-context (`[1m]`) variants.
pub const LARGE_CONTEXT_WINDOW: u64 = 1_000_000;

// ── Context use ───────────────────────────────────────────────────

/// How full one session's context window was after its latest model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUse {
    /// Prompt tokens the call consumed: fresh input plus both cache classes.
    pub used_tokens: u64,
    /// The window those tokens are measured against.
    pub window_tokens: u64,
}

impl ContextUse {
    /// Context use implied by one call's `usage`. `None` when the usage block
    /// carries no prompt tokens at all (nothing to report).
    ///
    /// Output tokens are left out on purpose: they are what the call *added*,
    /// and they are counted as prompt on the next call. This matches what
    /// Claude Code's own `/context` reports for the same turn.
    pub fn from_usage(usage: &Usage, model: Option<&str>) -> Option<Self> {
        let used = usage.input_tokens.unwrap_or(0)
            + usage.cache_creation_input_tokens.unwrap_or(0)
            + usage.cache_read_input_tokens.unwrap_or(0);
        if used == 0 {
            return None;
        }
        Some(Self {
            used_tokens: used,
            window_tokens: context_window_for(model, used),
        })
    }

    /// Whole-number percentage of the window in use.
    pub fn percent(&self) -> u64 {
        if self.window_tokens == 0 {
            return 0;
        }
        (self.used_tokens * 100 + self.window_tokens / 2) / self.window_tokens
    }

    /// The quiet sidebar readout, e.g. `ctx 62%`.
    pub fn label(&self) -> String {
        format!("ctx {}%", self.percent())
    }
}

/// The context window to measure against. The transcript names the model but
/// not its window, so this is inferred: an explicit `[1m]` / `-1m` variant is
/// the large window, and so is any call whose prompt already exceeded the
/// default one (it could not have, on a 200k model).
fn context_window_for(model: Option<&str>, used: u64) -> u64 {
    let large = model.is_some_and(|m| {
        let m = m.to_ascii_lowercase();
        m.contains("[1m]") || m.ends_with("-1m")
    });
    if large || used > DEFAULT_CONTEXT_WINDOW {
        LARGE_CONTEXT_WINDOW
    } else {
        DEFAULT_CONTEXT_WINDOW
    }
}

// ── Quota ─────────────────────────────────────────────────────────

/// One `rate_limit_event` reading for one limit window.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaReading {
    /// Which window this is (`five_hour`, `seven_day`, …).
    pub window: String,
    /// Fraction of the window used, `0.0..=1.0`, when the event carried one.
    pub used_fraction: Option<f64>,
    /// Claude Code's coarse verdict (`allowed`, `allowed_warning`, `rejected`).
    pub status: Option<String>,
    /// When the window resets, as Unix seconds.
    pub resets_at: Option<u64>,
}

impl QuotaReading {
    /// Read a `rate_limit_info` object. The payload is not a documented
    /// contract, so field names are accepted in both the camelCase Claude Code
    /// emits today and snake_case, and a percentage is accepted where a
    /// fraction is expected. `None` when the object says nothing usable.
    pub fn from_info(info: &serde_json::Value) -> Option<Self> {
        let field = |names: &[&str]| names.iter().find_map(|n| info.get(*n));

        let window = field(&["rateLimitType", "rate_limit_type", "window"])
            .and_then(|v| v.as_str())
            .unwrap_or("five_hour")
            .to_string();

        // A field named as a percentage is always one — `used_percentage: 1`
        // is 1%, not 100%. A fraction field above 1 can only be a percentage
        // sent under the wrong name.
        let percent = field(&["usedPercentage", "used_percentage"])
            .and_then(|v| v.as_f64())
            .map(|v| v / 100.0);
        let fraction = || {
            field(&["utilization", "used_fraction"])
                .and_then(|v| v.as_f64())
                .map(|v| if v > 1.0 { v / 100.0 } else { v })
        };
        let used_fraction = percent.or_else(fraction).map(|v| v.clamp(0.0, 1.0));

        let status = field(&["status"])
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let resets_at = field(&["resetsAt", "resets_at", "resetAt", "reset_at"])
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_f64().map(|f| f as u64))
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            // Milliseconds when the value is too large to be seconds.
            .map(|t| if t > 100_000_000_000 { t / 1000 } else { t });

        if used_fraction.is_none() && status.is_none() && resets_at.is_none() {
            return None;
        }
        Some(Self {
            window,
            used_fraction,
            status,
            resets_at,
        })
    }

    /// Read the `rate_limits` object Claude Code hands its status-line command
    /// (DEV-1259): one entry per window, keyed by the window's name, e.g.
    /// `{"five_hour":{"used_percentage":71,"resets_at":1762790400}}`. Each
    /// entry goes through [`from_info`](Self::from_info), so it is held to
    /// the same lenient field rules as a `rate_limit_event`.
    pub fn from_status_line(rate_limits: &serde_json::Value) -> Vec<Self> {
        let Some(windows) = rate_limits.as_object() else {
            return Vec::new();
        };
        windows
            .iter()
            .filter_map(|(window, info)| {
                let mut r = Self::from_info(info)?;
                r.window = window.clone();
                Some(r)
            })
            .collect()
    }

    /// True once the window this reading describes has reset.
    pub fn is_stale(&self, now_unix: u64) -> bool {
        self.resets_at.is_some_and(|r| now_unix >= r)
    }
}

/// The quota of one account. Every session on the account reads this one
/// value, which is what makes two sessions agree by construction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountQuota {
    readings: BTreeMap<String, QuotaReading>,
}

impl AccountQuota {
    /// Fold in a reading. Returns `true` when what would be shown changed.
    ///
    /// Readings arrive from several sessions in no particular order, so an
    /// older one must not overwrite a newer one: a reading for an earlier reset
    /// than the one held is from a window that has already rolled over.
    pub fn record(&mut self, reading: QuotaReading) -> bool {
        if let Some(held) = self.readings.get(&reading.window) {
            if let (Some(held_reset), Some(new_reset)) = (held.resets_at, reading.resets_at) {
                if new_reset < held_reset {
                    return false;
                }
            }
            if *held == reading {
                return false;
            }
        }
        self.readings.insert(reading.window.clone(), reading);
        true
    }

    /// The reading for the window worth showing: the 5-hour one when present,
    /// otherwise whichever window was seen.
    fn primary(&self) -> Option<&QuotaReading> {
        self.readings
            .get("five_hour")
            .or_else(|| self.readings.values().next())
    }

    /// The one-line account readout, or `None` before any reading has arrived.
    ///
    /// Live: `5h 71% · resets in 2h 14m`. Stale: `5h quota stale · reset 40m
    /// ago` — the percentage is withheld, because after the reset it describes
    /// a window that no longer exists.
    pub fn line(&self, now_unix: u64) -> Option<String> {
        let r = self.primary()?;
        let window = window_label(&r.window);
        if r.is_stale(now_unix) {
            let ago = now_unix.saturating_sub(r.resets_at.unwrap_or(now_unix));
            return Some(format!(
                "{window} quota stale · reset {} ago",
                short_duration(ago)
            ));
        }
        let level = match (r.used_fraction, r.status.as_deref()) {
            (Some(f), _) => format!("{}%", (f * 100.0).round() as u64),
            (None, Some("allowed")) => "ok".to_string(),
            (None, Some("allowed_warning")) => "near limit".to_string(),
            (None, Some("rejected")) => "limited".to_string(),
            (None, Some(other)) => other.to_string(),
            (None, None) => "seen".to_string(),
        };
        Some(match r.resets_at {
            Some(reset) => format!(
                "{window} {level} · resets in {}",
                short_duration(reset - now_unix)
            ),
            None => format!("{window} {level}"),
        })
    }
}

/// Short human name of a limit window.
fn window_label(window: &str) -> String {
    match window {
        "five_hour" => "5h".to_string(),
        "seven_day" => "7d".to_string(),
        "seven_day_opus" => "7d opus".to_string(),
        "seven_day_sonnet" => "7d sonnet".to_string(),
        other => other.replace('_', " "),
    }
}

/// `2h 14m`, `40m`, `<1m`.
fn short_duration(secs: u64) -> String {
    let mins = secs / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "<1m".to_string(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Current wall-clock time as Unix seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Transcript tail ───────────────────────────────────────────────

/// What the end of a session transcript says about budget: the latest context
/// use on the main chain, and every quota reading seen.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TailSummary {
    pub context: Option<ContextUse>,
    pub quota: Vec<QuotaReading>,
}

/// How much of a transcript's end is read. A turn's final assistant line sits
/// at the very end; this is ample for it without reading a long session whole.
pub const TAIL_BYTES: u64 = 256 * 1024;

/// Summarise the lines of a transcript tail. Sidechain (subagent) calls are
/// skipped — their context is the subagent's, not the session's.
pub fn summarize_lines<'a>(lines: impl Iterator<Item = &'a str>) -> TailSummary {
    let mut out = TailSummary::default();
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if v.get("isSidechain").and_then(|s| s.as_bool()) == Some(true) {
                    continue;
                }
                let Some(msg) = v.get("message") else {
                    continue;
                };
                let usage = msg
                    .get("usage")
                    .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok());
                let model = msg.get("model").and_then(|m| m.as_str());
                if let Some(ctx) = usage.and_then(|u| ContextUse::from_usage(&u, model)) {
                    out.context = Some(ctx);
                }
            }
            Some("rate_limit_event") => {
                if let Some(r) = v.get("rate_limit_info").and_then(QuotaReading::from_info) {
                    out.quota.push(r);
                }
            }
            _ => {}
        }
    }
    out
}

/// Read the last [`TAIL_BYTES`] of `path` and summarise it. `None` when the
/// file cannot be read. Blocking — call it off the foreground thread.
pub fn summarize_tail(path: &std::path::Path) -> Option<TailSummary> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        // The first line is almost certainly cut mid-record.
        lines.next();
    }
    Some(summarize_lines(lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, create: u64, read: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(500),
            cache_creation_input_tokens: Some(create),
            cache_read_input_tokens: Some(read),
        }
    }

    fn reading(used: f64, resets_at: u64) -> QuotaReading {
        QuotaReading {
            window: "five_hour".into(),
            used_fraction: Some(used),
            status: Some("allowed".into()),
            resets_at: Some(resets_at),
        }
    }

    #[test]
    fn context_use_counts_prompt_tokens_against_200k() {
        let ctx = ContextUse::from_usage(&usage(2, 2_441, 121_557), Some("claude-opus-5-5"))
            .expect("usage has prompt tokens");
        assert_eq!(ctx.used_tokens, 124_000);
        assert_eq!(ctx.window_tokens, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(ctx.label(), "ctx 62%");
    }

    #[test]
    fn context_use_uses_the_large_window_for_1m_models_and_overflow() {
        let named =
            ContextUse::from_usage(&usage(0, 0, 100_000), Some("claude-opus-5-5[1m]")).unwrap();
        assert_eq!(named.window_tokens, LARGE_CONTEXT_WINDOW);
        assert_eq!(named.percent(), 10);
        let inferred = ContextUse::from_usage(&usage(0, 0, 300_000), None).unwrap();
        assert_eq!(inferred.window_tokens, LARGE_CONTEXT_WINDOW);
    }

    #[test]
    fn empty_usage_reports_nothing() {
        assert!(ContextUse::from_usage(&usage(0, 0, 0), None).is_none());
    }

    #[test]
    fn quota_reading_parses_claude_codes_camel_case() {
        let info = serde_json::json!({
            "status": "allowed_warning",
            "resetsAt": 1_762_790_400u64,
            "rateLimitType": "five_hour",
            "utilization": 0.71
        });
        let r = QuotaReading::from_info(&info).unwrap();
        assert_eq!(r.window, "five_hour");
        assert_eq!(r.used_fraction, Some(0.71));
        assert_eq!(r.resets_at, Some(1_762_790_400));
        assert_eq!(r.status.as_deref(), Some("allowed_warning"));
    }

    #[test]
    fn quota_reading_accepts_percent_and_millis() {
        let info = serde_json::json!({"used_percentage": 42.0, "resets_at": 1_762_790_400_000u64});
        let r = QuotaReading::from_info(&info).unwrap();
        assert_eq!(r.used_fraction, Some(0.42));
        assert_eq!(r.resets_at, Some(1_762_790_400));
    }

    #[test]
    fn quota_reading_with_nothing_usable_is_none() {
        assert!(QuotaReading::from_info(&serde_json::json!({"remaining": 10})).is_none());
    }

    #[test]
    fn status_line_rate_limits_yield_one_reading_per_window() {
        let rl = serde_json::json!({
            "five_hour": {"used_percentage": 71.0, "resets_at": 1_762_790_400u64},
            "seven_day": {"used_percentage": 12, "resets_at": 1_763_200_000u64},
            "junk": {"nothing": true}
        });
        let mut readings = QuotaReading::from_status_line(&rl);
        readings.sort_by(|a, b| a.window.cmp(&b.window));
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].window, "five_hour");
        assert_eq!(readings[0].used_fraction, Some(0.71));
        assert_eq!(readings[0].resets_at, Some(1_762_790_400));
        assert_eq!(readings[1].window, "seven_day");
        assert_eq!(readings[1].used_fraction, Some(0.12));
        assert!(QuotaReading::from_status_line(&serde_json::json!(null)).is_empty());
        // A low percentage is a percentage, not a fraction.
        let low = QuotaReading::from_status_line(&serde_json::json!({
            "five_hour": {"used_percentage": 1, "resets_at": 5}
        }));
        assert_eq!(low[0].used_fraction, Some(0.01));
    }

    #[test]
    fn live_quota_shows_percent_and_time_to_reset() {
        let mut q = AccountQuota::default();
        q.record(reading(0.71, 10_000 + 2 * 3600 + 14 * 60));
        assert_eq!(q.line(10_000).as_deref(), Some("5h 71% · resets in 2h 14m"));
    }

    #[test]
    fn quota_past_its_reset_renders_stale_without_a_number() {
        let mut q = AccountQuota::default();
        q.record(reading(0.71, 10_000));
        let line = q.line(10_000 + 40 * 60).unwrap();
        assert_eq!(line, "5h quota stale · reset 40m ago");
        assert!(
            !line.contains('%'),
            "a stale reading must not show a live number"
        );
        // The reset instant itself is already stale.
        assert!(q.line(10_000).unwrap().contains("stale"));
    }

    #[test]
    fn an_older_window_does_not_overwrite_a_newer_one() {
        let mut q = AccountQuota::default();
        assert!(q.record(reading(0.10, 20_000)));
        assert!(!q.record(reading(0.95, 10_000)));
        assert!(q.line(15_000).unwrap().contains("10%"));
    }

    #[test]
    fn the_five_hour_window_is_preferred() {
        let mut q = AccountQuota::default();
        q.record(QuotaReading {
            window: "seven_day".into(),
            ..reading(0.30, 99_999)
        });
        q.record(reading(0.50, 99_999));
        assert!(q.line(0).unwrap().starts_with("5h 50%"));
    }

    #[test]
    fn tail_summary_takes_the_last_main_chain_usage() {
        let lines = [
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"cache_read_input_tokens":20000}}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[],"usage":{"input_tokens":1,"cache_read_input_tokens":190000}}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"utilization":0.2,"resetsAt":500}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[],"usage":{"input_tokens":1,"cache_read_input_tokens":40000}}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[],"usage":{"input_tokens":1,"cache_read_input_tokens":190000}}}"#,
            "{truncated",
        ];
        let s = summarize_lines(lines.into_iter());
        assert_eq!(s.context.unwrap().used_tokens, 40_001);
        assert_eq!(s.quota.len(), 1);
    }

    #[test]
    fn tail_read_skips_a_cut_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let filler = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"{}\"}}}}\n",
            "x".repeat(TAIL_BYTES as usize)
        );
        let last = r#"{"type":"assistant","message":{"content":[],"usage":{"input_tokens":5,"cache_read_input_tokens":1000}}}"#;
        std::fs::write(&path, format!("{filler}{last}\n")).unwrap();
        let s = summarize_tail(&path).unwrap();
        assert_eq!(s.context.unwrap().used_tokens, 1_005);
        assert!(summarize_tail(&dir.path().join("missing.jsonl")).is_none());
    }
}
