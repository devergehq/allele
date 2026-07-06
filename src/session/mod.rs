use gpui::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::terminal::TerminalView;

/// Cached context from the most recent PreToolUse hook event. PreToolUse
/// fires before the permission Notification, so we stash it here and pull
/// the tool details when the Notification arrives.
#[derive(Debug, Clone)]
pub struct PreToolUseContext {
    pub tool_name: String,
    pub tool_input: Option<serde_json::Value>,
}

/// Rich context about what a session is waiting for when in `AwaitingInput`
/// state. Populated from the hook payload on Notification events, cleared
/// when the session transitions out of AwaitingInput.
#[derive(Debug, Clone)]
pub struct AttentionContext {
    /// The tool Claude wants to run (e.g. "Bash", "Edit", "Read").
    pub tool_name: Option<String>,
    /// Brief summary of the tool input (e.g. "npm install", "src/main.rs").
    pub tool_input_summary: Option<String>,
    /// Notification message text from Claude Code.
    pub message: Option<String>,
}

/// Status of a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Done,
    /// Rehydrated from disk — no PTY attached yet. Click to cold-resume.
    Suspended,
    /// Highest-priority attention state. Claude is blocked on a permission
    /// prompt or a Notification-hook-level wait. User must act to unblock.
    AwaitingInput,
    /// Medium-priority attention state. Claude finished a response turn
    /// (Stop hook). User should review and provide the next prompt.
    ResponseReady,
}

impl SessionStatus {
    /// SVG icon name for this status (assets/icons/svg/). Shapes are
    /// distinct per status so state reads without color vision.
    pub fn icon_name(&self) -> &'static str {
        use crate::icon::name;
        match self {
            SessionStatus::Running => name::CIRCLE_FILL,
            SessionStatus::Idle => name::CIRCLE,
            SessionStatus::Done => name::CHECK,
            SessionStatus::Suspended => name::PAUSE,
            SessionStatus::AwaitingInput => name::ALERT_TRIANGLE,
            SessionStatus::ResponseReady => name::STAR_FILL,
        }
    }

    pub fn color(&self) -> gpui::Hsla {
        let t = crate::theme::theme();
        match self {
            SessionStatus::Running => t.success,
            SessionStatus::Idle => t.warning,
            SessionStatus::Done => t.text_faint,
            SessionStatus::Suspended => t.accent,
            SessionStatus::AwaitingInput => t.attention, // urgent, blocker
            SessionStatus::ResponseReady => t.ready,     // done, review
        }
    }
}

/// One named drawer terminal tab.
pub struct DrawerTab {
    pub view: Entity<TerminalView>,
    pub name: String,
}

/// A single Claude Code session.
///
/// `terminal_view` is `None` for sessions that were rehydrated from
/// `state.json` on startup — those sessions are in `Suspended` status
/// and have no PTY attached until the user explicitly resumes them.
pub struct Session {
    /// Stable UUID — the workspace identity. Names the clone dir
    /// (`~/.allele/workspaces/<proj>/<id[..8]>`), the git branch, and the
    /// `.allele-session` marker. Also the value passed to Claude as
    /// `--session-id` when the session is first created. Persisted to
    /// `state.json`. NEVER mutated after creation — `/clear` rotates the
    /// *Claude* conversation, not the workspace (see `claude_session_id`).
    pub id: String,
    /// The Claude conversation currently backing this workspace. Starts
    /// equal to `id`; a `/clear` (or `/compact`) makes Claude Code rotate to
    /// a fresh session id + transcript `.jsonl`, and we re-point this field
    /// there (learned from the hook payload's cwd — see `apply_hook_event`).
    /// `None` means "same as `id`" — the common case and the back-compat
    /// default for sessions persisted before this field existed. Read via
    /// [`Session::claude_session_id`]; it is what `--resume`, hook-event
    /// matching, and the transcript tailer key off.
    pub claude_session_id: Option<String>,
    pub label: String,
    pub terminal_view: Option<Entity<TerminalView>>,
    pub status: SessionStatus,
    /// Wall-clock time the session was originally started. Serialisable.
    pub started_at: SystemTime,
    /// Updated whenever we observe activity on the session (or on rehydrate).
    pub last_active: SystemTime,
    /// APFS clone path for this session. `None` means the session runs
    /// directly in the project source (fallback mode).
    pub clone_path: Option<PathBuf>,
    /// Per-session drawer terminals (plain shell). Multiple named tabs.
    /// Empty until the drawer is first toggled open.
    pub drawer_tabs: Vec<DrawerTab>,
    /// Index into `drawer_tabs` for the currently shown tab.
    pub drawer_active_tab: usize,
    /// Tab names to lazily spawn when the drawer is first opened — used
    /// when the session is rehydrated from `state.json`. Consumed on open.
    pub pending_drawer_tab_names: Vec<String>,
    /// Whether the bottom drawer is visible for this session. Per-session
    /// so switching sessions preserves each session's drawer state.
    pub drawer_visible: bool,
    /// Set to `true` after a successful merge-and-close. When the session
    /// is subsequently removed, `remove_session` skips creating an archive
    /// entry because the work is already in canonical.
    pub merged: bool,
    /// Set to `true` once `trigger_auto_naming` has been called for this
    /// session, to prevent spawning duplicate naming tasks.
    pub auto_naming_fired: bool,
    /// Port allocated for this session's `{{unique_port}}` substitution.
    /// Re-allocated on every session materialisation (creation + resume).
    /// Not persisted — the value isn't useful across app restarts because
    /// the process holding it is gone.
    pub allocated_port: Option<u16>,
    /// When `Some(deadline)`, the session was recently (re)started via
    /// `resume_session`. If the PTY exits before `deadline`, the session
    /// reverts to `Suspended` rather than flipping to `Done` — this avoids
    /// landing the user in the "Session ended" trap when `claude --resume`
    /// can't find history and exits immediately. Cleared once the deadline
    /// passes without an exit.
    pub resuming_until: Option<Instant>,
    /// Integer id of the Chrome tab linked to this session. Assigned by
    /// Chrome when we `make new tab…` via AppleScript. Stable within a
    /// Chrome process; becomes stale on Chrome restart — reconciled by
    /// recreating the tab on next sync.
    pub browser_tab_id: Option<i64>,
    /// Last URL we saw / set for the linked tab. Used when the stored tab
    /// id is stale so we can recreate the tab at the same URL.
    pub browser_last_url: Option<String>,
    /// Id of the coding agent that spawned this session (matches
    /// `AgentConfig.id` in settings). Resume uses this to re-spawn with
    /// the same adapter regardless of the current global default. `None`
    /// for pre-feature sessions — those fall back to the default.
    pub agent_id: Option<String>,
    /// Pinned sessions sort to the top of their project's session list.
    pub pinned: bool,
    /// Optional user comment displayed as a subtitle on the session row.
    pub comment: Option<String>,
    /// Per-session merge strategy. `None` = use the project's setting.
    pub merge_strategy_override: Option<crate::settings::MergeStrategy>,
    /// Whether the workspace has uncommitted changes. `None` until the
    /// first background poll completes. Display-only; never persisted.
    pub git_dirty: Option<bool>,
    /// The current git branch name for this session (e.g. "fix-auth-5dc47535").
    /// Persisted to state.json for orphan cleanup identification.
    pub branch_name: Option<String>,
    /// Transient: LLM-generated naming suggestions awaiting user selection
    /// (only populated in Interactive naming mode).
    pub naming_suggestions: Option<Vec<String>>,
    /// Cached tool context from the most recent PreToolUse hook event.
    /// PreToolUse always fires before a permission Notification, so this
    /// holds the tool details needed for the attention bar display.
    pub last_pre_tool_use: Option<PreToolUseContext>,
    /// Rich context about what this session is waiting for. Populated from
    /// the hook payload when a Notification fires (AwaitingInput), cleared
    /// when the session transitions to Running or any non-attention state.
    pub attention_context: Option<AttentionContext>,
    /// Transient status line from the allele.json `startup` command.
    /// Updated line-by-line as the script runs, cleared when it finishes.
    pub startup_status: Option<String>,
}

impl Session {
    /// Create a new running session with a caller-supplied UUID.
    ///
    /// The caller's UUID becomes the session's identity *and* is passed
    /// to Claude via `--session-id` so we can later resume with `--resume <id>`.
    pub fn new_with_id(id: String, label: String, terminal_view: Entity<TerminalView>) -> Self {
        let now = SystemTime::now();
        Self {
            id,
            claude_session_id: None,
            label,
            terminal_view: Some(terminal_view),
            status: SessionStatus::Idle,
            started_at: now,
            last_active: now,
            clone_path: None,
            drawer_tabs: Vec::new(),
            drawer_active_tab: 0,
            pending_drawer_tab_names: Vec::new(),
            drawer_visible: false,
            merged: false,
            auto_naming_fired: false,
            allocated_port: None,
            resuming_until: None,
            browser_tab_id: None,
            browser_last_url: None,
            agent_id: None,
            pinned: false,
            comment: None,
            merge_strategy_override: None,
            git_dirty: None,
            branch_name: None,
            naming_suggestions: None,
            last_pre_tool_use: None,
            attention_context: None,
            startup_status: None,
        }
    }

    /// Create a Suspended session from persisted state — no PTY attached.
    ///
    /// Used on startup to rehydrate `state.json`: the session appears in the
    /// sidebar with a ⏸ icon and does not spawn any claude process until the
    /// user clicks it.
    pub fn suspended_from_persisted(
        id: String,
        label: String,
        started_at: SystemTime,
        last_active: SystemTime,
        clone_path: Option<PathBuf>,
        merged: bool,
    ) -> Self {
        Self {
            id,
            claude_session_id: None,
            label,
            terminal_view: None,
            status: SessionStatus::Suspended,
            started_at,
            last_active,
            clone_path,
            drawer_tabs: Vec::new(),
            drawer_active_tab: 0,
            pending_drawer_tab_names: Vec::new(),
            drawer_visible: false,
            merged,
            auto_naming_fired: false,
            allocated_port: None,
            resuming_until: None,
            browser_tab_id: None,
            browser_last_url: None,
            agent_id: None,
            pinned: false,
            comment: None,
            merge_strategy_override: None,
            git_dirty: None,
            branch_name: None,
            naming_suggestions: None,
            last_pre_tool_use: None,
            attention_context: None,
            startup_status: None,
        }
    }

    pub fn with_clone(mut self, clone_path: PathBuf) -> Self {
        self.clone_path = Some(clone_path);
        self
    }

    /// Restore the persisted Claude conversation pointer during rehydration.
    /// A `None` (or an entry equal to `id`) leaves the session pointing at
    /// its stable `id`, which is correct for sessions that never `/clear`ed.
    pub fn with_claude_session_id(mut self, claude_session_id: Option<String>) -> Self {
        self.claude_session_id = claude_session_id.filter(|c| c != &self.id);
        self
    }

    /// The Claude conversation id currently backing this workspace — the
    /// value to pass to `claude --resume`, to match incoming hook events
    /// against, and to tail the transcript for. Falls back to the stable
    /// workspace `id` when no rotation has occurred.
    pub fn claude_session_id(&self) -> &str {
        self.claude_session_id.as_deref().unwrap_or(&self.id)
    }

    pub fn with_agent_id(mut self, agent_id: Option<String>) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Attach persisted browser tab id and last URL during rehydration.
    /// The tab id may be stale (Chrome restart); reconciled on first sync.
    pub fn with_browser(
        mut self,
        tab_id: Option<i64>,
        last_url: Option<String>,
    ) -> Self {
        self.browser_tab_id = tab_id;
        self.browser_last_url = last_url;
        self
    }

    /// Attach pending drawer-tab names + active index restored from disk.
    /// The tabs are spawned lazily when the drawer is first opened.
    pub fn with_drawer_tabs(mut self, names: Vec<String>, active: usize) -> Self {
        if !names.is_empty() {
            self.drawer_active_tab = active.min(names.len().saturating_sub(1));
            self.pending_drawer_tab_names = names;
        }
        self
    }

    /// Format elapsed time since `started_at` as a human-readable string.
    ///
    /// For `Running` and `Idle` sessions the timer is live — wall-clock
    /// since `started_at`. For `Suspended` and `Done` sessions the timer
    /// is frozen at the last observed activity, so a paused or completed
    /// session stops ticking in the sidebar.
    pub fn elapsed_display(&self) -> String {
        let elapsed = match self.status {
            // Frozen — timer stops ticking in the sidebar when the session
            // is not actively running something.
            SessionStatus::Suspended
            | SessionStatus::Done
            | SessionStatus::AwaitingInput
            | SessionStatus::ResponseReady => self
                .last_active
                .duration_since(self.started_at)
                .unwrap_or(Duration::ZERO),
            // Live — still doing work.
            SessionStatus::Running | SessionStatus::Idle => self
                .started_at
                .elapsed()
                .unwrap_or(Duration::ZERO),
        };
        let secs = elapsed.as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

#[cfg(test)]
mod tests {
    // Import Session explicitly rather than `use super::*` — this module's
    // parent does `use gpui::*`, whose glob would shadow the standard
    // `#[test]` attribute with gpui's own `test` macro.
    use super::Session;
    use std::time::SystemTime;

    fn sample(id: &str) -> Session {
        let now = SystemTime::now();
        Session::suspended_from_persisted(id.to_string(), "L".into(), now, now, None, false)
    }

    #[test]
    fn claude_session_id_falls_back_to_workspace_id() {
        let s = sample("workspace-abc");
        // No rotation yet → the Claude conversation is the workspace id itself.
        assert_eq!(s.claude_session_id(), "workspace-abc");
    }

    #[test]
    fn claude_session_id_uses_rotated_pointer() {
        let mut s = sample("workspace-abc");
        s.claude_session_id = Some("rotated-xyz".into());
        assert_eq!(s.claude_session_id(), "rotated-xyz");
        // The stable workspace identity is untouched by the rotation.
        assert_eq!(s.id, "workspace-abc");
    }

    #[test]
    fn with_claude_session_id_drops_redundant_equal_pointer() {
        // A persisted pointer equal to id is normalised away to None so we
        // don't carry a redundant override for never-cleared sessions.
        let s = sample("workspace-abc").with_claude_session_id(Some("workspace-abc".into()));
        assert_eq!(s.claude_session_id, None);
        assert_eq!(s.claude_session_id(), "workspace-abc");
    }

    #[test]
    fn with_claude_session_id_keeps_divergent_pointer() {
        let s = sample("workspace-abc").with_claude_session_id(Some("rotated-xyz".into()));
        assert_eq!(s.claude_session_id.as_deref(), Some("rotated-xyz"));
    }
}
