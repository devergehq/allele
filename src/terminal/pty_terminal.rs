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
}

impl ShellCommand {
    pub fn with_args(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
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
    /// Process-group leader pid of the child shell, captured at spawn.
    /// `None` on non-Unix or if the PID was unavailable. Used on drop to
    /// `killpg` the foreground tree so dev servers that ignore SIGHUP or
    /// daemonise don't leak.
    child_pid: Option<u32>,
    /// Cleanup callbacks to run when this terminal is dropped. Fired in
    /// LIFO order (defer semantics) before the kill path, so hooks can
    /// still read outside state.
    cleanup_hooks: Vec<CleanupHook>,
}

impl PtyTerminal {
    /// Create a terminal running a specific command in a specific directory
    pub fn spawn(
        size: TermSize,
        command: Option<ShellCommand>,
        working_dir: Option<PathBuf>,
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

        // Build the shell configuration
        let shell = command.map(|cmd| {
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

        // Capture the child's pid before the event loop takes ownership of
        // the Pty. alacritty calls `setsid()` in the fork, so this pid is
        // also the process-group leader — `killpg(pid, …)` reaches every
        // foreground descendant.
        #[cfg(unix)]
        let child_pid = Some(pty.child().id());
        #[cfg(not(unix))]
        let child_pid: Option<u32> = None;

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
            cleanup_hooks: Vec::new(),
        })
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

    /// Find the claude binary on the system
    pub fn find_claude() -> Option<PathBuf> {
        // Check common locations
        let candidates = [
            // User-local install
            dirs::home_dir().map(|h| h.join(".local/bin/claude")),
            // npm global
            dirs::home_dir().map(|h| h.join(".npm/bin/claude")),
            // Homebrew
            Some(PathBuf::from("/opt/homebrew/bin/claude")),
            Some(PathBuf::from("/usr/local/bin/claude")),
        ];

        for candidate in candidates.into_iter().flatten() {
            if candidate.exists() {
                return Some(candidate);
            }
        }

        // Fall back to PATH lookup
        which::which("claude").ok()
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
                eprintln!("PtyTerminal cleanup hook panicked: {panic:?}");
            }
        }

        // Close the PTY master FD — this signals the event loop to drain
        // and the kernel will SIGHUP the foreground process group.
        let _ = self.pty_tx.0.send(Msg::Shutdown);

        // Belt-and-braces: some children ignore SIGHUP, daemonise, or have
        // disowned their controlling terminal. Explicitly SIGTERM the
        // process group, then SIGKILL after a short grace. Done on a
        // detached thread so Drop (render-thread) stays non-blocking.
        #[cfg(unix)]
        if let Some(pid) = self.child_pid.take() {
            std::thread::spawn(move || kill_child_group(pid));
        }
    }
}

/// Kill the process group led by `pid` with a SIGTERM→grace→SIGKILL escalation.
/// Runs on a detached thread so it can sleep without blocking the caller.
#[cfg(unix)]
fn kill_child_group(pid: u32) {
    let pid = pid as libc::pid_t;
    // SIGTERM — ask nicely. ESRCH (no such process) is fine: alacritty's
    // event loop may have already reaped the child.
    unsafe { libc::killpg(pid, libc::SIGTERM) };

    std::thread::sleep(TERM_GRACE);

    // killpg(pid, 0) probes for the group's existence; if still alive,
    // escalate to SIGKILL.
    let alive = unsafe { libc::killpg(pid, 0) } == 0;
    if alive {
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
    // Do not waitpid — alacritty's EventLoop owns the Child and reaps it
    // when it drops. Waiting here would race with that reaper.
}
