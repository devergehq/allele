use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use flume::{Receiver, Sender};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Grace window between SIGTERM and SIGKILL when tearing down a PTY's
/// child process group on drop. Short enough that a UI "close tab" feels
/// instant; long enough that well-behaved servers (vite, rails) can flush.
const TERM_GRACE: Duration = Duration::from_millis(750);

/// Cleanup callback run when a `PtyTerminal` is dropped, before the
/// process-group kill path. See `PtyTerminal::on_close`.
pub type CleanupHook = Box<dyn FnOnce() + Send + 'static>;

/// A command to run in the PTY
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment variables applied on top of the inherited env when
    /// the PTY is spawned. Used by agent adapters to pass session context to
    /// their event integration (e.g. opencode's `ALLELE_SESSION_ID`).
    pub env: Vec<(String, String)>,
}

impl ShellCommand {
    pub fn with_args_env(
        program: impl Into<String>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            program: program.into(),
            args,
            env,
        }
    }
}

/// Terminal size in cells and pixels
#[derive(Debug, Clone, Copy)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for TermSize {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }

    fn screen_lines(&self) -> usize {
        self.rows as usize
    }

    fn columns(&self) -> usize {
        self.cols as usize
    }
}

impl From<TermSize> for WindowSize {
    fn from(size: TermSize) -> Self {
        WindowSize {
            num_cols: size.cols,
            num_lines: size.rows,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

/// Event listener that forwards alacritty events over a channel
#[derive(Clone)]
pub struct JsonEventListener {
    tx: Sender<AlacEvent>,
}

impl JsonEventListener {
    pub fn new(tx: Sender<AlacEvent>) -> Self {
        Self { tx }
    }
}

impl EventListener for JsonEventListener {
    fn send_event(&self, event: AlacEvent) {
        let _ = self.tx.send(event);
    }
}

/// Wrapper around alacritty_terminal + PTY
pub struct PtyTerminal {
    pub term: Arc<FairMutex<Term<JsonEventListener>>>,
    pub pty_tx: Notifier,
    pub events_rx: Receiver<AlacEvent>,
    pub size: TermSize,
    pub exited: bool,
    /// Set to true when Bell event is received, cleared by consumer.
    pub bell_pending: bool,
    /// Title set by terminal apps via OSC sequences.
    pub title: Option<String>,
    /// Pid of the process alacritty forked, captured at spawn. `None` on
    /// non-Unix or if the pid was unavailable.
    ///
    /// alacritty calls `setsid()` in the fork, so this pid leads *a* process
    /// group — but not necessarily the one running the user's job. Which it is
    /// depends on how the terminal was spawned:
    ///
    /// - **Agent PTYs** exec the agent binary directly, with no wrapping shell.
    ///   Here this pid *is* the job, and `killpg` on it reaches everything.
    /// - **Drawer tabs** spawn with `command: None`, which becomes alacritty's
    ///   `default_shell_command`: `/usr/bin/login -flp <user> /bin/zsh …`.
    ///   `login` forks, the interactive shell re-groups, and job control puts
    ///   each job in a group of its own. This pid is `login`'s — three groups
    ///   away from the dev server.
    ///
    /// So this alone is not enough to tear a drawer tab down; see `master`.
    child_pid: Option<u32>,
    /// Our own duplicate of the PTY master descriptor. `None` on non-Unix.
    ///
    /// Exists so `Drop` can ask `tcgetpgrp` which process group is in the
    /// foreground — that is the group actually running the user's job, and the
    /// one `child_pid` cannot identify for a drawer tab.
    ///
    /// Released before the event loop shuts the PTY down: the kernel delivers
    /// SIGHUP when the *last* descriptor to the master closes, and SIGHUP is
    /// what does most of the killing here. Holding this open would suppress it.
    master: Option<std::fs::File>,
    /// Cleanup callbacks to run when this terminal is dropped. Fired in
    /// LIFO order (defer semantics) before the kill path, so hooks can
    /// still read outside state.
    cleanup_hooks: Vec<CleanupHook>,
}

impl PtyTerminal {
    /// Create a terminal running a specific command in a specific directory
    /// `extra_env` is applied on top of the inherited environment before the
    /// command's own vars. It is a separate parameter rather than part of
    /// `ShellCommand` because the drawer spawns bare shells with no command at
    /// all, and those are exactly the terminals a project most needs to
    /// configure. See DEV-485.
    pub fn spawn(
        size: TermSize,
        command: Option<ShellCommand>,
        working_dir: Option<PathBuf>,
        extra_env: Vec<(String, String)>,
    ) -> anyhow::Result<Self> {
        let (events_tx, events_rx) = flume::unbounded();
        let listener = JsonEventListener::new(events_tx);

        // Configure the terminal
        let term_config = TermConfig {
            scrolling_history: 10_000,
            ..Default::default()
        };

        // Create alacritty terminal
        let term = Term::new(term_config, &size, listener.clone());
        let term = Arc::new(FairMutex::new(term));

        // Build environment — ensure terminal capability is set correctly
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        // Ensure locale is set for proper unicode rendering
        env.insert("LANG".to_string(), "en_AU.UTF-8".to_string());
        env.insert("LC_ALL".to_string(), "en_AU.UTF-8".to_string());
        // Force Claude Code into alt-screen render mode (its own internal
        // scrollback) so primary-screen CSI 2J + cursor-positioning repaints
        // don't duplicate viewport content into our terminal scrollback.
        // Harmless for non-CC processes that ignore the variable.
        env.insert("CLAUDE_CODE_NO_FLICKER".to_string(), "1".to_string());

        // Project-declared vars land before the command's own, so an adapter
        // integration var always wins a collision with project config.
        for (k, v) in extra_env {
            env.insert(k, v);
        }

        // Build the shell configuration. Adapter-supplied env vars are
        // merged into the PTY environment here so agent event integrations
        // (e.g. opencode's ALLELE_SESSION_ID) reach the child process.
        let shell = command.map(|cmd| {
            for (k, v) in cmd.env {
                env.insert(k, v);
            }
            Shell::new(cmd.program, cmd.args)
        });

        let cwd = working_dir
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));

        // Configure PTY options
        let pty_options = PtyOptions {
            shell,
            working_directory: Some(cwd),
            env,
            drain_on_exit: true,
            #[cfg(target_os = "windows")]
            escape_args: true,
        };

        // Spawn the PTY
        let window_id = 0;
        let pty = tty::new(&pty_options, size.into(), window_id)?;

        // Capture what we need before the event loop takes ownership of the Pty.
        #[cfg(unix)]
        let child_pid = Some(pty.child().id());
        #[cfg(not(unix))]
        let child_pid: Option<u32> = None;

        // Duplicate the master so `Drop` can read the foreground process group
        // off it. `try_clone` failing is not fatal — we lose the ability to
        // identify the job's group and fall back to SIGHUP plus `child_pid`,
        // which is the behaviour that shipped before DEV-449.
        #[cfg(unix)]
        let master = pty.file().try_clone().ok();
        #[cfg(not(unix))]
        let master: Option<std::fs::File> = None;

        // Start the event loop (reads PTY output → feeds to Term)
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let pty_tx = Notifier(event_loop.channel());
        let _io_thread = event_loop.spawn();

        Ok(Self {
            term,
            pty_tx,
            events_rx,
            size,
            exited: false,
            bell_pending: false,
            title: None,
            child_pid,
            master,
            cleanup_hooks: Vec::new(),
        })
    }

    /// Pid of the agent process this terminal is running.
    ///
    /// alacritty execs the agent binary directly — there is no wrapping shell —
    /// so this is the agent's own pid, which is what names its messaging socket.
    /// See [`crate::dispatch::address`]: it is the basis of the only session
    /// address that a rename cannot break.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Register a callback to run when this terminal is dropped. Hooks
    /// fire in LIFO order (latest registration runs first) before the
    /// PTY is killed, so they can still observe app state. Panics in a
    /// hook are caught and logged — one bad hook won't skip the rest.
    pub fn on_close<F>(&mut self, hook: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.cleanup_hooks.push(Box::new(hook));
    }

    /// Write input bytes to the PTY
    pub fn write(&self, input: &[u8]) {
        let _ = self.pty_tx.0.send(Msg::Input(input.to_vec().into()));
    }

    /// Resize the terminal
    pub fn resize(&mut self, new_size: TermSize) {
        self.size = new_size;
        let _ = self.pty_tx.0.send(Msg::Resize(new_size.into()));
        self.term.lock().resize(new_size);
    }

    /// Drain pending events (call regularly to process PTY output)
    /// Returns true if there were events (meaning terminal needs redraw)
    pub fn drain_events(&mut self) -> bool {
        let mut had_events = false;
        while let Ok(event) = self.events_rx.try_recv() {
            had_events = true;
            match event {
                AlacEvent::ChildExit(_status) => {
                    self.exited = true;
                }
                AlacEvent::Exit => {
                    self.exited = true;
                }
                AlacEvent::Bell => {
                    self.bell_pending = true;
                }
                AlacEvent::Title(title) => {
                    self.title = Some(title);
                }
                AlacEvent::ResetTitle => {
                    self.title = None;
                }
                _ => {}
            }
        }
        had_events
    }
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        // Run cleanup hooks first (LIFO), while the PTY is still alive —
        // hooks may want to observe state before we tear things down.
        // Each hook is panic-caught so one failure doesn't skip the rest.
        while let Some(hook) = self.cleanup_hooks.pop() {
            if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(hook)) {
                warn!("PtyTerminal cleanup hook panicked: {panic:?}");
            }
        }

        // Read the foreground process group off the master while the PTY is
        // still open. For a drawer tab this is the only way to learn which
        // group is running the user's job — `child_pid` is `login`'s.
        #[cfg(unix)]
        let foreground = self.master.as_ref().and_then(|f| {
            use std::os::unix::io::AsRawFd;
            let pgid = unsafe { libc::tcgetpgrp(f.as_raw_fd()) };
            (pgid > 0).then_some(pgid)
        });

        // Release our duplicate BEFORE asking the event loop to shut down, so
        // that its close is the last one and the kernel delivers SIGHUP.
        self.master = None;

        // Close the PTY master FD — this signals the event loop to drain
        // and the kernel will SIGHUP the foreground process group.
        let _ = self.pty_tx.0.send(Msg::Shutdown);

        // Belt-and-braces: some children ignore SIGHUP, daemonise, or have
        // disowned their controlling terminal. SIGTERM every group we know
        // about, then SIGKILL whatever outlives the grace. Done on a detached
        // thread so Drop (render-thread) stays non-blocking.
        #[cfg(unix)]
        {
            let groups = target_groups(self.child_pid.take(), foreground);
            if !groups.is_empty() {
                std::thread::spawn(move || kill_process_groups(groups));
            }
        }
    }
}

/// The process groups worth signalling when a terminal is torn down.
///
/// `child` is what alacritty forked; `foreground` is what `tcgetpgrp` reported.
/// For an agent PTY these are the same group and the result is one entry; for a
/// drawer tab running a dev server they differ, and both matter — the shell's
/// group so the shell goes, the job's group so the server does.
#[cfg(unix)]
fn target_groups(child: Option<u32>, foreground: Option<libc::pid_t>) -> Vec<libc::pid_t> {
    let mut groups = Vec::new();
    if let Some(pid) = child {
        groups.push(pid as libc::pid_t);
    }
    if let Some(pgid) = foreground {
        if !groups.contains(&pgid) {
            groups.push(pgid);
        }
    }
    groups
}

/// SIGTERM every group, wait for them to go, then SIGKILL whatever is left.
/// Runs on a detached thread so it can wait without blocking the caller.
#[cfg(unix)]
fn kill_process_groups(groups: Vec<libc::pid_t>) {
    // SIGTERM — ask nicely. ESRCH (no such process) is fine: alacritty's
    // event loop may have already reaped, or SIGHUP may have done the job.
    for &pgid in &groups {
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
    }

    // Poll rather than sleeping the whole grace window.
    //
    // We deliberately do not `waitpid` (see below), so a pid can be reaped by
    // alacritty's event loop and returned to the OS at any point during the
    // wait — after which anything we signal lands on whoever holds the recycled
    // pid. Since `PtyTerminal::spawn` creates `setsid()` group leaders, the
    // plausible victim is another one of our own terminals. Returning the
    // instant the groups are gone keeps that window to tens of milliseconds
    // instead of the full grace. It narrows the hazard rather than removing it;
    // removing it needs allele to own the reaping (kqueue `EVFILT_PROC`).
    const POLL: Duration = Duration::from_millis(25);
    let deadline = std::time::Instant::now() + TERM_GRACE;
    loop {
        // killpg(pgid, 0) probes for a group's existence without signalling.
        if groups
            .iter()
            .all(|&pgid| unsafe { libc::killpg(pgid, 0) } != 0)
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }

    for &pgid in &groups {
        if unsafe { libc::killpg(pgid, 0) } == 0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
    // Do not waitpid — alacritty's EventLoop owns the Child and reaps it
    // when it drops. Waiting here would race with that reaper.
}

#[cfg(all(test, unix))]
mod tests {
    use super::target_groups;

    /// An agent PTY execs the binary directly, so the forked pid *is* the
    /// foreground group. One entry, not a duplicate signal.
    #[test]
    fn agent_pty_yields_a_single_group() {
        assert_eq!(target_groups(Some(4242), Some(4242)), vec![4242]);
    }

    /// A drawer tab spawns `/usr/bin/login -flp <user> /bin/zsh …`; login forks,
    /// the shell re-groups, and job control gives the dev server a group of its
    /// own. Signalling only `child` — which is what shipped before DEV-449 —
    /// reaches login and leaves the server running.
    #[test]
    fn drawer_tab_yields_both_the_shell_and_the_job() {
        assert_eq!(target_groups(Some(8244), Some(8455)), vec![8244, 8455]);
    }

    /// `tcgetpgrp` can fail — no controlling terminal, or the master already
    /// closed. Falling back to the old behaviour beats signalling nothing.
    #[test]
    fn a_failed_tcgetpgrp_falls_back_to_the_child_group() {
        assert_eq!(target_groups(Some(8244), None), vec![8244]);
    }

    /// `tcgetpgrp` returns -1 on error and 0 is never a valid pgid; `Drop`
    /// filters both to `None` before this point, so nothing to signal.
    #[test]
    fn nothing_known_means_nothing_signalled() {
        assert!(target_groups(None, None).is_empty());
    }
}
