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
use crate::session::{AttentionContext, PreToolUseContext};
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
    /// - `SessionEnd` → `Idle` (PTY watcher handles actual process exit → Done)
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

        // Cache PreToolUse context — it fires before the permission
        // Notification and carries the tool name + input we need.
        if matches!(event.kind, HookKind::PreToolUse) {
            if let Some(ref payload) = event.payload {
                if let Some(ref name) = payload.tool_name {
                    session.last_pre_tool_use = Some(PreToolUseContext {
                        tool_name: name.clone(),
                        tool_input: payload.tool_input.clone(),
                    });
                }
            }
        }

        // Clear stale PreToolUse cache on PostToolUse — the tool completed
        // (auto-accepted or user-accepted), so any cached data is resolved.
        // Without this, a later Notification wrongly inherits the tool context
        // and renders as a permission prompt instead of a generic waiting state.
        if matches!(event.kind, HookKind::PostToolUse) {
            session.last_pre_tool_use = None;
        }

        // Populate attention context on Notification by pulling tool details
        // from the cached PreToolUse rather than scraping the terminal buffer.
        if matches!(event.kind, HookKind::Notification) {
            let message = event
                .payload
                .as_ref()
                .and_then(|p| p.message.clone());

            let (tool_name, tool_summary) = if let Some(ctx) = session.last_pre_tool_use.take() {
                let summary = summarise_tool_input(&ctx.tool_name, ctx.tool_input.as_ref());
                (Some(ctx.tool_name), summary)
            } else {
                (None, None)
            };

            session.attention_context = Some(AttentionContext {
                tool_name,
                tool_input_summary: tool_summary,
                message,
            });
        }

        // Clear attention context on any transition OUT of AwaitingInput.
        if prior == SessionStatus::AwaitingInput
            && !matches!(event.kind, HookKind::Notification)
        {
            session.attention_context = None;
        }

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
            HookKind::SessionEnd => {
                // /clear and real exits both fire SessionEnd, but only real
                // exits kill the PTY process. The PTY watcher in the render
                // loop catches actual process death via has_exited() — so we
                // never mark Done here. Transition to Idle (context was reset,
                // session is alive and waiting for new input).
                Some(SessionStatus::Idle)
            }
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

        // Capture attention context for the RichView permission block.
        let attention_for_rich = if new_status == SessionStatus::AwaitingInput {
            self.projects
                .get(p_idx)
                .and_then(|p| p.sessions.get(s_idx))
                .and_then(|s| s.attention_context.as_ref())
                .map(|ctx| (ctx.tool_name.clone(), ctx.tool_input_summary.clone()))
        } else {
            None
        };

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

        // Update the RichView permission block if this session is the one
        // currently being displayed in the transcript tab.
        let cursor = crate::actions::SessionCursor { project_idx: p_idx, session_idx: s_idx };
        if self.rich.cursor == Some(cursor) {
            if let Some(view) = self.rich.view.as_ref().cloned() {
                if let Some((tool_name, summary)) = attention_for_rich {
                    view.update(cx, |rv, cx| {
                        rv.push_permission_request(tool_name, summary, cx);
                    });
                } else if prior == SessionStatus::AwaitingInput {
                    view.update(cx, |rv, cx| {
                        rv.clear_permission_request(cx);
                    });
                }
            }
        }

        // Trigger auto-naming after all borrows are released.
        if let Some((session_id, clone_path)) = auto_name_data {
            info!("auto-naming: trigger fired for session {session_id}");
            self.trigger_auto_naming(session_id, clone_path, cx);
        }

        // Keep the changes panel live: tool activity in the displayed
        // session likely touched the working tree. The refresh runs on the
        // background executor and newer generations supersede older ones,
        // so firing per-event is safe.
        if self.right_panel.visible
            && self.active == Some(cursor)
            && matches!(event.kind, HookKind::PostToolUse | HookKind::Stop)
        {
            self.refresh_changes(cx);
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
            // All blocking work (file polling, LLM subprocess, git rename)
            // runs on the background executor so the UI thread stays free.
            let result = cx
                .background_executor()
                .spawn(async move {
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
                        std::thread::sleep(std::time::Duration::from_millis(2000));
                        if attempt == 0 {
                            info!("auto-naming: waiting for prompt file for {session_id}");
                        }
                    }

                    let Some(prompt) = prompt_text else {
                        warn!("auto-naming: no prompt file found after 4min for {session_id}");
                        return None;
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
                    let naming_result =
                        if mode != NamingMode::Legacy && !agent_binary.is_empty() {
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
                    let (branch_name, display_label, suggestions) =
                        if let Some(result) = naming_result {
                            (
                                result.branch_name,
                                result.display_label,
                                Some(result.suggestions),
                            )
                        } else {
                            // Fallback: keyword extraction (legacy mode or LLM failure).
                            let slug_raw = git::extract_slug_from_prompt(&prompt, 4);
                            if slug_raw.is_empty() {
                                warn!("auto-naming: empty slug from keyword extraction");
                                return None;
                            }
                            let slug = git::slugify(&slug_raw, 50);
                            if slug.is_empty() {
                                return None;
                            }
                            let branch = naming::branch_name_from_slug(&slug, &short_id);
                            let label = naming::slug_to_label(&slug);
                            (branch, label, None)
                        };

                    info!(
                        "auto-naming: generated branch_name={branch_name:?} for {session_id}"
                    );

                    // Rename git branch (also blocking I/O).
                    if mode != NamingMode::Interactive || suggestions.is_none() {
                        if let Some(ref cp) = clone_path {
                            if let Err(e) =
                                git::rename_session_branch(cp, &session_id, &branch_name)
                            {
                                warn!("auto-naming: branch rename failed: {e}");
                            }
                        }
                    }

                    Some((session_id, mode, branch_name, display_label, suggestions))
                })
                .await;

            // Back on the foreground — apply results to UI state.
            let Some((session_id, mode, branch_name, display_label, suggestions)) = result
            else {
                return;
            };

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

            // Auto mode (or fallback): update label immediately.
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

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        parts.first().unwrap_or(&"?").to_string()
    }
}

/// Extract a human-readable summary from a tool's input JSON.
/// Returns `None` if no meaningful summary can be derived.
fn summarise_tool_input(tool_name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let obj = input?.as_object()?;

    let raw = match tool_name {
        "Bash" => obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                obj.get("command").and_then(|v| v.as_str()).map(String::from)
            }),
        "Read" => obj.get("file_path").and_then(|v| v.as_str()).map(|p| short_path(p)),
        "Edit" => obj.get("file_path").and_then(|v| v.as_str()).map(|p| short_path(p)),
        "Write" => obj.get("file_path").and_then(|v| v.as_str()).map(|p| short_path(p)),
        _ => {
            // MCP tools and others — try description, then first string field
            obj.get("description")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| {
                    obj.values()
                        .find_map(|v| v.as_str())
                        .map(String::from)
                })
        }
    }?;

    if raw.len() > 80 {
        Some(format!("{}…", &raw[..77]))
    } else {
        Some(raw)
    }
}
