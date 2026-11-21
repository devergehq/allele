//! The facts the catch-up parts are handed (DEV-1353).
//!
//! Plain data only: strings, counts and Unix-epoch seconds. Nothing here
//! holds a PTY, a git repo or a GPUI entity, so the same value can be built
//! from a local clone (this Mac) or sent across the link as a digest and drawn
//! on another Mac (DEV-1272), which has no clone to count +/− from.
//!
//! Everything time-relative ("2h 14m ago", "3m 40s so far") is computed at
//! draw time from an explicit `now`, so a digest does not go stale in transit
//! and the logic is testable without a clock.

use serde::{Deserialize, Serialize};

/// An absence at least this long opens the strip on its own (guess 2,
/// confirmed by Patrick on 21 Nov 2026). A shorter one leaves it as it was.
pub const AWAY_OPENS_AFTER_SECS: u64 = 10 * 60;

/// Everything the strip shows for one session.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CatchUpFacts {
    /// How long you had been away when you switched back in, in seconds.
    /// `None` when Allele has no record of you viewing it before.
    pub away_secs: Option<u64>,
    /// Your last prompt and the latest line of the reply. `None` before the
    /// first prompt, or when the transcript could not be read.
    pub prompt: Option<PromptFacts>,
    /// What changed since you last looked. `None` when unknown (not a git
    /// work tree, no baseline yet, or still being counted).
    pub changes: Option<ChangeFacts>,
    /// What the session needs from you now.
    pub wants: WantsNow,
}

/// Part 1: the prompt line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFacts {
    /// The prompt you sent, as typed.
    pub text: String,
    /// When you sent it (Unix seconds), if the transcript said.
    pub asked_at: Option<u64>,
    /// One line of Claude's latest reply to it, if it has said anything yet.
    pub reply: Option<String>,
}

/// One changed file in the change summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    /// Git reported no line counts (binary, or too large to count).
    pub binary: bool,
}

/// Part 2: the change summary.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChangeFacts {
    /// Files that differ from when you last looked, sorted by path.
    pub files: Vec<FileChange>,
    /// Some of the work tree is not committed.
    pub uncommitted: bool,
}

impl ChangeFacts {
    /// Total lines added and removed across every file.
    pub fn totals(&self) -> (u64, u64) {
        self.files.iter().fold((0, 0), |(a, r), f| {
            (a + u64::from(f.added), r + u64::from(f.removed))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Which "only the terminal can answer" ask is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalAsk {
    /// Claude's plan-approval menu (`ExitPlanMode`).
    Plan,
    /// A multiple-choice question (`AskUserQuestion`) or any other menu.
    Question,
}

/// Part 3: the "wants now" row, in its four states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WantsNow {
    /// Finished; waiting for your next prompt.
    Nothing {
        /// When the turn finished (Unix seconds), if known.
        finished_at: Option<u64>,
    },
    /// Still working.
    Working {
        /// What it is doing right now ("Running cargo build"), if known.
        activity: Option<String>,
        /// When this stretch of work started (Unix seconds).
        since: u64,
    },
    /// Blocked on a permission prompt. Allow once and Deny can answer it.
    Permission {
        /// The tool that wants to run ("Bash").
        tool: String,
        /// What it wants to do, as a phrase ("run a shell command").
        purpose: String,
        /// What it would act on (a command, a path).
        target: Option<String>,
    },
    /// Blocked on Claude's own menu. Only the terminal can answer it.
    TerminalOnly { ask: TerminalAsk },
}

impl Default for WantsNow {
    fn default() -> Self {
        WantsNow::Nothing { finished_at: None }
    }
}

impl WantsNow {
    /// The state word that leads the strip's one-line summary.
    pub fn state_label(&self) -> &'static str {
        match self {
            WantsNow::Nothing { .. } => "Finished",
            WantsNow::Working { .. } => "Still working",
            WantsNow::Permission { .. } | WantsNow::TerminalOnly { .. } => "Blocked",
        }
    }

    /// The row's title.
    pub fn title(&self) -> String {
        match self {
            WantsNow::Nothing { .. } => "Wants nothing".into(),
            WantsNow::Working { .. } => "Nothing yet · still working".into(),
            WantsNow::Permission { purpose, .. } => format!("Wants to {purpose}"),
            WantsNow::TerminalOnly {
                ask: TerminalAsk::Plan,
            } => "Wants you to approve its plan".into(),
            WantsNow::TerminalOnly {
                ask: TerminalAsk::Question,
            } => "Wants you to pick an answer".into(),
        }
    }

    /// The row's second line. `None` for a permission (its target is drawn
    /// in monospace instead).
    pub fn detail(&self, now: u64) -> Option<String> {
        match self {
            WantsNow::Nothing {
                finished_at: Some(t),
            } => Some(format!(
                "Finished at {}. Waiting for your next prompt.",
                clock_time(*t)
            )),
            WantsNow::Nothing { finished_at: None } => Some("Waiting for your next prompt.".into()),
            WantsNow::Working { activity, since } => {
                let so_far = format!("{} so far", duration_short(now.saturating_sub(*since)));
                Some(match activity {
                    Some(a) => format!("{a} · {so_far}"),
                    None => format!("Working · {so_far}"),
                })
            }
            WantsNow::Permission { .. } => None,
            WantsNow::TerminalOnly { .. } => Some(
                "This is Claude's own menu, answered with the arrow keys. \
                 Only the terminal can answer it."
                    .into(),
            ),
        }
    }

    /// The short clause used in the folded one-line summary.
    pub fn summary_clause(&self) -> String {
        match self {
            WantsNow::Nothing { .. } => "wants nothing".into(),
            WantsNow::Working { activity, .. } => activity
                .as_deref()
                .map(lowercase_first)
                .unwrap_or_else(|| "working".into()),
            WantsNow::Permission {
                purpose, target, ..
            } => match target {
                Some(t) => format!("wants to run {}", first_words(t, 3)),
                None => format!("wants to {purpose}"),
            },
            WantsNow::TerminalOnly {
                ask: TerminalAsk::Plan,
            } => "wants you to approve its plan".into(),
            WantsNow::TerminalOnly {
                ask: TerminalAsk::Question,
            } => "wants you to pick an answer".into(),
        }
    }
}

impl CatchUpFacts {
    /// The folded strip's one line, minus the state word and away time,
    /// which are drawn in their own colours: "You asked: … · 3 files +181 −6
    /// · wants nothing".
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = &self.prompt {
            parts.push(format!("You asked: {}", first_line(&p.text)));
        }
        if let Some(c) = self.changes.as_ref().filter(|c| !c.is_empty()) {
            let (a, r) = c.totals();
            let so_far = if matches!(self.wants, WantsNow::Working { .. }) {
                " so far"
            } else {
                ""
            };
            parts.push(format!(
                "{} {} +{a} −{r}{so_far}",
                c.files.len(),
                if c.files.len() == 1 { "file" } else { "files" }
            ));
        }
        parts.push(self.wants.summary_clause());
        parts.join(" · ")
    }
}

/// Whether switching in after `away_secs` should open the strip by itself.
/// With no record of a previous view there is nothing to catch up on.
pub fn opens_on_arrival(away_secs: Option<u64>) -> bool {
    away_secs.is_some_and(|s| s >= AWAY_OPENS_AFTER_SECS)
}

/// "2h 14m", "25m", "40s". Coarse on purpose: it is read at a glance.
pub fn duration_short(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let (m, s) = (secs / 60, secs % 60);
        if m < 10 && s > 0 {
            format!("{m}m {s}s")
        } else {
            format!("{m}m")
        }
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        let (d, h) = (secs / 86_400, (secs % 86_400) / 3600);
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d {h}h")
        }
    }
}

/// "2:31 pm" in the local time zone.
pub fn clock_time(epoch_secs: u64) -> String {
    let (hour, minute) = local_hour_minute(epoch_secs);
    let (h12, suffix) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    format!("{h12}:{minute:02} {suffix}")
}

fn local_hour_minute(epoch_secs: u64) -> (u32, u32) {
    let t = epoch_secs as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` we own and reads `t`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        (tm.tm_hour as u32, tm.tm_min as u32)
    } else {
        (
            ((epoch_secs / 3600) % 24) as u32,
            ((epoch_secs / 60) % 60) as u32,
        )
    }
}

/// The first non-empty line, trimmed.
pub fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn first_words(text: &str, n: usize) -> String {
    text.split_whitespace()
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(files: &[(&str, u32, u32)]) -> ChangeFacts {
        ChangeFacts {
            files: files
                .iter()
                .map(|(p, a, r)| FileChange {
                    path: p.to_string(),
                    added: *a,
                    removed: *r,
                    binary: false,
                })
                .collect(),
            uncommitted: true,
        }
    }

    #[test]
    fn ten_minutes_away_opens_the_strip() {
        assert!(opens_on_arrival(Some(AWAY_OPENS_AFTER_SECS)));
        assert!(opens_on_arrival(Some(2 * 3600 + 14 * 60)));
    }

    #[test]
    fn a_short_absence_does_not_open_it() {
        assert!(!opens_on_arrival(Some(AWAY_OPENS_AFTER_SECS - 1)));
        assert!(!opens_on_arrival(Some(120)));
    }

    #[test]
    fn no_record_of_a_view_does_not_open_it() {
        assert!(!opens_on_arrival(None));
    }

    #[test]
    fn totals_add_up_every_file() {
        let c = changes(&[("a.rs", 41, 6), ("b.rs", 88, 0), ("c.rs", 52, 0)]);
        assert_eq!(c.totals(), (181, 6));
    }

    #[test]
    fn summary_line_matches_the_drawing() {
        let facts = CatchUpFacts {
            away_secs: Some(8040),
            prompt: Some(PromptFacts {
                text: "move target/ to the Trash on discard\nmore".into(),
                asked_at: None,
                reply: None,
            }),
            changes: Some(changes(&[
                ("a.rs", 41, 6),
                ("b.rs", 88, 0),
                ("c.rs", 52, 0),
            ])),
            wants: WantsNow::Nothing { finished_at: None },
        };
        assert_eq!(
            facts.summary_line(),
            "You asked: move target/ to the Trash on discard · 3 files +181 −6 · wants nothing"
        );
    }

    #[test]
    fn summary_line_leaves_out_an_empty_change_summary() {
        let facts = CatchUpFacts {
            changes: Some(ChangeFacts::default()),
            ..Default::default()
        };
        assert_eq!(facts.summary_line(), "wants nothing");
    }

    #[test]
    fn running_changes_read_so_far() {
        let facts = CatchUpFacts {
            changes: Some(changes(&[("a.rs", 41, 6)])),
            wants: WantsNow::Working {
                activity: Some("Running cargo build".into()),
                since: 0,
            },
            ..Default::default()
        };
        assert_eq!(
            facts.summary_line(),
            "1 file +41 −6 so far · running cargo build"
        );
    }

    #[test]
    fn working_detail_counts_up_from_since() {
        let w = WantsNow::Working {
            activity: Some("Running cargo build".into()),
            since: 1000,
        };
        assert_eq!(
            w.detail(1220).as_deref(),
            Some("Running cargo build · 3m 40s so far")
        );
        assert_eq!(
            w.detail(1230).as_deref(),
            Some("Running cargo build · 3m 50s so far")
        );
    }

    #[test]
    fn permission_reads_as_its_purpose() {
        let w = WantsNow::Permission {
            tool: "Bash".into(),
            purpose: "run a command".into(),
            target: Some("cargo test -p allele clone::trash".into()),
        };
        assert_eq!(w.title(), "Wants to run a command");
        assert_eq!(w.summary_clause(), "wants to run cargo test -p");
        assert_eq!(w.state_label(), "Blocked");
    }

    #[test]
    fn plan_approval_is_terminal_only() {
        let w = WantsNow::TerminalOnly {
            ask: TerminalAsk::Plan,
        };
        assert_eq!(w.title(), "Wants you to approve its plan");
        assert!(w
            .detail(0)
            .unwrap()
            .contains("Only the terminal can answer it"));
    }

    #[test]
    fn durations_are_coarse() {
        assert_eq!(duration_short(40), "40s");
        assert_eq!(duration_short(220), "3m 40s");
        assert_eq!(duration_short(25 * 60), "25m");
        assert_eq!(duration_short(2 * 3600 + 14 * 60), "2h 14m");
        assert_eq!(duration_short(3 * 86_400), "3d");
    }

    #[test]
    fn clock_time_is_twelve_hour() {
        let s = clock_time(0);
        assert!(s.ends_with(" am") || s.ends_with(" pm"), "{s}");
    }

    #[test]
    fn facts_round_trip_through_json() {
        let facts = CatchUpFacts {
            away_secs: Some(700),
            prompt: Some(PromptFacts {
                text: "hi".into(),
                asked_at: Some(5),
                reply: Some("Done.".into()),
            }),
            changes: Some(changes(&[("a.rs", 1, 2)])),
            wants: WantsNow::TerminalOnly {
                ask: TerminalAsk::Plan,
            },
        };
        let json = serde_json::to_string(&facts).expect("ser");
        let back: CatchUpFacts = serde_json::from_str(&json).expect("de");
        assert_eq!(back, facts);
    }
}
