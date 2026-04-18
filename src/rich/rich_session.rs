//! Process management for Rich Mode sessions.
//!
//! Spawns `claude -p --input-format stream-json --output-format stream-json`
//! ONCE per session and keeps it alive across turns. Each turn is a single
//! NDJSON line written to the child's stdin; Claude responds on stdout.
//! This eliminates the ~1–2 s per-turn cold start the older --resume-per-turn
//! model paid.
//!
//! The stdin NDJSON format (verified by spike):
//!   {"type":"user","message":{"role":"user","content":"<prompt text>"}}
//!
//! Uses `std::process::Command` (not tokio) because the spawn runs on the
//! GPUI main thread which has no tokio runtime. The background reader is a
//! plain OS thread — same pattern as the PTY reader in alacritty_terminal.

use crate::stream::{RichEvent, StreamParser};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

/// A Claude Code session running in stream-json mode. One process serves
/// many turns — `send_prompt` writes a new user turn to stdin, which Claude
/// responds to on stdout without needing a fresh process.
pub struct RichSession {
    child: Option<Child>,
    /// Writer side of the child's stdin. `None` once stdin has been closed
    /// (on kill) — subsequent `send_prompt` calls error out.
    stdin: Option<ChildStdin>,
    events_rx: flume::Receiver<RichEvent>,
    session_id: String,
    exited: bool,
    /// True while a prompt has been sent but no SessionResult event has
    /// arrived yet. Consumed by the UI to drive the busy indicator.
    in_progress: bool,
}

impl RichSession {
    /// Spawn a fresh Rich Mode session. The first user turn is NOT passed on
    /// the command line — callers must follow up with `send_prompt`.
    pub fn spawn(
        session_id: &str,
        working_dir: &Path,
        allowed_tools: &str,
        settings_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        Self::spawn_inner(session_id, working_dir, allowed_tools, settings_path, false)
    }

    /// Cold-start an existing session via `--resume`. Used when the app
    /// restarts on a previously persisted session id — subsequent turns in
    /// this process still flow through stdin.
    pub fn resume(
        session_id: &str,
        working_dir: &Path,
        allowed_tools: &str,
        settings_path: Option<&Path>,
    ) -> anyhow::Result<Self> {
        Self::spawn_inner(session_id, working_dir, allowed_tools, settings_path, true)
    }

    fn spawn_inner(
        session_id: &str,
        working_dir: &Path,
        allowed_tools: &str,
        settings_path: Option<&Path>,
        resume: bool,
    ) -> anyhow::Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json");

        if resume {
            cmd.arg("--resume").arg(session_id);
        } else {
            cmd.arg("--session-id").arg(session_id);
        }

        cmd.current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if !allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(allowed_tools);
        }

        if let Some(settings) = settings_path {
            cmd.arg("--settings").arg(settings);
        }

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (tx, rx) = flume::bounded(512);
        let sid = session_id.to_string();

        // Background OS thread: read stdout line-by-line, parse, send events.
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

        // Background thread: drain stderr and log it
        let sid_for_stderr = sid.clone();
        std::thread::Builder::new()
            .name("rich-stderr-reader".into())
            .spawn(move || {
                let reader = std::io::BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => eprintln!("[rich-stderr {sid_for_stderr}] {l}"),
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            events_rx: rx,
            session_id: sid,
            exited: false,
            in_progress: false,
        })
    }

    /// Send a new user turn to the running process. Errors if the process has
    /// died or stdin was closed.
    pub fn send_prompt(&mut self, text: &str) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin closed; session not accepting prompts"))?;
        let msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": text,
            }
        });
        writeln!(stdin, "{msg}")?;
        stdin.flush()?;
        self.in_progress = true;
        Ok(())
    }

    /// Drain all pending events from the background reader. Called on each
    /// render tick (16ms). Updates `in_progress` to false when a result
    /// event is observed so the UI can unblock input.
    pub fn drain_events(&mut self) -> Vec<RichEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events_rx.try_recv() {
            if matches!(&event, RichEvent::SessionResult { .. }) {
                self.in_progress = false;
            }
            events.push(event);
        }
        events
    }

    pub fn check_exited(&mut self) -> bool {
        if self.exited {
            return true;
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    self.exited = true;
                    self.in_progress = false;
                    true
                }
                Ok(None) => false,
                Err(_) => {
                    self.exited = true;
                    self.in_progress = false;
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

    /// True while the most recently sent prompt is still being processed
    /// (no SessionResult event seen yet).
    pub fn is_in_progress(&self) -> bool {
        self.in_progress && !self.exited
    }

    /// Kill the process (for mode switching or shutdown).
    pub fn kill(&mut self) {
        // Drop stdin first — signals EOF to the child, which should let it
        // exit cleanly if it's idle.
        self.stdin.take();
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
        }
        self.child = None;
        self.exited = true;
        self.in_progress = false;
    }
}

impl Drop for RichSession {
    fn drop(&mut self) {
        self.kill();
    }
}
