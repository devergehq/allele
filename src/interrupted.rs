//! Noticing that a turn was interrupted (DEV-432).
//!
//! Allele moves a session out of [`SessionStatus::Running`] only on hook
//! events: `stop` becomes `ResponseReady`, `notification` becomes
//! `AwaitingInput`. **Claude Code fires neither when a turn is interrupted**,
//! so nothing ever moves it — an interrupted session shows a pulsing green
//! light while it sits idle asking what to do instead.
//!
//! That indicator is the one signal a human scans a fleet with, and
//! `sessions_status` reports the same wrong thing to an orchestrator, so the
//! lie propagates to both audiences.
//!
//! The interruption is recorded in exactly one place — the agent's transcript
//! — so that is where it has to be read from. The marker is stable and
//! identical in dispatched and hand-started sessions:
//!
//! ```json
//! {"type":"user","message":{"role":"user",
//!  "content":[{"type":"text","text":"[Request interrupted by user]"}]}}
//! ```
//!
//! `transcript::TranscriptTailer` cannot serve this: there is exactly one and
//! it follows the *active* session for the rich view, so it sees nothing in a
//! background session — which is every dispatched session, and the whole
//! point.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use gpui::{AsyncApp, WeakEntity};

use crate::app_state::AppState;
use crate::session::SessionStatus;

/// The literal Claude Code writes into the transcript when a turn is stopped.
///
/// Matched as a substring of the raw line rather than by parsing each entry:
/// the check runs against every new byte of every running session's
/// transcript, and parsing that JSON to find one sentinel would be a lot of
/// work to reach the same answer.
const MARKER: &str = "[Request interrupted by user]";

/// Watches running sessions' transcripts for interruptions.
///
/// Holds a byte offset per transcript so each poll reads only what is new.
#[derive(Default)]
pub struct InterruptWatcher {
    offsets: HashMap<PathBuf, u64>,
}

impl InterruptWatcher {
    /// Given the running sessions and their transcripts, return the ids whose
    /// transcript gained an interruption marker since the last poll.
    ///
    /// Sessions that are not running are not read at all, and are forgotten so
    /// a later run starts from the end of the file rather than re-reporting
    /// history.
    pub fn poll(&mut self, running: &[(String, PathBuf)]) -> Vec<String> {
        self.offsets
            .retain(|path, _| running.iter().any(|(_, p)| p == path));

        let mut interrupted = Vec::new();
        for (session_id, path) in running {
            if self.scan(path) {
                interrupted.push(session_id.clone());
            }
        }
        interrupted
    }

    /// Read whatever is new in `path` and report whether it contains a marker.
    ///
    /// A transcript first seen is skipped to its end rather than scanned: it
    /// may contain interruptions from previous turns, and re-reporting those
    /// would knock a genuinely-running session out of `Running` the moment
    /// allele restarted.
    fn scan(&mut self, path: &PathBuf) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        let len = meta.len();

        let Some(&offset) = self.offsets.get(path) else {
            self.offsets.insert(path.clone(), len);
            return false;
        };

        // Truncated or replaced — Claude Code rotated the conversation.
        // Start again from the end for the same reason as a first sighting.
        if len < offset {
            self.offsets.insert(path.clone(), len);
            return false;
        }
        if len == offset {
            return false;
        }

        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(offset)).is_err() {
            return false;
        }

        let mut found = false;
        let mut consumed = offset;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            // +1 for the newline the reader stripped.
            consumed += line.len() as u64 + 1;
            if line.contains(MARKER) {
                found = true;
            }
        }
        self.offsets.insert(path.clone(), consumed.min(len));
        found
    }
}

/// One pass: find running sessions, scan what is new in their transcripts,
/// and move any that were interrupted out of `Running`.
///
/// The file reads happen off the foreground thread. They are small — only the
/// bytes appended since the last pass, only for sessions actually running —
/// but this runs on the same cadence as the hook poller and the UI thread is
/// not the place for even cheap I/O.
pub(crate) async fn poll_once(
    watcher: &mut InterruptWatcher,
    this: &WeakEntity<AppState>,
    cx: &mut AsyncApp,
) {
    let Ok(running) = this.update(cx, |state: &mut AppState, _cx| {
        state
            .projects
            .iter()
            .flat_map(|p| p.sessions.iter())
            .filter(|s| s.status == SessionStatus::Running)
            .filter_map(|s| {
                let cwd = s.clone_path.as_ref()?;
                let jsonl = crate::transcript::expected_session_jsonl(cwd, s.claude_session_id())?;
                Some((s.id.clone(), jsonl))
            })
            .collect::<Vec<_>>()
    }) else {
        return;
    };

    // Still hand the (empty) list over when nothing is running, so the watcher
    // forgets sessions that stopped and does not replay their history later.
    let interrupted = cx
        .background_executor()
        .spawn({
            let mut w = std::mem::take(watcher);
            async move {
                let found = w.poll(&running);
                (w, found)
            }
        })
        .await;
    let (returned, interrupted) = interrupted;
    *watcher = returned;

    if interrupted.is_empty() {
        return;
    }

    let _ = this.update(cx, |state: &mut AppState, cx| {
        for id in &interrupted {
            if let Some(session) = state
                .projects
                .iter_mut()
                .flat_map(|p| p.sessions.iter_mut())
                .find(|s| &s.id == id)
            {
                // `ResponseReady`, not `Idle`: its documented meaning is
                // "finished a response turn; user should review and provide
                // the next prompt", which is exactly where an interrupted
                // session sits — Claude Code itself asks what to do instead.
                if session.status == SessionStatus::Running {
                    session.set_status(SessionStatus::ResponseReady);
                }
            }
        }
        cx.notify();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &PathBuf, contents: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        f.write_all(contents.as_bytes()).expect("write");
    }

    fn interrupt_line() -> String {
        format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\
             [{{\"type\":\"text\",\"text\":\"{MARKER}\"}}]}}}}\n"
        )
    }

    #[test]
    fn an_interruption_appended_while_running_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        write(&path, "{\"type\":\"assistant\"}\n");
        let running = vec![("s1".to_string(), path.clone())];

        let mut w = InterruptWatcher::default();
        assert!(
            w.poll(&running).is_empty(),
            "first sight establishes a baseline"
        );

        write(&path, &interrupt_line());
        assert_eq!(w.poll(&running), vec!["s1".to_string()]);
    }

    /// The marker must be reported once, not on every subsequent poll — a
    /// session the user restarts would otherwise be knocked straight back out
    /// of `Running`.
    #[test]
    fn an_interruption_is_reported_only_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        write(&path, "{\"type\":\"assistant\"}\n");
        let running = vec![("s1".to_string(), path.clone())];

        let mut w = InterruptWatcher::default();
        w.poll(&running);
        write(&path, &interrupt_line());
        assert_eq!(w.poll(&running).len(), 1);
        assert!(w.poll(&running).is_empty(), "must not re-report");
    }

    /// A transcript full of past interruptions must not knock a session out of
    /// `Running` the first time it is seen — which is what would happen on
    /// every allele restart.
    #[test]
    fn history_in_a_newly_seen_transcript_is_not_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        write(&path, &interrupt_line());
        write(&path, &interrupt_line());
        let running = vec![("s1".to_string(), path.clone())];

        let mut w = InterruptWatcher::default();
        assert!(w.poll(&running).is_empty());
    }

    /// Ordinary content must not trip it — only the marker.
    #[test]
    fn ordinary_transcript_content_is_not_an_interruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        write(&path, "{\"type\":\"assistant\"}\n");
        let running = vec![("s1".to_string(), path.clone())];

        let mut w = InterruptWatcher::default();
        w.poll(&running);
        write(
            &path,
            "{\"type\":\"assistant\",\"text\":\"still working\"}\n",
        );
        assert!(w.poll(&running).is_empty());
    }

    /// A session that stops running is forgotten, so when it runs again the
    /// scan resumes from the end rather than replaying its history.
    #[test]
    fn a_session_that_stops_running_is_forgotten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        write(&path, "{\"type\":\"assistant\"}\n");
        let running = vec![("s1".to_string(), path.clone())];

        let mut w = InterruptWatcher::default();
        w.poll(&running);
        assert_eq!(w.offsets.len(), 1);
        assert!(w.poll(&[]).is_empty());
        assert!(w.offsets.is_empty(), "dropped when no longer running");
    }

    /// The path derivation is the piece most likely to be silently wrong —
    /// get the dashed-cwd encoding off by one character and the watcher finds
    /// nothing, forever, with no error. Pinned against a real pair observed on
    /// this machine, including the `--` that a leading dot produces.
    #[test]
    fn the_transcript_path_matches_a_real_observed_session() {
        let cwd = std::path::Path::new("/Users/patrickdorival/.allele/workspaces/pancake/39cdeda2");
        let derived =
            crate::transcript::expected_session_jsonl(cwd, "39cdeda2-0906-48f5-a150-235a2d14575d")
                .expect("home dir");
        let tail: Vec<_> = derived
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        assert_eq!(tail[0], "39cdeda2-0906-48f5-a150-235a2d14575d.jsonl");
        assert_eq!(
            tail[1],
            "-Users-patrickdorival--allele-workspaces-pancake-39cdeda2"
        );
    }

    #[test]
    fn a_missing_transcript_is_not_an_interruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.jsonl");
        let running = vec![("s1".to_string(), path)];
        let mut w = InterruptWatcher::default();
        assert!(w.poll(&running).is_empty());
    }
}
