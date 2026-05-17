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
use crate::naming::{self, NamingMode, NamingRequest};
use crate::settings::AgentKind;
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
        self.mark_state_dirty();

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

    /// Spawn a background task that reads the first prompt, generates a branch
    /// name (via LLM or keyword extraction), renames the branch, and updates
    /// the session label.
    pub(crate) fn trigger_auto_naming(
        &self,
        session_id: String,
        clone_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(events_dir) = hooks::events_dir() else { return; };

        // Snapshot the naming config and resolve the agent kind + binary for this session.
        let naming_config = self.user_settings.naming.clone();
        let agent_kind = self
            .projects
            .iter()
            .flat_map(|p| &p.sessions)
            .find(|s| s.id == session_id)
            .and_then(|s| s.agent_id.as_ref())
            .and_then(|aid| self.user_settings.agents.iter().find(|a| &a.id == aid))
            .map(|a| a.kind)
            .unwrap_or(AgentKind::Claude);
        let agent_binary = crate::agents::detect_path(agent_kind)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        cx.spawn(async move |this, cx| {
            // Poll for the .prompt file (written by the hook receiver on first
            // UserPromptSubmit). 120 attempts × 2s = 4 minutes.
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

            let short_id: String = session_id.chars().take(8).collect();
            let mode = naming_config.mode;

            let suggestions_count = match mode {
                NamingMode::Interactive => 3,
                _ => 1,
            };

            // Attempt LLM naming (Auto or Interactive modes).
            let naming_result = if mode != NamingMode::Legacy && !agent_binary.is_empty() {
                let request = NamingRequest {
                    prompt_text: &prompt,
                    agent_kind,
                    agent_binary: &agent_binary,
                    short_id: &short_id,
                    suggestions_count,
                };
                match naming::generate(&naming_config, &request) {
                    Ok(result) => Some(result),
                    Err(reason) => {
                        warn!("auto-naming: LLM naming failed ({reason}), falling back to keyword extraction");
                        None
                    }
                }
            } else {
                None
            };

            // Determine final branch name and label.
            let (branch_name, display_label, suggestions) = if let Some(result) = naming_result {
                (result.branch_name, result.display_label, Some(result.suggestions))
            } else {
                // Fallback: keyword extraction (legacy mode or LLM failure).
                let slug_raw = git::extract_slug_from_prompt(&prompt, 4);
                if slug_raw.is_empty() {
                    warn!("auto-naming: empty slug from keyword extraction");
                    return;
                }
                let slug = git::slugify(&slug_raw, 50);
                if slug.is_empty() {
                    return;
                }
                let branch = naming::branch_name_from_slug(&slug, &short_id);
                let label = naming::slug_to_label(&slug);
                (branch, label, None)
            };

            info!("auto-naming: generated branch_name={branch_name:?} for {session_id}");

            // In Interactive mode, hand off to the main thread to show the
            // selection modal. The branch rename happens after user picks.
            if mode == NamingMode::Interactive {
                if let Some(suggestions) = suggestions {
                    let _ = this.update(cx, |this: &mut AppState, cx| {
                        for project in &mut this.projects {
                            for session in &mut project.sessions {
                                if session.id == session_id {
                                    session.naming_suggestions = Some(suggestions.clone());
                                    break;
                                }
                            }
                        }
                        cx.notify();
                    });
                    return;
                }
            }

            // Auto mode (or fallback): rename branch and update label immediately.
            if let Some(ref cp) = clone_path {
                if let Err(e) = git::rename_session_branch(cp, &session_id, &branch_name) {
                    warn!("auto-naming: branch rename failed: {e}");
                }
            }

            let _ = this.update(cx, |this: &mut AppState, cx| {
                for project in &mut this.projects {
                    for session in &mut project.sessions {
                        if session.id == session_id {
                            info!(
                                "auto-naming: label updated {:?} -> {:?} for {session_id}",
                                session.label, display_label
                            );
                            session.label = display_label.clone();
                            session.branch_name = Some(branch_name.clone());
                            break;
                        }
                    }
                }
                this.mark_state_dirty();
                cx.notify();
            });
        })
        .detach();
    }
}
