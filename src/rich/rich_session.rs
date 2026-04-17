//! Process management for Rich Mode sessions.
//!
//! Spawns `claude -p --output-format stream-json`, reads NDJSON from stdout
//! on a background thread, parses into RichEvents, and feeds them through a
//! flume channel to the GPUI view.
//!
//! Uses `std::process::Command` (not tokio) because the spawn runs on the
//! GPUI main thread which has no tokio runtime. The background reader is a
//! plain OS thread — same pattern as the PTY reader in alacritty_terminal.

use crate::stream::{RichEvent, StreamParser};
use std::io::BufRead;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// A Claude Code session running in stream-json mode.
pub struct RichSession {
    /// The spawned CLI process.
    child: Option<Child>,
    /// Channel receiving parsed events from the background reader thread.
    events_rx: flume::Receiver<RichEvent>,
    /// Session UUID (same as used for PTY mode).
    session_id: String,
    /// Whether the process has exited.
    exited: bool,
}

impl RichSession {
    /// Spawn a new Rich Mode session.
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
            .stderr(Stdio::piped());

        if !allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(allowed_tools);
        }

        if let Some(settings) = settings_path {
            cmd.arg("--settings").arg(settings);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = flume::bounded(512);

        // Background OS thread: read stdout line-by-line, parse, send events.
        // Plain thread (not tokio) because we spawn from the GPUI main thread
        // which has no async runtime.
        std::thread::Builder::new()
            .name("rich-stream-reader".into())
            .spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                let mut parser = StreamParser::new();

                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break, // pipe closed or IO error
                    };
                    let events = parser.feed_line(&line);
                    for event in events {
                        if tx.send(event).is_err() {
                            return; // receiver dropped
                        }
                    }
                }
            })?;

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
            .stderr(Stdio::piped());

        if !allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(allowed_tools);
        }

        if let Some(settings) = settings_path {
            cmd.arg("--settings").arg(settings);
        }

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = flume::bounded(512);

        std::thread::Builder::new()
            .name("rich-stream-reader".into())
            .spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                let mut parser = StreamParser::new();

                for line in reader.lines() {
                    let line = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    let events = parser.feed_line(&line);
                    for event in events {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                }
            })?;

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
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
        self.child = None;
        self.exited = true;
    }
}

impl Drop for RichSession {
    fn drop(&mut self) {
        self.kill();
    }
}
