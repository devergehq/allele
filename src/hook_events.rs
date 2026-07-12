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
    /// Re-associate a hook event whose Claude session id matches no known
    /// session back to the workspace it fired in, by matching the event's
    /// `cwd` against each session's working directory (its clone dir, or the
    /// project source for fallback sessions). On a *unique* match this adopts
    /// the new id as the session's `claude_session_id` — the workspace `id`
    /// stays put — and returns the session's cursor so the caller processes
    /// the event normally.
    ///
    /// Guards against mis-attachment: it only re-keys when exactly one live
    /// session's cwd matches. Zero or multiple matches → `None` (drop).
    fn rekey_session_by_cwd(
        &mut self,
        event: &hooks::HookEvent,
        cx: &mut Context<Self>,
    ) -> Option<crate::actions::SessionCursor> {
        // No cwd (jq unavailable on the receiver) → nothing to anchor on.
        let raw_cwd = event.cwd.as_ref()?;
        let target = std::fs::canonicalize(raw_cwd)
            .unwrap_or_else(|_| std::path::PathBuf::from(raw_cwd));

        // Resolve each session's working directory and compare to the event's.
        let same_dir = |dir: &std::path::Path| -> bool {
            let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            canon == target
        };

        let mut matches = self.projects.iter().enumerate().flat_map(|(p_idx, p)| {
            p.sessions.iter().enumerate().filter_map(move |(s_idx, s)| {
                let cwd = s
                    .clone_path
                    .clone()
                    .unwrap_or_else(|| p.source_path.clone());
                if same_dir(&cwd) {
                    Some((p_idx, s_idx))
                } else {
                    None
                }
            })
        });

        let (p_idx, s_idx) = matches.next()?;
        if matches.next().is_some() {
            // Ambiguous — more than one session lives in this cwd. Don't guess.
            warn!(
                "hook-event: cwd {:?} matches multiple sessions; not re-keying {:?}",
                raw_cwd, event.session_id
            );
            return None;
        }

        let session = self.projects.get_mut(p_idx)?.sessions.get_mut(s_idx)?;
        let old_id = session.claude_session_id().to_string();
        tracing::info!(
            "hook-event: re-keying session {} claude id {} -> {} (cwd {:?}, kind {:?})",
            session.id, old_id, event.session_id, raw_cwd, event.kind
        );
        session.claude_session_id = Some(event.session_id.clone());
        self.mark_state_dirty();

        // If the re-keyed session is the one on screen, drop the cached rich
        // cursor so `ensure_rich_view` rebuilds the transcript tailer against
        // the new conversation's `.jsonl` on the next render.
        let cursor = crate::actions::SessionCursor { project_idx: p_idx, session_idx: s_idx };
        if self.rich.cursor == Some(cursor) {
            self.rich.cursor = None;
        }
        cx.notify();

        Some(cursor)
    }

    /// Apply a single agent event to the matching session.
    ///
    /// Interpretation is delegated to the session's [`AgentAdapter`]: the raw
    /// native `kind` string is translated into a canonical
    /// [`agents::Lifecycle`] (plus a cache directive), and this method maps
    /// that onto [`SessionStatus`]. The mapping is agent-agnostic — Claude,
    /// opencode, and any future agent share this code path and differ only in
    /// their adapter's `interpret_event`.
    ///
    /// Canonical transitions:
    /// - `Start`         → `Idle`          (session started / context reset)
    /// - `Busy`          → `Running`       (tool ran / user submitted / streaming)
    /// - `AwaitingInput` → `AwaitingInput` (blocked on a permission / idle wait)
    /// - `TurnComplete`  → `ResponseReady` (finished a response turn)
    /// - `End`           → `Idle`          (PTY watcher handles real exit → Done)
    pub(crate) fn apply_hook_event(&mut self, event: hooks::HookEvent, cx: &mut Context<Self>) {
        // Find the matching session by the Claude conversation id currently
        // backing it. This is `id` for sessions that never `/clear`ed, or the
        // rotated id for those that did.
        let matched = self.projects.iter().enumerate().find_map(|(p_idx, p)| {
            p.sessions
                .iter()
                .position(|s| s.claude_session_id() == event.session_id)
                .map(|s_idx| (p_idx, s_idx))
        });

        let (p_idx, s_idx) = match matched {
            Some(cursor) => cursor,
            None => {
                // No session owns this id. The likeliest cause is a `/clear`
                // (or `/compact`): Claude Code rotated to a fresh session id +
                // transcript while staying in the same workspace. Re-associate
                // by cwd — the workspace directory is stable across a rotation
                // — and adopt the new id so status, resume, and the transcript
                // all follow the live conversation. Falls through to a drop if
                // no unique workspace matches (a genuinely stale/foreign event).
                match self.rekey_session_by_cwd(&event, cx) {
                    Some(cursor) => (cursor.project_idx, cursor.session_idx),
                    None => {
                        warn!(
                            "hook-event: no matching session for {:?} kind={:?} cwd={:?}",
                            event.session_id, event.kind, event.cwd
                        );
                        return;
                    }
                }
            }
        };

        // Resolve the adapter for this session's agent BEFORE taking a mutable
        // borrow of the session. Sessions with no recorded agent (legacy)
        // default to Claude, preserving prior behaviour.
        let agent_kind = self
            .projects
            .get(p_idx)
            .and_then(|p| p.sessions.get(s_idx))
            .and_then(|s| s.agent_id.as_ref())
            .and_then(|aid| self.user_settings.agents.iter().find(|a| &a.id == aid))
            .map(|a| a.kind)
            .unwrap_or(AgentKind::Claude);
        let adapter = crate::agents::adapter_for(agent_kind);
        let signal = adapter.interpret_event(&event.kind);

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

        use crate::agents::{CacheOp, Lifecycle};
        use session::SessionStatus;

        // --- Auto-naming: trigger on any event while label is a placeholder ---
        // Fires on the first event (usually session start) to start polling
        // for the .prompt file. If that attempt times out (user hadn't typed
        // yet), a retry fires on the user-prompt-submit event when the .prompt
        // file is guaranteed to exist. Placeholder prefixes cover every
        // built-in agent's default label plus the bare shell.
        let is_placeholder = session.label.starts_with("Claude ")
            || session.label.starts_with("opencode ")
            || session.label.starts_with("Shell ");
        // Fire exactly once per session. `auto_naming_fired` guards against
        // spawning a second task while the first is still polling for the
        // prompt file. It is reset back to `false` only if that task times out
        // waiting for the prompt (see `trigger_auto_naming`), which lets a
        // later event retry — without ever running two naming tasks at once.
        // (An unconditional retry on `user_prompt_submit` used to live here and
        // caused a duplicate naming task — and a second modal with different
        // suggestions — on every session.)
        //
        // `branch_locked` rides along so the naming task knows whether the user
        // pinned the branch — it still suggests a label but must not rename.
        let auto_name_data = if is_placeholder && !session.auto_naming_fired {
            session.auto_naming_fired = true;
            info!(
                "auto-naming: triggered for {} label={:?} on {:?}",
                session.id, session.label, event.kind
            );
            Some((session.id.clone(), session.clone_path.clone(), session.branch_locked))
        } else {
            None
        };

        // Apply the adapter's tool-context cache directive. Claude caches on
        // PreToolUse (to enrich a later Notification that lacks the tool name)
        // and clears on PostToolUse; opencode carries the tool inline so it
        // always leaves the cache untouched.
        match signal.cache_op {
            CacheOp::Set => {
                if let Some(ref payload) = event.payload {
                    if let Some(ref name) = payload.tool_name {
                        session.last_pre_tool_use = Some(PreToolUseContext {
                            tool_name: name.clone(),
                            tool_input: payload.tool_input.clone(),
                        });
                    }
                }
            }
            CacheOp::Clear => {
                session.last_pre_tool_use = None;
            }
            CacheOp::Leave => {}
        }

        // Populate attention context whenever we enter AwaitingInput. Prefer
        // tool details carried inline on the event (opencode); otherwise fall
        // back to the cached PreToolUse context (Claude, whose Notification
        // hook doesn't carry the tool name itself).
        if signal.lifecycle == Lifecycle::AwaitingInput {
            let message = event.payload.as_ref().and_then(|p| p.message.clone());
            let inline_tool = event.payload.as_ref().and_then(|p| p.tool_name.clone());

            let (tool_name, tool_summary) = if let Some(name) = inline_tool {
                let summary = summarise_tool_input(
                    &name,
                    event.payload.as_ref().and_then(|p| p.tool_input.as_ref()),
                );
                (Some(name), summary)
            } else if let Some(ctx) = session.last_pre_tool_use.take() {
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

        // Clear attention context on any real transition OUT of AwaitingInput
        // (an Ignore event isn't a transition, and re-entering AwaitingInput
        // keeps the freshly-set context above).
        if prior == SessionStatus::AwaitingInput
            && !matches!(signal.lifecycle, Lifecycle::AwaitingInput | Lifecycle::Ignore)
        {
            session.attention_context = None;
        }

        let new_status = match signal.lifecycle {
            Lifecycle::Start => Some(SessionStatus::Idle),
            Lifecycle::Busy => Some(SessionStatus::Running),
            Lifecycle::AwaitingInput => Some(SessionStatus::AwaitingInput),
            Lifecycle::TurnComplete => Some(SessionStatus::ResponseReady),
            // Real process exits are caught by the PTY watcher (→ Done); an
            // End signal (e.g. /clear) just means the context was reset and
            // the session is alive and waiting for new input.
            Lifecycle::End => Some(SessionStatus::Idle),
            Lifecycle::Ignore => None,
        };

        let Some(new_status) = new_status else {
            // No status change, but still trigger auto-naming if applicable.
            if let Some((session_id, clone_path, branch_locked)) = auto_name_data {
                info!("auto-naming: trigger fired for session {session_id}");
                self.trigger_auto_naming(session_id, clone_path, branch_locked, cx);
            }
            return;
        };
        if new_status == prior {
            // No status transition, but still trigger auto-naming if applicable.
            if let Some((session_id, clone_path, branch_locked)) = auto_name_data {
                info!("auto-naming: trigger fired for session {session_id}");
                self.trigger_auto_naming(session_id, clone_path, branch_locked, cx);
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
        if let Some((session_id, clone_path, branch_locked)) = auto_name_data {
            info!("auto-naming: trigger fired for session {session_id}");
            self.trigger_auto_naming(session_id, clone_path, branch_locked, cx);
        }

        // Keep the changes panel live: tool activity in the displayed
        // session likely touched the working tree. The refresh runs on the
        // background executor and newer generations supersede older ones,
        // so firing per-event is safe.
        // Refresh when a tool completed (Claude PostToolUse) or the turn
        // finished (any agent's TurnComplete → ResponseReady) — both imply the
        // working tree may have changed. `new_status` is set by this point.
        if self.right_panel.visible
            && self.active == Some(cursor)
            && (event.kind == "post_tool_use" || new_status == SessionStatus::ResponseReady)
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
        branch_locked: bool,
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
                        return NamingOutcome::TimedOut { session_id };
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
                                return NamingOutcome::Skip;
                            }
                            let slug = git::slugify(&slug_raw, 50);
                            if slug.is_empty() {
                                return NamingOutcome::Skip;
                            }
                            let branch = naming::branch_name_from_slug(&slug, &short_id);
                            let label = naming::slug_to_label(&slug);
                            (branch, label, None)
                        };

                    info!(
                        "auto-naming: generated branch_name={branch_name:?} for {session_id}"
                    );

                    // Rename git branch (also blocking I/O). Skipped when the
                    // user pinned a specific branch — only the label is suggested.
                    if !branch_locked && (mode != NamingMode::Interactive || suggestions.is_none()) {
                        if let Some(ref cp) = clone_path {
                            if let Err(e) =
                                git::rename_session_branch(cp, &session_id, &branch_name)
                            {
                                warn!("auto-naming: branch rename failed: {e}");
                            }
                        }
                    }

                    NamingOutcome::Named {
                        session_id,
                        mode,
                        branch_name,
                        display_label,
                        suggestions,
                    }
                })
                .await;

            // Back on the foreground — apply results to UI state.
            let (session_id, mode, branch_name, display_label, suggestions) = match result {
                NamingOutcome::Named {
                    session_id,
                    mode,
                    branch_name,
                    display_label,
                    suggestions,
                } => (session_id, mode, branch_name, display_label, suggestions),
                NamingOutcome::TimedOut { session_id } => {
                    // No task is in flight now — reset the guard so a later
                    // event can retry naming for this session.
                    let _ = this.update(cx, |this: &mut AppState, _cx| {
                        for project in &mut this.projects {
                            for session in &mut project.sessions {
                                if session.id == session_id {
                                    session.auto_naming_fired = false;
                                    break;
                                }
                            }
                        }
                    });
                    return;
                }
                NamingOutcome::Skip => return,
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
                            // Keep the real branch when the user pinned one —
                            // only adopt the generated branch name if we renamed.
                            if !branch_locked {
                                session.branch_name = Some(branch_name.clone());
                            }
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

/// Outcome of the background auto-naming task, interpreted on the foreground.
enum NamingOutcome {
    /// The prompt file never appeared within the poll window. No naming task is
    /// in flight anymore, so `auto_naming_fired` is reset to allow a retry.
    TimedOut { session_id: String },
    /// Naming completed and produced a result to apply.
    Named {
        session_id: String,
        mode: NamingMode,
        branch_name: String,
        display_label: String,
        suggestions: Option<Vec<String>>,
    },
    /// Naming produced nothing usable (genuine failure); retrying won't help.
    Skip,
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
        Some(format!("{}…", crate::rich::truncate_to_char_boundary(&raw, 77)))
    } else {
        Some(raw)
    }
}
