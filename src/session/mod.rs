use gpui::*;
use crate::terminal::TerminalView;
use std::path::PathBuf;
use std::time::Instant;
use uuid::Uuid;

/// Status of a session
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionStatus {
    Running,
    Idle,
    Done,
}

impl SessionStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            SessionStatus::Running => "●",
            SessionStatus::Idle => "○",
            SessionStatus::Done => "✓",
        }
    }

    pub fn color(&self) -> u32 {
        match self {
            SessionStatus::Running => 0xa6e3a1, // green
            SessionStatus::Idle => 0xf9e2af,    // yellow
            SessionStatus::Done => 0x6c7086,    // grey
        }
    }
}

/// A single Claude Code session
pub struct Session {
    pub id: String,
    pub label: String,
    pub terminal_view: Entity<TerminalView>,
    pub status: SessionStatus,
    pub started_at: Instant,
    /// APFS clone path for this session (None for standalone sessions).
    /// When the session is closed, this clone is deleted from the filesystem.
    pub clone_path: Option<PathBuf>,
}

impl Session {
    pub fn new(label: String, terminal_view: Entity<TerminalView>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            label,
            terminal_view,
            status: SessionStatus::Running,
            started_at: Instant::now(),
            clone_path: None,
        }
    }

    pub fn with_clone(mut self, clone_path: PathBuf) -> Self {
        self.clone_path = Some(clone_path);
        self
    }

    /// Format elapsed time as a human-readable string
    pub fn elapsed_display(&self) -> String {
        let elapsed = self.started_at.elapsed();
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
