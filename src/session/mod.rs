use gpui::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::terminal::TerminalView;

/// Status of a session
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Done,
    /// Rehydrated from disk — no PTY attached yet. Click to cold-resume.
    Suspended,
}

impl SessionStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            SessionStatus::Running => "●",
            SessionStatus::Idle => "○",
            SessionStatus::Done => "✓",
            SessionStatus::Suspended => "⏸",
        }
    }

    pub fn color(&self) -> u32 {
        match self {
            SessionStatus::Running => 0xa6e3a1,   // green
            SessionStatus::Idle => 0xf9e2af,      // yellow
            SessionStatus::Done => 0x6c7086,      // grey
            SessionStatus::Suspended => 0x89b4fa, // blue
        }
    }
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
    ) -> Self {
        Self {
            id,
            label,
            terminal_view: None,
            status: SessionStatus::Suspended,
            started_at,
            last_active,
            clone_path,
        }
    }

    pub fn with_clone(mut self, clone_path: PathBuf) -> Self {
        self.clone_path = Some(clone_path);
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
            SessionStatus::Suspended | SessionStatus::Done => self
                .last_active
                .duration_since(self.started_at)
                .unwrap_or(Duration::ZERO),
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
