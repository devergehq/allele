//! Rich Mode — structured rendering of Claude Code stream-json output.
//!
//! Components:
//!   - `RichSession` — process management (spawn CLI, read stdout, feed parser)
//!   - `RichView` — GPUI view rendering the activity feed
//!   - `VirtualScroll` — viewport-culled scrolling for the block list
//!   - Blocks — styled components (TextBlock, ToolCallCard, DiffElement, ThinkingAside)

mod rich_session;
mod compose_bar;
mod document;
mod rich_view;

pub use rich_session::RichSession;
pub use compose_bar::{bind_compose_keys, ComposeBar, ComposeBarEvent};
pub use document::*;
pub use rich_view::{RichView, RichViewEvent};
