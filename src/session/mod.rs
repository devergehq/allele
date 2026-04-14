use gpui::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::terminal::TerminalView;

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
    pub fn icon(&self) -> &'static str {
        match self {
            SessionStatus::Running => "●",
            SessionStatus::Idle => "○",
            SessionStatus::Done => "✓",
            SessionStatus::Suspended => "⏸",
            SessionStatus::AwaitingInput => "⚠",
            SessionStatus::ResponseReady => "★",
        }
    }

    pub fn color(&self) -> u32 {
        match self {
            SessionStatus::Running => 0xa6e3a1,       // green
            SessionStatus::Idle => 0xf9e2af,          // yellow
            SessionStatus::Done => 0x6c7086,          // grey
            SessionStatus::Suspended => 0x89b4fa,     // blue
            SessionStatus::AwaitingInput => 0xfab387, // peach — urgent, blocker
            SessionStatus::ResponseReady => 0xcba6f7, // lavender — done, review
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
    /// Stable UUID — same value used as Claude's `--session-id` and later
    /// as `--resume <id>`. Persisted to `state.json`.
    pub id: String,
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
            label,
            terminal_view: Some(terminal_view),
            status: SessionStatus::Running,
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
        }
    }

    pub fn with_clone(mut self, clone_path: PathBuf) -> Self {
        self.clone_path = Some(clone_path);
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
