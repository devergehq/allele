//! Long-lived polling tasks started once at launch (DEV-624).
//!
//! Extracted verbatim from `fn main()`, which had grown to 1,088 lines. These
//! four tasks run for the life of the app and share one shape: a timer, a read
//! of something outside the process, and a fold of the result into `AppState`
//! on the foreground thread.
//!
//! Every filesystem read here belongs on the background executor — the
//! foreground scans these replaced were what froze the UI during a dispatch
//! (DEV-602, DEV-609).

use gpui::Context;

use crate::app_state::AppState;
use crate::transcript::session_is_resumable;
use crate::{debug_capture, git, hooks, interrupted};

/// Start every background poller. Called once, from `main`.
pub(crate) fn spawn_all(cx: &mut Context<AppState>) {
    // Spawn the hook-event polling task. Runs for the life
    // of the app, reads ~/.allele/events/*.jsonl every
    // 250ms, and routes each new event into apply_hook_event.
    //
    // Fast-forward existing files so we don't flood the user
    // with pre-existing events from a previous app session.
    cx.spawn(async move |this, cx| {
        let mut watcher = hooks::EventWatcher::new();
        let mut interrupts = interrupted::InterruptWatcher::default();
        watcher.initialize_offsets();

        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;

            // Claude Code emits no hook when a turn is
            // interrupted, so this is the only way a session
            // leaves `Running` after one. See DEV-432.
            interrupted::poll_once(&mut interrupts, &this, cx).await;

            // Which ids may own a file in the events dir. Read
            // on the foreground because it walks AppState, but
            // it touches no disk — the pruning it feeds does,
            // and that happens below, off-thread.
            let Ok(live) = this.read_with(cx, |this, _cx| this.live_event_ids()) else {
                break; // AppState dropped — app is exiting
            };

            // Scanning the events directory is filesystem work
            // — a read_dir plus an open and fstat per file,
            // four times a second, over a directory nothing
            // ever pruned (830 files / 133MB when this was
            // found). Only folding the events into AppState
            // needs the foreground, so the scan moves off it,
            // mirroring `interrupted::poll_once` above, and
            // takes the prune with it. See DEV-602.
            let (returned, events) = cx
                .background_executor()
                .spawn({
                    let mut w = std::mem::take(&mut watcher);
                    async move {
                        let events = w.poll();
                        w.maybe_prune(&live);
                        (w, events)
                    }
                })
                .await;
            watcher = returned;

            if events.is_empty() {
                continue;
            }

            if this
                .update(cx, |this: &mut AppState, cx| {
                    for event in events {
                        this.apply_hook_event(event, cx);
                    }
                })
                .is_err()
            {
                break; // AppState dropped — app is exiting
            }
        }
    })
    .detach();

    // Git workspace-status poller (DEV-9). Every 15s, run a
    // porcelain status per session clone on the background
    // executor and update the sidebar dirty indicators.
    cx.spawn(async move |this, cx| {
        // The first pass runs immediately. `Session::resumable`
        // starts as `None`, and a rehydrated session's Resume
        // affordance should not be missing for the first 15
        // seconds after launch (DEV-602).
        let mut interval = std::time::Duration::ZERO;
        loop {
            cx.background_executor().timer(interval).await;
            interval = std::time::Duration::from_secs(15);

            // Collect clone paths, not (p_idx, s_idx). The
            // status runs across an await during which the
            // user can reorder or remove sessions, which would
            // land a result on the wrong session's row.
            let Ok((targets, resumable_targets)) = this.update(cx, |this: &mut AppState, cx| {
                // Idle-drawer parking rides this tick rather than
                // adding a loop of its own: the threshold is
                // minutes, so 15s is ample resolution, and one
                // timer is one thing to reason about (DEV-445).
                this.reap_idle_drawers(cx);

                let mut t = Vec::new();
                for project in this.projects.iter() {
                    for session in project.sessions.iter() {
                        if let Some(cp) = &session.clone_path {
                            t.push(cp.clone());
                        }
                    }
                }
                (t, this.resumable_targets())
            }) else {
                break; // AppState dropped — app is exiting
            };
            if targets.is_empty() && resumable_targets.is_empty() {
                continue;
            }

            // Resumability rides this tick for the same reason
            // idle-drawer parking does. Both are filesystem
            // work — a porcelain status per clone, and per
            // session a stat plus a scan of ~/.claude/projects
            // — so they share one timer and one background hop
            // rather than each growing a loop of their own.
            let (results, resumable) = cx
                .background_executor()
                .spawn(async move {
                    let results = targets
                        .into_iter()
                        .map(|cp| {
                            let count = git::working_tree_change_count(&cp);
                            (cp, count)
                        })
                        .collect::<Vec<_>>();
                    let resumable = resumable_targets
                        .into_iter()
                        .map(|(id, clone_path)| {
                            let ok = session_is_resumable(clone_path.as_deref(), &id);
                            (id, ok)
                        })
                        .collect::<Vec<_>>();
                    (results, resumable)
                })
                .await;

            if this
                .update(cx, |this: &mut AppState, cx| {
                    let mut changed = false;
                    for (repo, count) in results {
                        let current = this
                            .projects
                            .iter()
                            .flat_map(|p| p.sessions.iter())
                            .find(|s| s.clone_path.as_deref() == Some(&*repo))
                            .map(|s| s.git_dirty_count);
                        if current != Some(count) {
                            this.record_workspace_change_count(&repo, count);
                            changed = true;
                        }
                    }
                    for (id, ok) in resumable {
                        if this.record_resumable(&id, ok) {
                            changed = true;
                        }
                    }
                    if changed {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();

    // Agent-facing capture requests arrive through a tiny file
    // protocol so shell agents need no Accessibility permission.
    cx.spawn(async move |this, cx| loop {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(250))
            .await;
        if !debug_capture::take_request() {
            continue;
        }
        if this
            .update(cx, |state: &mut AppState, cx| {
                state.capture_ui_requested = true;
                cx.notify();
            })
            .is_err()
        {
            break;
        }
    })
    .detach();

    // Rich Sidecar transcript tailer poll. Runs on a much
    // gentler cadence than the old 120ms spinner timer so
    // it doesn't drive full AppState re-renders that trigger
    // spurious terminal resizes and destroy scrollback.
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;

            if this
                .update(cx, |state: &mut AppState, cx| {
                    // Drain the transcript tailer (if built) and
                    // feed events into the RichView. Runs
                    // regardless of which main tab is visible so
                    // the document stays current when the user
                    // flips to Transcript. No cx.notify() here —
                    // the RichView entity updates itself.
                    state.poll_transcript_tailer(cx);
                })
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}
