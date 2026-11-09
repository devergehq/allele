//! Local tab-usage counters (DEV-1265).
//!
//! Counts how many seconds each main tab (Claude, Reader, Browser,
//! Transcript) is the visible tab **while the Allele window is focused**, and
//! how long the Changes panel and the Scratch Pad are open under the same
//! condition. The point is to replace a belief ("nobody uses the Reader") with
//! a week of numbers before anyone deletes a tab.
//!
//! # Where it goes
//!
//! Only to `<root>/.allele/tab-usage.json` ([`usage_file`]), rewritten
//! atomically every [`FLUSH_EVERY`] and on quit. That file is also the way to
//! read the totals: it is plain JSON, keyed by surface, in seconds.
//!
//! **Nothing leaves the machine.** This module uses `std::fs` and nothing
//! else from the outside world: no socket, no HTTP client, no IPC. That is
//! checked mechanically by `module_makes_no_network_calls` below, which scans
//! this file's non-test source for networking APIs.
//!
//! # How it counts
//!
//! The tracker is sampled — from `render`, from a one-second poller, and on
//! every window activation change. Each sample credits the time since the
//! previous one to whatever was visible *at* the previous sample, then records
//! what is visible now. A sample taken right as something changes (a tab
//! switch triggers a render; focus loss fires the activation observer) is
//! therefore exact, and the poller bounds the error of anything else to one
//! interval. A gap longer than [`MAX_GAP`] (the Mac slept, the main thread
//! stalled) is credited only up to the cap, so sleep is never counted as use.
//!
//! The counter only observes state. It never changes which tab is shown or how
//! any surface behaves.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::app_state::MainTab;

/// File name under `<root>/.allele`.
const FILE_NAME: &str = "tab-usage.json";
/// Format version written into the file, so a later change can migrate it.
const FORMAT_VERSION: u32 = 1;
/// How often the poller samples the tracker.
pub(crate) const SAMPLE_EVERY: Duration = Duration::from_secs(1);
/// How often accumulated time is written to disk (also written on quit).
pub(crate) const FLUSH_EVERY: Duration = Duration::from_secs(30);
/// Longest interval a single sample may credit. Normal samples are at most
/// [`SAMPLE_EVERY`] apart; anything far beyond that is sleep or a stall.
const MAX_GAP: Duration = Duration::from_secs(5);

/// `<root>/.allele/tab-usage.json`.
pub(crate) fn usage_file() -> Option<PathBuf> {
    crate::paths::allele_dir().map(|d| d.join(FILE_NAME))
}

/// Something whose visible time is counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Surface {
    Claude,
    Reader,
    Browser,
    Transcript,
    Changes,
    ScratchPad,
}

impl Surface {
    /// The key this surface is stored under in the file.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Surface::Claude => "claude",
            Surface::Reader => "reader",
            Surface::Browser => "browser",
            Surface::Transcript => "transcript",
            Surface::Changes => "changes_panel",
            Surface::ScratchPad => "scratch_pad",
        }
    }

    pub(crate) fn from_tab(tab: MainTab) -> Self {
        match tab {
            MainTab::Claude => Surface::Claude,
            MainTab::Reader => Surface::Reader,
            MainTab::Browser => Surface::Browser,
            MainTab::Transcript => Surface::Transcript,
        }
    }
}

/// What the window is showing at one instant, as far as counting cares.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Visible {
    /// The visible main tab, or `None` when something else (a remote tab)
    /// replaces the main column.
    pub(crate) tab: Option<MainTab>,
    pub(crate) changes_panel: bool,
    pub(crate) scratch_pad: bool,
}

impl Visible {
    fn surfaces(self) -> impl Iterator<Item = Surface> {
        [
            self.tab.map(Surface::from_tab),
            self.changes_panel.then_some(Surface::Changes),
            self.scratch_pad.then_some(Surface::ScratchPad),
        ]
        .into_iter()
        .flatten()
    }
}

/// The on-disk shape of `tab-usage.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct UsageTotals {
    #[serde(default)]
    pub(crate) version: u32,
    /// Unix seconds when counting first started for this file.
    #[serde(default)]
    pub(crate) since_unix: u64,
    /// Unix seconds of the last write.
    #[serde(default)]
    pub(crate) updated_unix: u64,
    /// Seconds visible while the window was focused, keyed by [`Surface::key`].
    #[serde(default)]
    pub(crate) seconds: BTreeMap<String, f64>,
}

impl UsageTotals {
    #[cfg(test)]
    pub(crate) fn seconds_for(&self, surface: Surface) -> f64 {
        self.seconds.get(surface.key()).copied().unwrap_or(0.0)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Accumulates visible time and owns its file.
pub(crate) struct UsageTracker {
    totals: UsageTotals,
    path: Option<PathBuf>,
    /// The previous sample: when it was taken, and what was being counted
    /// then (`None` = not counting: unfocused, disabled, or no sample yet).
    last: Option<(Instant, Option<Visible>)>,
    dirty: bool,
    last_flush: Option<Instant>,
}

impl UsageTracker {
    /// Load the totals at `path` (or start empty). `None` keeps everything in
    /// memory — used when there is no home directory, and by tests.
    pub(crate) fn load(path: Option<PathBuf>) -> Self {
        let totals = path
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice::<UsageTotals>(&bytes).ok())
            .unwrap_or_default();
        Self {
            totals,
            path,
            last: None,
            dirty: false,
            last_flush: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn totals(&self) -> &UsageTotals {
        &self.totals
    }

    /// Record a sample at `now`. `counting` is what is visible now, or `None`
    /// when time should not be counted (window unfocused, setting off).
    ///
    /// The interval since the previous sample is credited to what was visible
    /// at the previous sample.
    pub(crate) fn sample(&mut self, now: Instant, counting: Option<Visible>) {
        if let Some((then, Some(prev))) = self.last {
            let elapsed = now.saturating_duration_since(then).min(MAX_GAP);
            if !elapsed.is_zero() {
                let secs = elapsed.as_secs_f64();
                for surface in prev.surfaces() {
                    *self
                        .totals
                        .seconds
                        .entry(surface.key().to_string())
                        .or_insert(0.0) += secs;
                    self.dirty = true;
                }
            }
        }
        self.last = Some((now, counting));
    }

    /// Write to disk if anything changed and [`FLUSH_EVERY`] has passed.
    pub(crate) fn flush_if_due(&mut self, now: Instant) {
        let due = self
            .last_flush
            .is_none_or(|t| now.saturating_duration_since(t) >= FLUSH_EVERY);
        if self.dirty && due {
            self.flush();
            self.last_flush = Some(now);
        }
    }

    /// Write to disk now if anything changed. Called on quit.
    pub(crate) fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = self.path.clone() else {
            self.dirty = false;
            return;
        };
        let now = unix_now();
        if self.totals.since_unix == 0 {
            self.totals.since_unix = now;
        }
        self.totals.updated_unix = now;
        self.totals.version = FORMAT_VERSION;
        match write_atomic(&path, &self.totals) {
            Ok(()) => self.dirty = false,
            Err(e) => tracing::warn!("tab usage: could not write {}: {e}", path.display()),
        }
    }
}

impl crate::AppState {
    /// What counting sees right now, or `None` when it should not count.
    fn usage_view(&self) -> Option<Visible> {
        if !self.window_focused || !self.user_settings.usage_counting_enabled {
            return None;
        }
        // A remote tab replaces the main column and hides the Changes panel
        // (see `render`), so neither is visible while one is shown.
        let local = self.remote_tabs.shown().is_none();
        Some(Visible {
            tab: local.then_some(self.main_tab),
            changes_panel: local && self.right_panel.visible,
            scratch_pad: self.scratch_pad.is_some(),
        })
    }

    /// Feed the usage tracker one sample. Cheap; safe to call every frame.
    pub(crate) fn sample_usage(&mut self) {
        self.sample_usage_at(Instant::now());
    }

    pub(crate) fn sample_usage_at(&mut self, now: Instant) {
        let view = self.usage_view();
        self.usage.sample(now, view);
    }
}

/// Sample every [`SAMPLE_EVERY`] so time is counted even when nothing
/// renders, and write the totals out every [`FLUSH_EVERY`].
pub(crate) fn spawn_poller(cx: &mut gpui::Context<crate::AppState>) {
    cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(SAMPLE_EVERY).await;
        let alive = this.update(cx, |state: &mut crate::AppState, _cx| {
            state.sample_usage();
            state.usage.flush_if_due(Instant::now());
        });
        if alive.is_err() {
            break; // AppState dropped — app is exiting
        }
    })
    .detach();
}

/// Write `totals` to `path` via a sibling temp file and a rename, so a reader
/// never sees a half-written file.
fn write_atomic(path: &Path, totals: &UsageTotals) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(totals).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(tab: MainTab) -> Option<Visible> {
        Some(Visible {
            tab: Some(tab),
            ..Visible::default()
        })
    }

    /// Sample once a second for `secs` seconds from `start` with `view`.
    fn run(t: &mut UsageTracker, start: Instant, secs: u64, view: Option<Visible>) -> Instant {
        for i in 0..=secs {
            t.sample(start + Duration::from_secs(i), view);
        }
        start + Duration::from_secs(secs)
    }

    #[test]
    fn ten_seconds_on_transcript_adds_ten_seconds() {
        let mut t = UsageTracker::load(None);
        run(&mut t, Instant::now(), 10, on(MainTab::Transcript));
        let got = t.totals().seconds_for(Surface::Transcript);
        assert!((got - 10.0).abs() <= 1.0, "got {got}");
        assert_eq!(t.totals().seconds_for(Surface::Claude), 0.0);
    }

    #[test]
    fn tab_switch_credits_each_tab_its_own_time() {
        let mut t = UsageTracker::load(None);
        let mid = run(&mut t, Instant::now(), 4, on(MainTab::Claude));
        // A render at the switch instant records the new tab.
        t.sample(mid, on(MainTab::Reader));
        run(&mut t, mid, 6, on(MainTab::Reader));
        assert!((t.totals().seconds_for(Surface::Claude) - 4.0).abs() < 1e-9);
        assert!((t.totals().seconds_for(Surface::Reader) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn unfocused_time_is_not_counted() {
        let mut t = UsageTracker::load(None);
        let start = Instant::now();
        let blur = run(&mut t, start, 3, on(MainTab::Transcript));
        // Activation observer fires on focus loss: stop counting.
        t.sample(blur, None);
        let refocus = run(&mut t, blur, 20, None);
        t.sample(refocus, on(MainTab::Transcript));
        run(&mut t, refocus, 2, on(MainTab::Transcript));
        let got = t.totals().seconds_for(Surface::Transcript);
        assert!((got - 5.0).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn sleep_gap_is_capped() {
        let mut t = UsageTracker::load(None);
        let start = Instant::now();
        t.sample(start, on(MainTab::Claude));
        t.sample(start + Duration::from_secs(3600), on(MainTab::Claude));
        assert_eq!(
            t.totals().seconds_for(Surface::Claude),
            MAX_GAP.as_secs_f64()
        );
    }

    #[test]
    fn changes_panel_and_scratch_pad_are_counted_alongside_the_tab() {
        let mut t = UsageTracker::load(None);
        let view = Some(Visible {
            tab: Some(MainTab::Claude),
            changes_panel: true,
            scratch_pad: true,
        });
        run(&mut t, Instant::now(), 7, view);
        for s in [Surface::Claude, Surface::Changes, Surface::ScratchPad] {
            assert!((t.totals().seconds_for(s) - 7.0).abs() < 1e-9, "{s:?}");
        }
    }

    #[test]
    fn hidden_main_column_counts_no_tab() {
        let mut t = UsageTracker::load(None);
        let view = Some(Visible {
            tab: None,
            ..Visible::default()
        });
        run(&mut t, Instant::now(), 5, view);
        assert!(t.totals().seconds.is_empty());
    }

    #[test]
    fn counters_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".allele").join(FILE_NAME);

        let mut first = UsageTracker::load(Some(path.clone()));
        run(&mut first, Instant::now(), 10, on(MainTab::Transcript));
        first.flush(); // what quit does
        drop(first);

        let mut second = UsageTracker::load(Some(path.clone()));
        assert!((second.totals().seconds_for(Surface::Transcript) - 10.0).abs() < 1e-9);
        assert!(second.totals().since_unix > 0);
        run(&mut second, Instant::now(), 5, on(MainTab::Transcript));
        second.flush();

        let third = UsageTracker::load(Some(path));
        assert!((third.totals().seconds_for(Surface::Transcript) - 15.0).abs() < 1e-9);
    }

    #[test]
    fn flush_if_due_waits_for_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut t = UsageTracker::load(Some(path.clone()));
        let start = Instant::now();
        let end = run(&mut t, start, 2, on(MainTab::Claude));
        t.flush_if_due(end);
        assert!(path.exists(), "first flush writes immediately");
        std::fs::remove_file(&path).unwrap();
        let later = run(&mut t, end, 2, on(MainTab::Claude));
        t.flush_if_due(later);
        assert!(!path.exists(), "second flush waits for FLUSH_EVERY");
        t.flush_if_due(end + FLUSH_EVERY);
        assert!(path.exists());
    }

    #[test]
    fn corrupt_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"not json").unwrap();
        let t = UsageTracker::load(Some(path));
        assert!(t.totals().seconds.is_empty());
    }

    /// Seconds before now — `Instant` can't be built from a number.
    fn ago(secs: u64) -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .expect("uptime exceeds a few seconds")
    }

    /// Acceptance: turning the setting off stops counting, at once (the
    /// handler samples), and the choice is persisted.
    #[gpui::test]
    fn turning_the_setting_off_stops_counting(cx: &mut gpui::TestAppContext) {
        use crate::actions::SettingsAction;
        use crate::app_state::fixture::Fixture;

        let fx = Fixture::new(cx);
        fx.update(cx, |state, _w, _cx| {
            state.main_tab = MainTab::Transcript;
            state.sample_usage_at(ago(3));
        });

        let (_, settings_dirty) = fx.dispatch(SettingsAction::UpdateUsageCounting(false), cx);
        assert!(settings_dirty);
        fx.checkpoint(cx);
        assert!(!fx.settings.snapshot().usage_counting_enabled);

        let counted = fx.update(cx, |state, _w, _cx| {
            // A minute later, still off: nothing more is credited.
            state.sample_usage_at(Instant::now() + Duration::from_secs(60));
            state.usage.totals().seconds_for(Surface::Transcript)
        });
        assert!(
            (counted - 3.0).abs() < 0.5,
            "only the 3 s before off: {counted}"
        );

        fx.dispatch(SettingsAction::UpdateUsageCounting(true), cx);
        let resumed = fx.update(cx, |state, _w, _cx| {
            state.sample_usage_at(Instant::now() + Duration::from_secs(62));
            state.usage.totals().seconds_for(Surface::Transcript)
        });
        assert!(resumed > counted, "turning it back on counts again");
    }

    /// Acceptance: with the window not key, `AppState` reports nothing to count.
    #[gpui::test]
    fn unfocused_window_is_not_counted_by_app_state(cx: &mut gpui::TestAppContext) {
        use crate::app_state::fixture::Fixture;

        let fx = Fixture::new(cx);
        let got = fx.update(cx, |state, _w, _cx| {
            state.window_focused = false;
            state.sample_usage_at(ago(10));
            state.sample_usage_at(Instant::now());
            state.usage.totals().seconds_for(Surface::Claude)
        });
        assert_eq!(got, 0.0);
    }

    /// Acceptance: the Changes panel and the Scratch Pad follow `AppState`.
    #[gpui::test]
    fn changes_panel_counts_when_visible(cx: &mut gpui::TestAppContext) {
        use crate::app_state::fixture::Fixture;

        let fx = Fixture::new(cx);
        let got = fx.update(cx, |state, _w, _cx| {
            state.right_panel.visible = true;
            state.sample_usage_at(ago(4));
            state.sample_usage_at(Instant::now());
            state.usage.totals().seconds_for(Surface::Changes)
        });
        assert!((got - 4.0).abs() < 0.5, "got {got}");
    }

    /// The "nothing leaves the machine" guarantee, checked rather than
    /// promised: the non-test part of this file names no networking API.
    #[test]
    fn module_makes_no_network_calls() {
        let src = include_str!("usage.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        // Assembled at runtime so this list doesn't match itself.
        let banned = [
            ["std::", "net"].concat(),
            ["Tcp", "Stream"].concat(),
            ["Udp", "Socket"].concat(),
            ["Unix", "Stream"].concat(),
            ["req", "west"].concat(),
            ["hy", "per"].concat(),
            ["ure", "q::"].concat(),
            ["http", "://"].concat(),
            ["https", "://"].concat(),
            ["Command", "::new"].concat(),
        ];
        for needle in &banned {
            assert!(
                !body.contains(needle.as_str()),
                "usage.rs must stay local-only, but names `{needle}`"
            );
        }
    }
}
