//! The catch-up strip (DEV-1353): what happened while you were away, between
//! the tabs and the terminal.
//!
//! When you switch back into a session you have been away from for ten
//! minutes or more, a strip opens above the terminal with three parts: your
//! last prompt and one line of the reply, what changed since you looked, and
//! what the session wants now. After a shorter absence it stays as you left
//! it — by default folded to one line. It never covers the terminal: it sits
//! in the column above it, so the terminal loses rows instead.
//!
//! The first key you type goes to the terminal and folds the strip; Esc folds
//! it without typing anything. The session header's Catch-up button opens it
//! again.
//!
//! ## Last viewed
//!
//! Allele did not record when you last looked at a session (DEV-76 never
//! landed). It does now: [`ViewedMark`] on each [`Session`] holds the time you
//! left it and a snapshot of its working tree at that moment, both persisted
//! in `state.json`. While a session is on screen the time is kept current, so
//! a quit-and-relaunch reads as an absence from the quit.
//!
//! ## Layout
//!
//! - `facts.rs`: the plain data the parts are handed. Serialisable, no handles.
//! - `parts.rs`: the three parts, drawn from facts only.
//! - `gather.rs`: builds the facts on this Mac from the transcript, git and
//!   the live session.
//! - this file: when the strip opens and folds, and the strip's frame.

pub mod facts;
pub mod gather;
pub mod parts;

use std::collections::HashMap;
use std::rc::Rc;
use std::time::SystemTime;

use gpui::*;

use crate::app_state::{AppState, MainTab};
use crate::icon::{icon, name as icons};
use crate::session::{Session, SessionStatus};
use crate::theme::theme;
use facts::{CatchUpFacts, ChangeFacts, PromptFacts, WantsNow};

/// When you last looked at a session, and what its work tree was then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewedMark {
    /// The last moment the session was on screen.
    pub at: Option<SystemTime>,
    /// The working tree, as a git tree id, when you left it. The change
    /// summary is measured from here.
    pub tree: Option<String>,
}

/// Per-session strip state. In memory only: a relaunch decides afresh from
/// how long you were away.
#[derive(Debug, Default)]
struct SessionStrip {
    open: bool,
    /// How long you had been away when you last switched in.
    away_secs: Option<u64>,
    prompt: Option<PromptFacts>,
    changes: Option<ChangeFacts>,
    /// Bumped per refresh so an older result never overwrites a newer one.
    generation: u64,
    loading: bool,
    refresh_queued: bool,
}

/// The strip's state across sessions, keyed by session id.
#[derive(Default)]
pub(crate) struct CatchUpState {
    /// The session that was active at the last render, to see switches.
    observed: Option<String>,
    sessions: HashMap<String, SessionStrip>,
    /// A once-a-second repaint is running for a working session's clock.
    ticking: bool,
}

impl AppState {
    fn session_by_id_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.projects
            .iter_mut()
            .flat_map(|p| p.sessions.iter_mut())
            .find(|s| s.id == id)
    }

    /// Notice a switch between sessions and act on it. Called every render;
    /// cheap when nothing changed.
    ///
    /// `self.active` is assigned from many call sites, so the switch is seen
    /// here, by comparison, rather than hooked at each of them — the same
    /// approach as `ensure_active_changes_observed`.
    pub(crate) fn track_catch_up(&mut self, cx: &mut Context<Self>) {
        let now = SystemTime::now();
        let active_id = self.active_session().map(|s| s.id.clone());
        if self.catch_up.observed == active_id {
            // Still looking at it: keep its last-viewed time current. Not
            // marked dirty on its own; the next save carries it.
            if let Some(cursor) = self.active {
                if let Some(s) = self
                    .projects
                    .get_mut(cursor.project_idx)
                    .and_then(|p| p.sessions.get_mut(cursor.session_idx))
                {
                    s.viewed.at = Some(now);
                }
            }
            return;
        }

        if let Some(prev) = self.catch_up.observed.take() {
            self.leave_session(&prev, now, cx);
        }
        self.catch_up.observed = active_id.clone();
        if let Some(id) = active_id {
            self.arrive_at_session(&id, now, cx);
        }
    }

    /// You left `id`: stamp the time and snapshot its tree in the background.
    fn leave_session(&mut self, id: &str, now: SystemTime, cx: &mut Context<Self>) {
        let Some(session) = self.session_by_id_mut(id) else {
            return;
        };
        session.viewed.at = Some(now);
        let clone = session.clone_path.clone();
        self.mark_state_dirty();
        let Some(dir) = clone else {
            return;
        };
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let tree = cx
                .background_executor()
                .spawn(async move { crate::git::snapshot::snapshot_worktree(&dir) })
                .await;
            let _ = this.update(cx, |this: &mut Self, _cx| {
                // Only if you have not come back in the meantime; otherwise
                // the arrival already measured against the older baseline and
                // this one would hide changes made while it ran.
                if this.catch_up.observed.as_deref() == Some(id.as_str()) {
                    return;
                }
                if let (Some(tree), Some(s)) = (tree, this.session_by_id_mut(&id)) {
                    s.viewed.tree = Some(tree);
                    this.mark_state_dirty();
                }
            });
        })
        .detach();
    }

    /// You switched to `id`: decide open or folded, then gather the facts.
    fn arrive_at_session(&mut self, id: &str, now: SystemTime, cx: &mut Context<Self>) {
        let away = self
            .session_by_id_mut(id)
            .and_then(|s| s.viewed.at)
            .and_then(|at| now.duration_since(at).ok())
            .map(|d| d.as_secs());
        let strip = self.catch_up.sessions.entry(id.to_string()).or_default();
        strip.away_secs = away;
        if facts::opens_on_arrival(away) {
            strip.open = true;
            // A long absence starts the "since you looked" count again.
            strip.changes = None;
            strip.prompt = None;
        }
        if let Some(s) = self.session_by_id_mut(id) {
            s.viewed.at = Some(now);
        }
        self.refresh_catch_up(id.to_string(), cx);
    }

    /// Refresh the active session's prompt line and change summary. Called
    /// on hook events so the strip keeps up while the session works.
    pub(crate) fn refresh_active_catch_up(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.catch_up.observed.clone() {
            self.refresh_catch_up(id, cx);
        }
    }

    fn refresh_catch_up(&mut self, id: String, cx: &mut Context<Self>) {
        let Some((transcript, clone, baseline)) = self
            .projects
            .iter()
            .find_map(|p| p.sessions.iter().find(|s| s.id == id).map(|s| (p, s)))
            .map(|(p, s)| {
                let cwd = s
                    .clone_path
                    .clone()
                    .unwrap_or_else(|| p.source_path.clone());
                (
                    crate::transcript::expected_session_jsonl(&cwd, s.claude_session_id()),
                    s.clone_path.clone(),
                    s.viewed.tree.clone(),
                )
            })
        else {
            return;
        };
        let strip = self.catch_up.sessions.entry(id.clone()).or_default();
        if strip.loading {
            strip.refresh_queued = true;
            return;
        }
        strip.loading = true;
        strip.generation += 1;
        let generation = strip.generation;

        cx.spawn(async move |this, cx| {
            let (prompt, changes) = cx
                .background_executor()
                .spawn(async move {
                    let prompt = transcript.as_deref().and_then(gather::read_last_exchange);
                    let changes = clone
                        .as_deref()
                        .and_then(|dir| gather::compute_changes(dir, baseline.as_deref()).0);
                    (prompt, changes)
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                let Some(strip) = this.catch_up.sessions.get_mut(&id) else {
                    return;
                };
                if strip.generation != generation {
                    return;
                }
                strip.loading = false;
                strip.prompt = prompt;
                strip.changes = changes;
                let again = std::mem::take(&mut strip.refresh_queued);
                cx.notify();
                if again {
                    this.refresh_catch_up(id, cx);
                }
            });
        })
        .detach();
    }

    /// The facts for the active session, if it has anything to catch up on.
    fn active_catch_up_facts(&self) -> Option<CatchUpFacts> {
        let session = self.active_session()?;
        let strip = self.catch_up.sessions.get(&session.id)?;
        let facts = CatchUpFacts {
            away_secs: strip.away_secs,
            prompt: strip.prompt.clone(),
            changes: strip.changes.clone(),
            wants: gather::wants_from_session(session),
        };
        // A session you have never prompted has nothing to catch up on.
        let something = facts.prompt.is_some()
            || facts.changes.as_ref().is_some_and(|c| !c.is_empty())
            || !matches!(facts.wants, WantsNow::Nothing { .. });
        something.then_some(facts)
    }

    /// Whether the strip would draw for the active session, and is open.
    pub(crate) fn catch_up_status(&self) -> Option<bool> {
        if self.main_tab != MainTab::Claude {
            return None;
        }
        self.active_catch_up_facts()?;
        let id = &self.active_session()?.id;
        Some(self.catch_up.sessions.get(id).is_some_and(|s| s.open))
    }

    /// Fold the strip to one line (first key, Esc, the Fold button).
    pub(crate) fn fold_catch_up(&mut self, cx: &mut Context<Self>) {
        self.set_catch_up_open(false, cx);
    }

    /// Open the strip (Catch-up in the header, Open on the folded line).
    pub(crate) fn open_catch_up(&mut self, cx: &mut Context<Self>) {
        self.set_catch_up_open(true, cx);
    }

    fn set_catch_up_open(&mut self, open: bool, cx: &mut Context<Self>) {
        let Some(id) = self.active_session().map(|s| s.id.clone()) else {
            return;
        };
        let strip = self.catch_up.sessions.entry(id).or_default();
        if strip.open != open {
            strip.open = open;
            cx.notify();
        }
    }

    /// Keep a working session's "3m 40s so far" moving while it is shown.
    fn ensure_catch_up_ticking(&mut self, cx: &mut Context<Self>) {
        if self.catch_up.ticking {
            return;
        }
        self.catch_up.ticking = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            let keep = this
                .update(cx, |this: &mut Self, cx| {
                    let working = this.catch_up_status() == Some(true)
                        && this
                            .active_session()
                            .is_some_and(|s| s.status == SessionStatus::Running);
                    if working {
                        cx.notify();
                    } else {
                        this.catch_up.ticking = false;
                    }
                    working
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        })
        .detach();
    }

    /// The strip between the tab strip and the terminal, or `None` when the
    /// active session has nothing to catch up on.
    pub(crate) fn render_catch_up_strip(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.catch_up_status()?;
        let facts = self.active_catch_up_facts()?;
        let now = gather::epoch_secs(SystemTime::now());
        if open && matches!(facts.wants, WantsNow::Working { .. }) {
            self.ensure_catch_up_ticking(cx);
        }
        let t = theme();

        let (state_icon, state_color) = match facts.wants {
            WantsNow::Nothing { .. } => (icons::STAR_FILL, t.ready),
            WantsNow::Working { .. } => (icons::LOADER, t.info),
            WantsNow::Permission { .. } | WantsNow::TerminalOnly { .. } => {
                (icons::LOCK, t.attention)
            }
        };

        let toggle = div()
            .id("catch-up-toggle")
            .cursor_pointer()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .flex_shrink_0()
            .px(px(8.0))
            .py(px(2.0))
            .rounded(px(4.0))
            .bg(t.bg_hover)
            .hover(|s| s.bg(t.bg_raised))
            .text_size(px(11.0))
            .text_color(t.text_secondary)
            .child(icon(
                if open {
                    icons::CHEVRON_UP
                } else {
                    icons::CHEVRON_DOWN
                },
                10.0,
                t.text_secondary,
            ))
            .child(if open { "Fold" } else { "Open" })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut Self, _e, _w, cx| {
                    cx.stop_propagation();
                    this.set_catch_up_open(!open, cx);
                }),
            );

        let mut line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.0))
            .child(icon(state_icon, 11.0, state_color))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(state_color)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(facts.wants.state_label()),
            );
        if let Some(away) = facts.away_secs.filter(|s| *s >= 60) {
            line = line.child(
                div()
                    .flex_shrink_0()
                    .text_color(t.text_faint)
                    .child(format!(
                        "· away {}",
                        facts::duration_short(away - away % 60)
                    )),
            );
        }
        line = line
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_color(t.text_secondary)
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(facts.summary_line()),
            )
            .child(toggle);

        let mut strip = div()
            .id("catch-up-strip")
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(t.border_subtle)
            .bg(t.bg_surface)
            .child(line);

        if open {
            let weak = cx.entity().downgrade();
            let w1 = weak.clone();
            let w2 = weak.clone();
            let actions = parts::WantsActions {
                allow_once: Rc::new(move |_w, cx| {
                    let _ = w1.update(cx, |this, cx| this.answer_active_permission(true, cx));
                }),
                deny: Rc::new(move |_w, cx| {
                    let _ = w2.update(cx, |this, cx| this.answer_active_permission(false, cx));
                }),
                open_terminal: Rc::new(move |window, cx| {
                    let _ = weak.update(cx, |this, cx| this.open_active_terminal(window, cx));
                }),
            };

            let mut left = div().flex().flex_col().gap(px(8.0)).flex_1().min_w(px(0.0));
            if let Some(p) = &facts.prompt {
                left = left.child(parts::prompt_line(p, now));
            }
            let working = matches!(facts.wants, WantsNow::Working { .. });
            if let Some(summary) = facts
                .changes
                .as_ref()
                .and_then(|c| parts::change_summary(c, working))
            {
                left = left.child(summary);
            }
            strip = strip.child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(12.0))
                    .pb(px(4.0))
                    .child(left)
                    .child(div().flex_1().min_w(px(0.0)).child(parts::wants_now_row(
                        &facts.wants,
                        now,
                        Some(&actions),
                    ))),
            );
        }
        Some(strip.into_any_element())
    }

    /// "Open the terminal" / "Answer in terminal": fold the strip and put the
    /// keyboard in the terminal, where Claude's menu is.
    fn open_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.main_tab = MainTab::Claude;
        self.fold_catch_up(cx);
        if let Some(tv) = self.active_session().and_then(|s| s.terminal_view.clone()) {
            let handle = tv.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // Named imports, not `super::*`: the glob would bring in `gpui::test`,
    // which shadows `#[test]` inside `#[gpui::test]`'s expansion and recurses.
    use super::ViewedMark;
    use crate::actions::SessionCursor;
    use crate::app_state::fixture::Fixture;
    use crate::session::Session;
    use gpui::TestAppContext;
    use std::time::Duration;
    use std::time::SystemTime;

    fn fixture(viewed_ago: [Option<Duration>; 2], cx: &mut TestAppContext) -> Fixture {
        let mut project =
            crate::project::Project::new("alpha".into(), std::path::PathBuf::from("/tmp/alpha"));
        let now = SystemTime::now();
        for (i, ago) in viewed_ago.into_iter().enumerate() {
            let mut s = Session::suspended_from_persisted(
                format!("s-{i}"),
                format!("s-{i}"),
                now,
                now,
                Duration::ZERO,
                None,
            );
            s.viewed.at = ago.map(|d| now - d);
            project.sessions.push(s);
        }
        Fixture::with_projects(vec![project], cx)
    }

    fn switch_to(fx: &Fixture, idx: usize, cx: &mut TestAppContext) {
        fx.update(cx, |state, _w, cx| {
            state.active = Some(SessionCursor {
                project_idx: 0,
                session_idx: idx,
            });
            state.track_catch_up(cx);
        });
    }

    fn is_open(fx: &Fixture, id: &str, cx: &mut TestAppContext) -> bool {
        fx.update(cx, |state, _w, _cx| {
            state.catch_up.sessions.get(id).is_some_and(|s| s.open)
        })
    }

    #[gpui::test]
    fn ten_minutes_away_opens_the_strip_on_arrival(cx: &mut TestAppContext) {
        let fx = fixture([None, Some(Duration::from_secs(11 * 60))], cx);
        switch_to(&fx, 0, cx);
        switch_to(&fx, 1, cx);
        assert!(is_open(&fx, "s-1", cx));
        let away = fx.update(cx, |state, _w, _cx| {
            state.catch_up.sessions["s-1"].away_secs
        });
        assert!(away.is_some_and(|s| s >= 11 * 60));
    }

    #[gpui::test]
    fn a_short_absence_leaves_it_folded(cx: &mut TestAppContext) {
        let fx = fixture([None, Some(Duration::from_secs(2 * 60))], cx);
        switch_to(&fx, 0, cx);
        switch_to(&fx, 1, cx);
        assert!(!is_open(&fx, "s-1", cx));
    }

    #[gpui::test]
    fn a_session_never_viewed_stays_folded(cx: &mut TestAppContext) {
        let fx = fixture([None, None], cx);
        switch_to(&fx, 0, cx);
        switch_to(&fx, 1, cx);
        assert!(!is_open(&fx, "s-1", cx));
    }

    #[gpui::test]
    fn leaving_stamps_when_you_last_looked(cx: &mut TestAppContext) {
        let fx = fixture([None, None], cx);
        switch_to(&fx, 0, cx);
        let before = SystemTime::now();
        switch_to(&fx, 1, cx);
        let at = fx.update(cx, |state, _w, _cx| state.projects[0].sessions[0].viewed.at);
        assert!(at.is_some_and(|t| t >= before));
    }

    #[gpui::test]
    fn fold_state_survives_a_short_trip_away(cx: &mut TestAppContext) {
        let fx = fixture([None, Some(Duration::from_secs(3600))], cx);
        switch_to(&fx, 0, cx);
        switch_to(&fx, 1, cx);
        assert!(is_open(&fx, "s-1", cx));

        // Left open: still open after popping out and straight back.
        switch_to(&fx, 0, cx);
        switch_to(&fx, 1, cx);
        assert!(is_open(&fx, "s-1", cx));

        // Folded: still folded after the same trip.
        fx.update(cx, |state, _w, cx| state.fold_catch_up(cx));
        switch_to(&fx, 0, cx);
        switch_to(&fx, 1, cx);
        assert!(!is_open(&fx, "s-1", cx));
    }

    #[gpui::test]
    fn catch_up_reopens_a_folded_strip(cx: &mut TestAppContext) {
        let fx = fixture([None, Some(Duration::from_secs(3600))], cx);
        switch_to(&fx, 1, cx);
        fx.update(cx, |state, _w, cx| state.fold_catch_up(cx));
        assert!(!is_open(&fx, "s-1", cx));
        fx.update(cx, |state, _w, cx| state.open_catch_up(cx));
        assert!(is_open(&fx, "s-1", cx));
    }

    #[test]
    fn last_viewed_is_persisted() {
        let now = SystemTime::now();
        let mut s = Session::suspended_from_persisted(
            "a".into(),
            "a".into(),
            now,
            now,
            Duration::ZERO,
            None,
        );
        s.viewed = ViewedMark {
            at: Some(now),
            tree: Some("abc".into()),
        };
        let p = crate::state::PersistedSession::from_session(&s, "proj");
        let json = serde_json::to_string(&p).expect("ser");
        let back: crate::state::PersistedSession = serde_json::from_str(&json).expect("de");
        assert_eq!(back.last_viewed_at, Some(now));
        assert_eq!(back.viewed_tree.as_deref(), Some("abc"));
    }

    #[test]
    fn a_state_file_without_last_viewed_still_loads() {
        let now = SystemTime::now();
        let s = Session::suspended_from_persisted(
            "a".into(),
            "a".into(),
            now,
            now,
            Duration::ZERO,
            None,
        );
        let p = crate::state::PersistedSession::from_session(&s, "proj");
        let json = serde_json::to_string(&p).expect("ser");
        assert!(!json.contains("last_viewed_at"));
        let back: crate::state::PersistedSession = serde_json::from_str(&json).expect("de");
        assert_eq!(back.last_viewed_at, None);
    }
}
