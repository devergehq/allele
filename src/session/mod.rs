use gpui::*;
use crate::terminal::TerminalView;
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
}

impl Session {
    pub fn new(label: String, terminal_view: Entity<TerminalView>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            label,
            terminal_view,
            status: SessionStatus::Running,
        }
    }
}
