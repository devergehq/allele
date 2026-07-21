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
pub use document::truncate_to_char_boundary;
pub(crate) mod markdown;
// Narrative projection (DEV-29): interpretive layer over the event stream.
// The transcript reader (DEV-31) is its consumer; allow dead_code until then.
#[allow(dead_code)]
mod narrative;
// Tool-activity classification (DEV-35). classify_tool/default_collapsed drive
// the document's collapse behaviour today; the rail summary lands with the
// reader UI (DEV-31).
mod tool_rail;
// Transcript reading & navigation spine (DEV-31): search, jump index, and the
// "Since last viewed" model. The view layer renders on top; allow dead_code
// until that wiring lands.
#[allow(dead_code)]
mod reader;
// Permission & decision model (DEV-34): request cards, actions, decision log.
// Consumed by the permission card UI + hook wiring; allow dead_code until then.
#[allow(dead_code)]
mod permissions;
// Unified composer model (DEV-30): shared submission/validation/history policy
// for the Narrative compose bar and Scratch Pad. Both widgets migrate onto this
// in follow-up wiring; allow dead_code until then.
#[allow(dead_code)]
mod composer_model;
mod rich_view;

pub use rich_view::{RichView, RichViewEvent};
