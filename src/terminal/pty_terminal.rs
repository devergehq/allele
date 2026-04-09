use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions};
use alacritty_terminal::vte::ansi;
use flume::{Receiver, Sender};
use std::sync::Arc;

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
}

impl PtyTerminal {
    pub fn new(size: TermSize) -> anyhow::Result<Self> {
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

        // Configure PTY options — launch default shell
        let pty_options = PtyOptions {
            shell: None, // uses $SHELL or default
            working_directory: Some(std::env::current_dir().unwrap_or_default()),
            env: std::collections::HashMap::new(),
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
        })
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
    pub fn drain_events(&self) -> bool {
        let mut had_events = false;
        while let Ok(_event) = self.events_rx.try_recv() {
            had_events = true;
        }
        had_events
    }
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        let _ = self.pty_tx.0.send(Msg::Shutdown);
    }
}
