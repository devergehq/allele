//! RichView — GPUI view that renders the activity feed of Claude Code events.
//!
//! Follows the same patterns as TerminalView:
//!   - Entity<RichView> is the view handle
//!   - 16ms poll loop drains events from RichSession and updates the document
//!   - Render walks the document blocks and produces styled GPUI elements

use gpui::*;

use super::compose_bar::{ComposeBar, ComposeBarEvent};
use super::document::{Block, BlockKind, RichDocument};
use super::rich_session::RichSession;

/// Scan `~/.claude/projects/*/` for a jsonl file matching this session id.
/// Returns false on any IO error so the caller falls back to `--session-id`.
fn session_history_exists(session_id: &str) -> bool {
    let Some(home) = dirs::home_dir() else { return false; };
    let projects_dir = home.join(".claude").join("projects");
    let needle = format!("{session_id}.jsonl");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else { return false; };
    for entry in entries.flatten() {
        let sub = entry.path();
        if !sub.is_dir() {
            continue;
        }
        if sub.join(&needle).exists() {
            return true;
        }
    }
    false
}

// ── Catppuccin Mocha palette (matching terminal) ──────────────────

const BASE: u32 = 0x1e1e2e;
const SURFACE0: u32 = 0x313244;
const SURFACE1: u32 = 0x45475a;
const TEXT: u32 = 0xcdd6f4;
const SUBTEXT0: u32 = 0xa6adc8;
const SUBTEXT1: u32 = 0xbac2de;
const GREEN: u32 = 0xa6e3a1;
const RED: u32 = 0xf38ba8;
const PEACH: u32 = 0xfab387;
const BLUE: u32 = 0x89b4fa;
const LAVENDER: u32 = 0xcba6f7;
const OVERLAY0: u32 = 0x6c7086;
const TEAL: u32 = 0x94e2d5;

fn hex(c: u32) -> Hsla {
    let r = ((c >> 16) & 0xFF) as f32 / 255.0;
    let g = ((c >> 8) & 0xFF) as f32 / 255.0;
    let b = (c & 0xFF) as f32 / 255.0;
    Rgba { r, g, b, a: 1.0 }.into()
}

fn hex_alpha(c: u32, alpha: f32) -> Hsla {
    let r = ((c >> 16) & 0xFF) as f32 / 255.0;
    let g = ((c >> 8) & 0xFF) as f32 / 255.0;
    let b = (c & 0xFF) as f32 / 255.0;
    Rgba { r, g, b, a: alpha }.into()
}

// ── Events emitted to parent ──────────────────────────────────────

pub enum RichViewEvent {
    /// User wants to switch this session to PTY mode.
    SwitchToPty,
}

// ── View ──────────────────────────────────────────────────────────

pub struct RichView {
    focus_handle: FocusHandle,
    document: RichDocument,
    session: Option<RichSession>,
    compose_bar: Entity<ComposeBar>,
    session_id: String,
    working_dir: std::path::PathBuf,
    allowed_tools: String,
    settings_path: Option<std::path::PathBuf>,
    font_size: f32,
    /// Auto-scroll to bottom on new content.
    auto_scroll: bool,
}

impl EventEmitter<RichViewEvent> for RichView {}

impl RichView {
    /// Create a new Rich Mode view.
    ///
    /// `session` is optional — pass `None` to create an idle RichView with no
    /// active CLI process. The user's first ComposeBar submission will spawn
    /// the first session (this avoids a dummy "Ready." introduction turn).
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        session: Option<RichSession>,
        session_id: String,
        working_dir: std::path::PathBuf,
        allowed_tools: String,
        settings_path: Option<std::path::PathBuf>,
        font_size: f32,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        // Create compose bar
        let compose_bar = cx.new(|cx| ComposeBar::new(cx, font_size));

        // Subscribe to compose bar submit events
        cx.subscribe(&compose_bar, |this: &mut Self, _bar, event: &ComposeBarEvent, cx| {
            match event {
                ComposeBarEvent::Submit { text } => {
                    this.handle_submit(text.clone(), cx);
                }
            }
        })
        .detach();

        // Start the 16ms poll loop (same pattern as TerminalView)
        cx.spawn_in(window, async |this: WeakEntity<Self>, cx: &mut AsyncWindowContext| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;

                let should_continue = this
                    .update(cx, |this: &mut Self, cx: &mut Context<Self>| {
                        let changed = this.poll_events();
                        if changed {
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);

                if !should_continue {
                    break;
                }
            }
        })
        .detach();

        Self {
            focus_handle,
            document: RichDocument::new(),
            session,
            compose_bar,
            session_id,
            working_dir,
            allowed_tools,
            settings_path,
            font_size,
            auto_scroll: true,
        }
    }

    /// Handle a prompt submission from the compose bar.
    fn handle_submit(&mut self, text: String, cx: &mut Context<Self>) {
        // Kill current session if still running
        if let Some(ref mut session) = self.session {
            if !session.is_exited() {
                // Session still active — can't send a new prompt until it finishes
                // TODO: support interrupting + sending follow-up
                return;
            }
        }

        // Echo the user's prompt into the feed BEFORE spawning the CLI,
        // so the user sees their message immediately.
        self.document.push_user_prompt(text.clone());

        // Mark compose bar as busy
        self.compose_bar.update(cx, |bar, cx| bar.set_busy(true, cx));

        // Decide whether to resume or start fresh based on whether the CLI
        // has written a history file for this session id yet.
        let has_history = session_history_exists(&self.session_id);
        let spawn_result = if has_history {
            RichSession::resume(
                &text,
                &self.session_id,
                &self.working_dir,
                &self.allowed_tools,
                self.settings_path.as_deref(),
            )
        } else {
            RichSession::spawn(
                &text,
                &self.session_id,
                &self.working_dir,
                &self.allowed_tools,
                self.settings_path.as_deref(),
            )
        };

        match spawn_result {
            Ok(new_session) => {
                self.session = Some(new_session);
                // Show the thinking indicator until first output arrives.
                self.document.push_awaiting_indicator();
                cx.notify();
            }
            Err(e) => {
                eprintln!("[rich] failed to spawn session: {e}");
                self.compose_bar.update(cx, |bar, cx| bar.set_busy(false, cx));
            }
        }
    }

    /// Drain events from the RichSession and apply to the document.
    /// Returns true if anything changed (needs repaint).
    fn poll_events(&mut self) -> bool {
        let events = if let Some(ref mut session) = self.session {
            session.drain_events()
        } else {
            return false;
        };

        if events.is_empty() {
            // Check if process exited
            if let Some(ref mut session) = self.session {
                if session.check_exited() {
                    // Unset busy on compose bar so user can send follow-up
                    // (can't borrow cx here — notify will trigger re-render
                    // which re-evaluates busy state)
                    return true;
                }
            }
            return false;
        }

        for event in events {
            self.document.apply_event(event);
        }
        true
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Focus handle of the compose bar (what should receive keystrokes).
    pub fn compose_focus_handle(&self, cx: &App) -> FocusHandle {
        self.compose_bar.read(cx).focus_handle().clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Kill the underlying process (for mode switching).
    pub fn kill_session(&mut self) {
        if let Some(ref mut session) = self.session {
            session.kill();
        }
    }
}

impl Render for RichView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = self.font_size;

        // Update compose bar busy state based on session
        let session_active = self
            .session
            .as_ref()
            .map(|s| !s.is_exited())
            .unwrap_or(false);
        self.compose_bar.update(cx, |bar, cx| bar.set_busy(session_active, cx));

        // Scrollable activity feed.
        //
        // Separates concerns: outer = scroll container (flex child, bounded
        // by parent); inner = content layout (flex-col with padding and the
        // blocks). This avoids conflating the scroll viewport with the
        // children's flex layout, which is what was preventing GPUI from
        // activating the scroll behaviour.
        let mut inner = div()
            .flex()
            .flex_col()
            .p(px(12.0));

        // Render each block into the inner content container.
        for block in self.document.blocks() {
            let element = render_block(block, font_size);
            inner = inner.child(element);
        }

        // Empty state
        if self.document.block_count() == 0 {
            let message = if session_active {
                "Waiting for response..."
            } else {
                "Send a message to start."
            };
            inner = inner.child(
                div()
                    .py(px(48.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_color(hex(SUBTEXT0))
                            .text_size(px(font_size))
                            .child(message),
                    ),
            );
        }

        // Scrollable viewport. `flex_1 + min_h(0)` is the correct sibling
        // behaviour inside rich_view's flex-col (feed + compose_bar).
        let feed = div()
            .id("rich-view-feed")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .bg(hex(BASE))
            .child(inner);

        // Main layout: feed + compose bar.
        //
        // `size_full()` is critical. GPUI's default display mode is `Block`,
        // not `Flex`. The parent `main_area` in main.rs does NOT call
        // `.flex()` — it's a Block container. In Block layout, a child's
        // `flex_1` has NO effect; the child is sized by content and
        // overflows silently (clipped by `overflow_hidden` on main_area).
        //
        // By using `size_full` (w:100% + h:100%) we get a definite size
        // from main_area's bounded height regardless of its display mode.
        // The internal `.flex().flex_col()` then correctly distributes
        // that size between the feed (`flex_1`) and the compose bar
        // (`flex_shrink_0`), and the feed's `overflow_y_scroll` finally
        // has a bounded viewport to scroll within.
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(hex(BASE))
            .child(feed)
            .child(self.compose_bar.clone())
    }
}

// ── Block renderers ───────────────────────────────────────────────

fn render_block(block: &Block, font_size: f32) -> Div {
    let indent = if block.parent_agent_id.is_some() {
        px(24.0)
    } else {
        px(0.0)
    };

    let mut wrapper = div().pl(indent).mb(px(4.0));

    match &block.kind {
        BlockKind::Text { content, streaming } => {
            wrapper = wrapper.child(render_text_block(content, *streaming, font_size));
        }
        BlockKind::Thinking { content } => {
            wrapper = wrapper.child(render_thinking_block(content, block.collapsed, font_size));
        }
        BlockKind::ToolCall {
            tool_name,
            input_summary,
            result,
            ..
        } => {
            wrapper = wrapper.child(render_tool_call(tool_name, input_summary, result.as_ref(), font_size));
        }
        BlockKind::Diff {
            file_path,
            old_string,
            new_string,
            result,
            ..
        } => {
            wrapper = wrapper.child(render_diff(file_path, old_string, new_string, result.as_ref(), font_size));
        }
        BlockKind::SessionEnd {
            duration_ms,
            cost_usd,
            num_turns,
            is_error,
            result_text,
        } => {
            wrapper = wrapper.child(render_session_end(
                *duration_ms,
                *cost_usd,
                *num_turns,
                *is_error,
                result_text.as_deref(),
                font_size,
            ));
        }
        BlockKind::UserPrompt { content } => {
            wrapper = wrapper.child(render_user_prompt(content, font_size));
        }
        BlockKind::AwaitingResponse => {
            wrapper = wrapper.child(render_awaiting(font_size));
        }
    }

    wrapper
}

// ── Text block ────────────────────────────────────────────────────

fn render_text_block(content: &str, streaming: bool, font_size: f32) -> Div {
    let text_color = if streaming {
        hex(SUBTEXT1)
    } else {
        hex(TEXT)
    };

    div()
        .py(px(4.0))
        .child(
            div()
                .text_color(text_color)
                .text_size(px(font_size))
                .child(content.to_string()),
        )
}

// ── Thinking block (collapsed by default, subtle) ─────────────────

fn render_thinking_block(content: &str, collapsed: bool, font_size: f32) -> Div {
    let header = div()
        .flex()
        .gap(px(6.0))
        .items_center()
        .child(
            div()
                .text_color(hex(OVERLAY0))
                .text_size(px(font_size - 1.0))
                .child("thinking"),
        )
        .child(
            div()
                .text_color(hex(OVERLAY0))
                .text_size(px(font_size - 2.0))
                .child(if collapsed { "+" } else { "-" }),
        );

    let mut block = div()
        .py(px(2.0))
        .pl(px(8.0))
        .border_l_2()
        .border_color(hex_alpha(OVERLAY0, 0.3))
        .child(header);

    if !collapsed {
        block = block.child(
            div()
                .mt(px(4.0))
                .text_color(hex_alpha(OVERLAY0, 0.7))
                .text_size(px(font_size - 1.0))
                .child(content.to_string()),
        );
    }

    block
}

// ── Tool call card ────────────────────────────────────────────────

fn render_tool_call(
    tool_name: &str,
    input_summary: &str,
    result: Option<&super::document::ToolCallResult>,
    font_size: f32,
) -> Div {
    let status_color = match result {
        Some(r) if r.is_error => hex(RED),
        Some(_) => hex(GREEN),
        None => hex(PEACH), // still running
    };

    let mut card = div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(hex_alpha(SURFACE0, 0.6))
        .border_l_2()
        .border_color(status_color);

    // Header: tool name + summary
    card = card.child(
        div()
            .flex()
            .gap(px(8.0))
            .items_center()
            .child(
                div()
                    .text_color(hex(BLUE))
                    .text_size(px(font_size - 1.0))
                    .font_weight(FontWeight::BOLD)
                    .child(tool_name.to_string()),
            )
            .child(
                div()
                    .text_color(hex(SUBTEXT0))
                    .text_size(px(font_size - 1.0))
                    .child(input_summary.to_string()),
            ),
    );

    // Result (if available and is error)
    if let Some(r) = result {
        if r.is_error {
            let preview = if r.content.len() > 200 {
                format!("{}...", &r.content[..197])
            } else {
                r.content.clone()
            };
            card = card.child(
                div()
                    .mt(px(4.0))
                    .text_color(hex(RED))
                    .text_size(px(font_size - 1.0))
                    .child(preview),
            );
        }
    }

    card
}

// ── Diff view ─────────────────────────────────────────────────────

fn render_diff(
    file_path: &str,
    old_string: &str,
    new_string: &str,
    _result: Option<&super::document::ToolCallResult>,
    font_size: f32,
) -> Div {
    let code_size = font_size - 1.0;

    let mut diff = div()
        .rounded(px(4.0))
        .bg(hex_alpha(SURFACE0, 0.4))
        .overflow_hidden();

    // File path header
    diff = diff.child(
        div()
            .px(px(10.0))
            .py(px(4.0))
            .bg(hex_alpha(SURFACE1, 0.6))
            .child(
                div()
                    .text_color(hex(SUBTEXT1))
                    .text_size(px(code_size))
                    .child(file_path.to_string()),
            ),
    );

    // Old lines (red)
    for line in old_string.lines() {
        diff = diff.child(
            div()
                .px(px(10.0))
                .py(px(1.0))
                .bg(hex_alpha(RED, 0.1))
                .flex()
                .gap(px(6.0))
                .child(
                    div()
                        .text_color(hex(RED))
                        .text_size(px(code_size))
                        .child("-"),
                )
                .child(
                    div()
                        .text_color(hex_alpha(RED, 0.8))
                        .text_size(px(code_size))
                        .child(line.to_string()),
                ),
        );
    }

    // New lines (green)
    for line in new_string.lines() {
        diff = diff.child(
            div()
                .px(px(10.0))
                .py(px(1.0))
                .bg(hex_alpha(GREEN, 0.1))
                .flex()
                .gap(px(6.0))
                .child(
                    div()
                        .text_color(hex(GREEN))
                        .text_size(px(code_size))
                        .child("+"),
                )
                .child(
                    div()
                        .text_color(hex_alpha(GREEN, 0.8))
                        .text_size(px(code_size))
                        .child(line.to_string()),
                ),
        );
    }

    diff
}

// ── Session end ───────────────────────────────────────────────────

fn render_session_end(
    duration_ms: u64,
    cost_usd: f64,
    num_turns: u32,
    is_error: bool,
    result_text: Option<&str>,
    font_size: f32,
) -> Div {
    let color = if is_error { hex(RED) } else { hex(TEAL) };
    let secs = duration_ms / 1000;
    let duration = if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    };

    // Subtle inline stats after each turn (cost/duration), NOT a "session
    // complete" banner. The conversation isn't over — each claude -p
    // invocation is one turn, and the user can keep sending follow-ups.
    let label = if is_error { "Turn failed" } else { "Turn" };
    let mut block = div()
        .mt(px(4.0))
        .mb(px(4.0))
        .px(px(6.0))
        .child(
            div()
                .flex()
                .gap(px(10.0))
                .child(
                    div()
                        .text_color(color)
                        .text_size(px(font_size - 2.0))
                        .child(label),
                )
                .child(
                    div()
                        .text_color(hex_alpha(SUBTEXT0, 0.7))
                        .text_size(px(font_size - 2.0))
                        .child(format!("{duration} · ${cost_usd:.4}")),
                ),
        );

    // Show error text if available
    if let Some(text) = result_text {
        if !text.is_empty() {
            let preview = if text.len() > 500 {
                format!("{}...", &text[..497])
            } else {
                text.to_string()
            };
            block = block.child(
                div()
                    .mt(px(6.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(hex_alpha(SURFACE0, 0.4))
                    .child(
                        div()
                            .text_color(if is_error { hex(RED) } else { hex(SUBTEXT0) })
                            .text_size(px(font_size - 1.0))
                            .child(preview),
                    ),
            );
        }
    }

    block
}

// ── User prompt (echoed when user submits) ────────────────────────

fn render_user_prompt(content: &str, font_size: f32) -> Div {
    div()
        .my(px(6.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(hex_alpha(BLUE, 0.08))
        .border_l_2()
        .border_color(hex_alpha(BLUE, 0.5))
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .items_start()
                .child(
                    div()
                        .text_color(hex(BLUE))
                        .text_size(px(font_size - 1.0))
                        .font_weight(FontWeight::BOLD)
                        .child("You"),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(hex(TEXT))
                        .text_size(px(font_size))
                        .child(content.to_string()),
                ),
        )
}

// ── Awaiting response (thinking indicator) ────────────────────────

fn render_awaiting(font_size: f32) -> Div {
    div()
        .my(px(6.0))
        .px(px(10.0))
        .py(px(6.0))
        .flex()
        .gap(px(8.0))
        .items_center()
        .child(
            div()
                .text_color(hex(PEACH))
                .text_size(px(font_size))
                .child("●"),
        )
        .child(
            div()
                .text_color(hex(SUBTEXT0))
                .text_size(px(font_size - 1.0))
                .child("Thinking…"),
        )
}
