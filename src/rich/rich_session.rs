//! Process management for Rich Mode sessions.
//!
//! Spawns `claude -p --output-format stream-json`, reads NDJSON from stdout,
//! parses into RichEvents, and feeds them through a channel to the GPUI view.

use crate::stream::{RichEvent, StreamParser};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// A Claude Code session running in stream-json mode.
pub struct RichSession {
    /// The spawned CLI process.
    child: Option<Child>,
    /// Channel receiving parsed events from the background reader.
    events_rx: flume::Receiver<RichEvent>,
    /// Session UUID (same as used for PTY mode).
    session_id: String,
    /// Whether the process has exited.
    exited: bool,
}

impl RichSession {
    /// Spawn a new Rich Mode session.
    ///
    /// `prompt` — the user's initial message.
    /// `session_id` — UUID shared with PTY mode for --resume switching.
    /// `working_dir` — the APFS clone path (or project source).
    /// `allowed_tools` — tools to auto-approve (e.g. "Read,Edit,Grep,Glob").
    /// `settings_path` — path to hooks.json for hook configuration.
    pub fn spawn(
        prompt: &str,
        session_id: &str,
        working_dir: &Path,
        allowed_tools: &str,
        settings_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--session-id")
            .arg(session_id)
            .current_dir(working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if !allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(allowed_tools);
        }

        if let Some(settings) = settings_path {
            cmd.arg("--settings").arg(settings);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = flume::bounded(512);

        // Background task: read stdout line-by-line, parse, send events
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut parser = StreamParser::new();

            while let Ok(Some(line)) = lines.next_line().await {
                let events = parser.feed_line(&line);
                for event in events {
                    if tx.send_async(event).await.is_err() {
                        return; // receiver dropped
                    }
                }
            }
        });

        Ok(Self {
            child: Some(child),
            events_rx: rx,
            session_id: session_id.to_string(),
            exited: false,
        })
    }

    /// Resume an existing session in Rich Mode.
    ///
    /// Uses `--resume` instead of `--session-id` to continue from prior
    /// conversation context (works across PTY ↔ Rich mode switching).
    pub fn resume(
        prompt: &str,
        session_id: &str,
        working_dir: &Path,
        allowed_tools: &str,
        settings_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--resume")
            .arg(session_id)
            .current_dir(working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if !allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(allowed_tools);
        }

        if let Some(settings) = settings_path {
            cmd.arg("--settings").arg(settings);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = flume::bounded(512);

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut parser = StreamParser::new();

            while let Ok(Some(line)) = lines.next_line().await {
                let events = parser.feed_line(&line);
                for event in events {
                    if tx.send_async(event).await.is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            child: Some(child),
            events_rx: rx,
            session_id: session_id.to_string(),
            exited: false,
        })
    }

    /// Drain all pending events from the background reader.
    /// Called on each render tick (16ms).
    pub fn drain_events(&mut self) -> Vec<RichEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Check if the process has exited.
    pub fn check_exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    self.exited = true;
                    true
                }
                Ok(None) => false,
                Err(_) => {
                    self.exited = true;
                    true
                }
            }
        } else {
            true
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    /// Kill the process (for mode switching or shutdown).
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            // kill_on_drop handles this, but be explicit
            let _ = child.start_kill();
        }
        self.exited = true;
    }
}

impl Drop for RichSession {
    fn drop(&mut self) {
        self.kill();
    }
}
