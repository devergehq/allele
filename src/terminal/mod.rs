mod grid_element;
pub mod keymap;
pub mod pty_terminal;
mod terminal_view;

pub use pty_terminal::ShellCommand;
pub use terminal_view::{
    clamp_font_size, TerminalEvent, TerminalView, DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MIN_FONT_SIZE,
};
