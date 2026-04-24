//! AppState event-handler methods that react to parsed hook events
//! delivered by src/hooks/mod.rs (which owns the receiver / watcher).
//!
//! Extracted from src/main.rs per docs/RE-DECOMPOSITION-PLAN.md §5 phase 8.
//! See ARCHITECTURE.md §2 for the distinction between this module and
//! src/hooks/mod.rs.

use std::path::PathBuf;

use gpui::*;
use tracing::{info, warn};

use crate::app_state::AppState;
use crate::{git, hooks, session, settings};

impl AppState {
    /// Apply a single hook event to the matching session.
    ///
    /// Transition rules:
    /// - `Notification` → `AwaitingInput` (permission prompt / idle wait)
    /// - `Stop` → `ResponseReady` (Claude finished a response turn)
    /// - `PreToolUse` / `PostToolUse` → `Running` (Claude is actively executing
    ///   a tool, which means any prior permission prompt has been resolved)
    /// - `UserPromptSubmit` → `Running` (user submitted new input)
    /// - `SessionStart` → `Running`
    /// - `SessionEnd` → `Done`
    ///
    /// Note: `Stop` no longer has special handling for `AwaitingInput`.
    /// In practice Claude doesn't emit `Stop` while still blocked on a
    /// prompt — `Stop` means the response turn completed, which implies
    /// any prompt was resolved. The earlier "don't stomp" rule was
    /// overly defensive and caused stuck AwaitingInput states in the wild.
    pub(crate) fn apply_hook_event(&mut self, event: hooks::HookEvent, cx: &mut Context<Self>) {
        // Find the matching session by its internal ID (= Claude session ID).
        let Some((p_idx, s_idx)) = self.projects.iter().enumerate().find_map(|(p_idx, p)| {
            p.sessions
                .iter()
                .position(|s| s.id == event.session_id)
                .map(|s_idx| (p_idx, s_idx))
        }) else {
            // Event for an unknown session — probably stale, drop it.
            warn!(
                "hook-event: no matching session for {:?} kind={:?}",
                event.session_id, event.kind
            );
            return;
        };

        let Some(session) = self
            .projects
            .get_mut(p_idx)
            .and_then(|p| p.sessions.get_mut(s_idx))
        else {
            return;
        };

        let prior = session.status;
        let now = std::time::SystemTime::now();
        session.last_active = now;

        use hooks::HookKind;
        use session::SessionStatus;

        // --- Auto-naming: trigger on any event while label is a placeholder ---
        // Fires on the first hook event (usually SessionStart) to start polling
        // for the .prompt file. If that attempt times out (user hadn't typed yet),
        // a retry fires on UserPromptSubmit when the .prompt file is guaranteed
        // to exist.
        let is_placeholder = session.label.starts_with("Claude ")
            || session.label.starts_with("Shell ");
        let auto_name_data = if is_placeholder {
            if !session.auto_naming_fired {
                // First attempt — start polling for the prompt file.
                session.auto_naming_fired = true;
                info!(
                    "auto-naming: triggered for {} label={:?} on {:?}",
                    session.id, session.label, event.kind
                );
                Some((session.id.clone(), session.clone_path.clone()))
            } else if matches!(event.kind, HookKind::UserPromptSubmit) {
                // Retry — first attempt likely timed out before user typed.
                // The .prompt file is guaranteed to exist now.
                info!(
                    "auto-naming: retrying for {} on UserPromptSubmit (label still {:?})",
                    session.id, session.label
                );
                Some((session.id.clone(), session.clone_path.clone()))
            } else {
                None
            }
        } else {
            None
        };

        let new_status = match event.kind {
            HookKind::Notification => Some(SessionStatus::AwaitingInput),
            HookKind::Stop => Some(SessionStatus::ResponseReady),
            HookKind::PreToolUse | HookKind::PostToolUse => {
                // Tool execution is the key clearing signal. If Claude is
                // running a tool, any prior permission prompt has been
                // resolved and we should be back in Running. If we were
                // already Running, this is a no-op (the prior==new guard
                // below drops it).
                Some(SessionStatus::Running)
            }
            HookKind::UserPromptSubmit => Some(SessionStatus::Running),
            HookKind::SessionStart => Some(SessionStatus::Idle),
            HookKind::SessionEnd => Some(SessionStatus::Done),
            HookKind::Other => None,
        };

        let Some(new_status) = new_status else {
            // No status change, but still trigger auto-naming if applicable.
            if let Some((session_id, clone_path)) = auto_name_data {
                info!("auto-naming: trigger fired for session {session_id}");
                self.trigger_auto_naming(session_id, clone_path, cx);
            }
            return;
        };
        if new_status == prior {
            // No status transition, but still trigger auto-naming if applicable.
            if let Some((session_id, clone_path)) = auto_name_data {
                info!("auto-naming: trigger fired for session {session_id}");
                self.trigger_auto_naming(session_id, clone_path, cx);
            }
            return;
        }

        session.status = new_status;

        // Capture the label for notifications BEFORE we drop the borrow.
        let session_label = session.label.clone();
        let project_name = self
            .projects
            .get(p_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        // Persist the updated status.
        self.save_state();

        // Fire sound + notification affordances — ONLY on transitions INTO
        // an attention state, never on transitions out of one.
        match new_status {
            SessionStatus::AwaitingInput => {
                if self.user_settings.sound_on_awaiting_input {
                    let sound_path = self
                        .user_settings
                        .awaiting_input_sound_path
                        .clone()
                        .unwrap_or_else(|| settings::DEFAULT_AWAITING_INPUT_SOUND.to_string());
                    hooks::play_sound(&sound_path);
                }
                if self.user_settings.notifications_enabled {
                    hooks::show_notification(
                        &format!("{project_name} — needs input"),
                        &format!("{session_label} is blocked and waiting for you"),
                    );
                }
            }
            SessionStatus::ResponseReady => {
                if self.user_settings.sound_on_response_ready {
                    let sound_path = self
                        .user_settings
                        .response_ready_sound_path
                        .clone()
                        .unwrap_or_else(|| settings::DEFAULT_RESPONSE_READY_SOUND.to_string());
                    hooks::play_sound(&sound_path);
                }
                if self.user_settings.notifications_enabled {
                    hooks::show_notification(
                        &format!("{project_name} — response ready"),
                        &format!("{session_label} finished responding"),
                    );
                }
            }
            _ => {}
        }

        cx.notify();

        // Trigger auto-naming after all borrows are released.
        if let Some((session_id, clone_path)) = auto_name_data {
            info!("auto-naming: trigger fired for session {session_id}");
            self.trigger_auto_naming(session_id, clone_path, cx);
        }
    }

    /// Spawn a background task that reads the first prompt from the hook
    /// events directory, extracts keywords to produce a 3-5 word slug, then
    /// updates the session label and renames the git branch.
    /// No external dependencies — pure Rust keyword extraction.
    pub(crate) fn trigger_auto_naming(
        &self,
        session_id: String,
        clone_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(events_dir) = hooks::events_dir() else { return; };

        cx.spawn(async move |this, cx| {
            // Read the .prompt file (written by the hook receiver on the first
            // UserPromptSubmit). Since auto-naming fires on the first hook event
            // (often session_start, before the user types), we poll generously:
            // 120 attempts × 2s = 4 minutes. The extraction itself is instant
            // so there's no cost to waiting longer.
            let prompt_path = events_dir.join(format!("{session_id}.prompt"));
            let mut prompt_text = None;
            for attempt in 0..120 {
                if let Ok(text) = std::fs::read_to_string(&prompt_path) {
                    if !text.trim().is_empty() {
                        prompt_text = Some(text);
                        break;
                    }
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(2000))
                    .await;
                if attempt == 0 {
                    info!("auto-naming: waiting for prompt file for {session_id}");
                }
            }

            let Some(prompt) = prompt_text else {
                warn!("auto-naming: no prompt file found after 4min for {session_id}");
                return;
            };
            info!(
                "auto-naming: prompt file read for {session_id} ({} chars)",
                prompt.len()
            );

            // Extract keywords — pure Rust, no LLM needed.
            let slug_raw = git::extract_slug_from_prompt(&prompt, 4);

            info!("auto-naming: extracted slug_raw={slug_raw:?} for {session_id}");
            if slug_raw.is_empty() {
                warn!("auto-naming: empty slug from keyword extraction");
                return;
            }

            let slug = git::slugify(&slug_raw, 50);
            if slug.is_empty() {
                return;
            }

            // Human-readable label: replace hyphens with spaces, title case,
            // capped at 40 chars for sidebar display.
            let full_label: String = slug
                .split('-')
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(c) => {
                            let upper: String = c.to_uppercase().collect();
                            format!("{upper}{}", chars.as_str())
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let display_label = if full_label.len() > 40 {
                let mut truncated = full_label[..40].to_string();
                // Avoid cutting mid-word — trim back to last space.
                if let Some(last_space) = truncated.rfind(' ') {
                    truncated.truncate(last_space);
                }
                truncated
            } else {
                full_label
            };

            // Rename the git branch in the background (non-blocking).
            if let Some(ref cp) = clone_path {
                info!("auto-naming: renaming branch for {session_id} with slug={slug:?}");
                if let Err(e) = git::rename_session_branch(cp, &session_id, &slug) {
                    warn!("auto-naming: branch rename failed: {e}");
                    // Continue — label update is still valuable
                } else {
                    info!("auto-naming: branch rename succeeded for {session_id}");
                }
            }

            // Update session label on the main thread.
            info!("auto-naming: updating label to {display_label:?} for {session_id}");
            let _ = this.update(cx, |this: &mut AppState, cx| {
                for project in &mut this.projects {
                    for session in &mut project.sessions {
                        if session.id == session_id {
                            info!(
                                "auto-naming: label updated {:?} -> {:?} for {session_id}",
                                session.label, display_label
                            );
                            session.label = display_label.clone();
                            break;
                        }
                    }
                }
                this.save_state();
                cx.notify();
            });
        })
        .detach();
    }
}
