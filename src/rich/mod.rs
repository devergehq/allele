//! Rich Sidecar — read-only structured view of a Claude Code session.
//!
//! Renders events tailed from Claude Code's own JSONL transcript
//! (`~/.claude/projects/<dashed-cwd>/<session-uuid>.jsonl` plus
//! `subagents/agent-*.jsonl`). The sidecar does NOT spawn, drive, or
//! otherwise communicate with the `claude` CLI — Claude Code runs
//! interactively in the PTY as it normally would, and Allele watches
//! the files it writes.
//!
//! Components:
//!   - `RichView` — GPUI view rendering the activity feed + compose bar
//!   - `RichDocument` / `Block` — internal document model (in `document`)
//!   - `ComposeBar` — multi-line input widget with paste-card + attachments
//!   - `markdown` — pulldown-cmark → GPUI element tree
//!   - `attachments` — file attachment pipeline (local-only; no IPC)
//!
//! User prompts composed in the ComposeBar are emitted upward via
//! `RichViewEvent::Submit` for the caller to route into the PTY using
//! the same bracketed-paste path the Scratch Pad uses.

pub mod attachments;
pub mod compose_bar;
mod document;
mod markdown;
mod rich_view;

pub use attachments::Attachment;
pub use compose_bar::{ComposeBar, ComposeBarEvent};
pub use document::*;
pub use rich_view::{RichView, RichViewEvent};
