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

    /// Whether the session's runtime timer should be accruing in this status.
    ///
    /// The timer runs whenever the session is "on" — i.e. has (or had) a live
    /// PTY attached — regardless of whether the agent is actively producing
    /// output. Only `Suspended` (paused, PTY killed) and `Done` (ended, PTY
    /// exited) freeze the clock.
    pub fn counts_toward_runtime(&self) -> bool {
        !matches!(self, SessionStatus::Suspended | SessionStatus::Done)
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
    /// When `true`, the user deliberately checked out (or named) a specific
    /// branch at session creation. Auto-naming still suggests a session label,
    /// but the git branch is never renamed to match it — the chosen branch
    /// stays put until the user explicitly changes it. Persisted so a rehydrated
    /// session with a placeholder label can't re-fire the rename after restart.
    pub branch_locked: bool,
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
    /// Total active runtime banked from all prior "on" stretches — the sum of
    /// wall-clock time the session spent in a runtime-counting status (see
    /// [`SessionStatus::counts_toward_runtime`]). Persisted. Paused and idle
    /// spans are never added here, so this is true active runtime, not age.
    pub active_accumulated: Duration,
    /// When `Some(t)`, the session is currently in a runtime-counting status
    /// and the live stretch began at `t`. `None` while frozen (Suspended/Done).
    /// Transient — rehydrated sessions come back Suspended, so this resets to
    /// `None` across restarts and only `active_accumulated` survives.
    pub active_since: Option<SystemTime>,
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
            // Idle is a runtime-counting status, so the clock starts now.
            active_accumulated: Duration::ZERO,
            active_since: Some(now),
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
            branch_locked: false,
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
        active_accumulated: Duration,
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
            // Suspended is frozen, so the clock is not running; only the
            // banked runtime restored from disk carries over.
            active_accumulated,
            active_since: None,
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
            branch_locked: false,
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

    /// Attach the branch name backing this session's clone. Used when
    /// restoring an archived session onto a freshly checked-out branch.
    pub fn with_branch_name(mut self, branch_name: Option<String>) -> Self {
        self.branch_name = branch_name;
        self
    }

    /// Attach persisted browser tab id and last URL during rehydration.
    /// The tab id may be stale (Chrome restart); reconciled on first sync.
    pub fn with_browser(mut self, tab_id: Option<i64>, last_url: Option<String>) -> Self {
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

    /// Change the session's status, keeping the active-runtime accounting in
    /// sync. This is the ONLY sanctioned way to mutate `status` — routing all
    /// transitions through here is what guarantees the timer counts active
    /// runtime and not paused/idle wall-clock.
    ///
    /// On a live→frozen transition it banks the in-flight stretch into
    /// `active_accumulated`; on a frozen→live transition it starts a new
    /// stretch. Transitions that stay on the same side of the boundary (e.g.
    /// Running↔AwaitingInput) leave the running clock untouched.
    pub fn set_status(&mut self, new: SessionStatus) {
        let was_counting = self.status.counts_toward_runtime();
        let now_counting = new.counts_toward_runtime();
        if was_counting && !now_counting {
            // Leaving a live state — bank whatever the current stretch accrued.
            if let Some(since) = self.active_since.take() {
                self.active_accumulated += since.elapsed().unwrap_or(Duration::ZERO);
            }
        } else if !was_counting && now_counting {
            // Entering a live state — start a fresh stretch.
            self.active_since = Some(SystemTime::now());
        }
        self.status = new;
    }

    /// Total active runtime as a `Duration`: the banked accumulator plus the
    /// in-flight stretch if the session is currently in a counting status.
    /// Paused and completed sessions report exactly their banked runtime.
    pub fn active_runtime(&self) -> Duration {
        let mut total = self.active_accumulated;
        if let Some(since) = self.active_since {
            total += since.elapsed().unwrap_or(Duration::ZERO);
        }
        total
    }

    /// Format active runtime as a human-readable string.
    ///
    /// The value is accumulated active runtime (see [`Session::active_runtime`]),
    /// so a paused session's display stops advancing and a resumed session
    /// continues from where it left off — never the session's wall-clock age.
    /// Days are surfaced once the total reaches 24h so long-lived sessions read
    /// as e.g. "3d 5h" rather than "77h".
    pub fn elapsed_display(&self) -> String {
        let secs = self.active_runtime().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else if secs < 86_400 {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        } else {
            format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
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
        Session::suspended_from_persisted(
            id.to_string(),
            "L".into(),
            now,
            now,
            std::time::Duration::ZERO,
            None,
            false,
        )
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

    // --- Active-runtime timer -------------------------------------------

    #[test]
    fn counts_toward_runtime_only_excludes_paused_and_done() {
        use super::SessionStatus::*;
        for s in [Running, Idle, AwaitingInput, ResponseReady] {
            assert!(s.counts_toward_runtime(), "{s:?} should count");
        }
        for s in [Suspended, Done] {
            assert!(!s.counts_toward_runtime(), "{s:?} should be frozen");
        }
    }

    #[test]
    fn paused_session_reports_only_banked_runtime() {
        use std::time::Duration;
        // Frozen session: active_since is None, so runtime == accumulator and
        // does not advance with wall-clock time.
        let mut s = sample("s");
        s.active_accumulated = Duration::from_secs(5 * 3600);
        assert_eq!(s.active_runtime(), Duration::from_secs(5 * 3600));
        assert_eq!(s.elapsed_display(), "5h 0m");
    }

    #[test]
    fn set_status_banks_in_flight_stretch_on_pause() {
        use super::SessionStatus;
        use std::time::Duration;
        let mut s = sample("s");
        // Simulate a live session that has been running for ~10s.
        s.set_status(SessionStatus::Running);
        s.active_since = Some(SystemTime::now() - Duration::from_secs(10));
        s.set_status(SessionStatus::Suspended);
        // Stretch is banked and the clock is stopped.
        assert!(s.active_since.is_none());
        assert!(s.active_accumulated >= Duration::from_secs(10));
    }

    #[test]
    fn set_status_resumes_clock_from_frozen() {
        use super::SessionStatus;
        use std::time::Duration;
        let mut s = sample("s");
        s.active_accumulated = Duration::from_secs(120);
        assert!(s.active_since.is_none());
        s.set_status(SessionStatus::Running);
        // Clock restarts; prior banked runtime is preserved.
        assert!(s.active_since.is_some());
        assert_eq!(s.active_accumulated, Duration::from_secs(120));
    }

    #[test]
    fn transition_within_live_states_does_not_rebank() {
        use super::SessionStatus;
        use std::time::Duration;
        let mut s = sample("s");
        s.set_status(SessionStatus::Running);
        let since = s.active_since;
        s.set_status(SessionStatus::AwaitingInput);
        // Both are counting states — the running stretch is untouched.
        assert_eq!(s.active_since, since);
        assert_eq!(s.active_accumulated, Duration::ZERO);
    }

    #[test]
    fn elapsed_display_surfaces_days_past_24h() {
        use std::time::Duration;
        let mut s = sample("s");
        s.active_accumulated = Duration::from_secs(3 * 86_400 + 5 * 3600 + 42 * 60);
        assert_eq!(s.elapsed_display(), "3d 5h");
    }

    #[test]
    fn elapsed_display_sub_day_formats() {
        use std::time::Duration;
        let mut s = sample("s");
        s.active_accumulated = Duration::from_secs(45);
        assert_eq!(s.elapsed_display(), "45s");
        s.active_accumulated = Duration::from_secs(3 * 60 + 7);
        assert_eq!(s.elapsed_display(), "3m 7s");
        s.active_accumulated = Duration::from_secs(2 * 3600 + 13 * 60);
        assert_eq!(s.elapsed_display(), "2h 13m");
    }
}
