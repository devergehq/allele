use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use flume::{Receiver, Sender};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A command to run in the PTY
pub struct ShellCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl ShellCommand {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

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
    /// Alt screen state — true when terminal is in alternate screen buffer.
    pub in_alt_screen: bool,
    /// Title set by terminal apps via OSC sequences.
    pub title: Option<String>,
}

impl PtyTerminal {
    /// Create a terminal running the default shell
    pub fn new(size: TermSize) -> anyhow::Result<Self> {
        Self::spawn(size, None, None)
    }

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
            in_alt_screen: false,
            title: None,
        })
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
        // Debug: log every resize with caller context
        let caller = if cfg!(debug_assertions) {
            let bt = std::backtrace::Backtrace::force_capture();
            // Extract just the first allele frame
            let bt_str = bt.to_string();
            bt_str.lines()
                .find(|l| l.contains("terminal_view") || l.contains("main"))
                .unwrap_or("unknown")
                .trim()
                .to_string()
        } else {
            String::new()
        };
        eprintln!(
            "PTY resize: {}x{} -> {}x{} [{}]",
            self.size.cols, self.size.rows, new_size.cols, new_size.rows, caller
        );
        self.size = new_size;
        let _ = self.pty_tx.0.send(Msg::Resize(new_size.into()));
        self.term.lock().resize(new_size);
    }

    /// Scroll the terminal display
    pub fn scroll(&self, delta: i32) {
        self.term.lock().scroll_display(Scroll::Delta(delta));
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
        let _ = self.pty_tx.0.send(Msg::Shutdown);
    }
}
