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
use crate::session::AttentionContext;
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

        // Populate attention context from the hook payload on Notification.
        // Claude Code's Notification hook only sends {session_id, message} —
        // tool details aren't in the payload. For permission prompts, scrape
        // the terminal buffer to extract the tool name and command summary.
        if matches!(event.kind, HookKind::Notification) {
            let message = event
                .payload
                .as_ref()
                .and_then(|p| p.message.clone());
            let is_permission = message
                .as_deref()
                .map(|m| m.contains("permission"))
                .unwrap_or(false);

            let (tool_name, tool_summary) = if is_permission {
                if let Some(ref tv) = session.terminal_view {
                    let lines = tv.read(cx).read_last_lines(30);
                    scrape_permission_from_buffer(&lines)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

            session.attention_context = Some(AttentionContext {
                tool_name,
                tool_input_summary: tool_summary,
                message,
                title: event.payload.as_ref().and_then(|p| p.title.clone()),
                ts: event.ts,
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

/// Create a brief summary of a tool's input for the attention bar.
fn summarise_attention_input(
    tool_name: Option<&str>,
    tool_input: Option<&serde_json::Value>,
) -> Option<String> {
    let name = tool_name?;
    let input = tool_input?;

    let summary = match name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| if c.len() > 80 { format!("{}…", &c[..77]) } else { c.to_string() }),
        "Read" | "read_file" | "Edit" | "edit_file" | "Write" | "write_file" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| short_path(p)),
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            Some(format!("/{pattern}/"))
        }
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| if s.len() > 60 { format!("{}…", &s[..57]) } else { s.to_string() }),
        _ => input
            .as_object()
            .and_then(|obj| obj.values().next())
            .and_then(|v| v.as_str())
            .map(|s| if s.len() > 60 { format!("{}…", &s[..57]) } else { s.to_string() }),
    };

    summary
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        parts.first().unwrap_or(&"?").to_string()
    }
}

/// Scrape the terminal buffer for Claude Code's permission prompt to extract
/// the tool name and a summary of what it wants to do.
///
/// Claude Code renders permission prompts like:
/// ```text
///   Bash command
///     git add src/main.rs && git commit ...
///   Read file
///     /path/to/file.rs
///   Edit file
///     /path/to/file.rs
///   Write(/path/to/file.rs)
///     content...
/// ```
///
/// Returns `(tool_name, summary)` — both `None` if the pattern isn't found.
fn scrape_permission_from_buffer(lines: &[String]) -> (Option<String>, Option<String>) {
    // Known Claude Code permission prompt headers.
    const TOOL_HEADERS: &[(&str, &str)] = &[
        ("Bash command", "Bash"),
        ("Run shell command", "Bash"),
        ("Read file", "Read"),
        ("Edit file", "Edit"),
        ("Write file", "Write"),
        ("Execute command", "Bash"),
    ];

    // Scan backwards from the bottom for "Do you want to proceed?" to confirm
    // we're looking at a permission prompt, then scan upward for the tool header.
    let mut has_prompt = false;
    let mut prompt_line_idx = 0;
    for (i, line) in lines.iter().enumerate().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with("Do you want to proceed") {
            has_prompt = true;
            prompt_line_idx = i;
            break;
        }
    }

    if !has_prompt {
        return (None, None);
    }

    // Scan upward from the "Do you want to proceed?" line for a tool header.
    for i in (0..prompt_line_idx).rev() {
        let trimmed = lines[i].trim();

        // Check known headers
        for &(header, tool_name) in TOOL_HEADERS {
            if trimmed == header {
                // The content (command, file path, etc.) is on the lines
                // between the header and the prompt. Take the first non-empty
                // indented line as the summary.
                let summary = lines[i + 1..prompt_line_idx]
                    .iter()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty())
                    .map(|s| {
                        if s.len() > 80 {
                            format!("{}…", &s[..77])
                        } else {
                            s.to_string()
                        }
                    });
                return (Some(tool_name.to_string()), summary);
            }
        }

        // Handle "Write(/path/to/file)" pattern
        if trimmed.starts_with("Write(") && trimmed.ends_with(')') {
            let path = &trimmed[6..trimmed.len() - 1];
            return (Some("Write".to_string()), Some(short_path(path)));
        }
    }

    (None, None)
}
