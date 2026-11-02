// Sidebar UI — project tree + session list + status indicators
// Phase 1: Static list with hardcoded projects
// Phase 3: Dynamic project/workspace/session tree

/// Named session groups (DEV-853) — the pure decisions `render` draws.
pub(crate) mod groups;
/// The MACHINES section (DEV-1163) — paired hosts and their sessions, as data.
pub(crate) mod machines;
/// A remote session's Interrupt / Discard menu (DEV-1168).
pub(crate) mod remote_menu;
pub(crate) mod render;
