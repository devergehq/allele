//! RichView — GPUI view that renders the activity feed of Claude Code events.
//!
//! Follows the same patterns as TerminalView:
//!   - Entity<RichView> is the view handle
//!   - 16ms poll loop drains events from RichSession and updates the document
//!   - Render walks the document blocks and produces styled GPUI elements

use gpui::*;

use super::document::{Block, BlockKind, RichDocument};
use super::rich_session::RichSession;

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
    session_id: String,
    font_size: f32,
    /// Auto-scroll to bottom on new content.
    auto_scroll: bool,
}

impl EventEmitter<RichViewEvent> for RichView {}

impl RichView {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        session: RichSession,
        session_id: String,
        font_size: f32,
    ) -> Self {
        let focus_handle = cx.focus_handle();

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
            session: Some(session),
            session_id,
            font_size,
            auto_scroll: true,
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
                    return true; // trigger repaint for "session ended" state
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

        // Scrollable container
        let mut content = div()
            .id("rich-view-scroll")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .bg(hex(BASE))
            .p(px(12.0));

        // Render each block
        for block in self.document.blocks() {
            let element = render_block(block, font_size);
            content = content.child(element);
        }

        // Session ended indicator
        if let Some(ref session) = self.session {
            if session.is_exited() && self.document.block_count() == 0 {
                content = content.child(
                    div()
                        .p(px(16.0))
                        .child(
                            div()
                                .text_color(hex(SUBTEXT0))
                                .text_size(px(font_size))
                                .child("Session ended with no output."),
                        ),
                );
            }
        }

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(hex(BASE))
            .child(content)
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
        } => {
            wrapper = wrapper.child(render_session_end(
                *duration_ms,
                *cost_usd,
                *num_turns,
                *is_error,
                font_size,
            ));
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
    font_size: f32,
) -> Div {
    let color = if is_error { hex(RED) } else { hex(TEAL) };
    let secs = duration_ms / 1000;
    let duration = if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    };

    div()
        .mt(px(8.0))
        .pt(px(8.0))
        .border_t_1()
        .border_color(hex_alpha(SURFACE1, 0.5))
        .child(
            div()
                .flex()
                .gap(px(16.0))
                .child(
                    div()
                        .text_color(color)
                        .text_size(px(font_size - 1.0))
                        .child(if is_error { "Session failed" } else { "Session complete" }),
                )
                .child(
                    div()
                        .text_color(hex(SUBTEXT0))
                        .text_size(px(font_size - 1.0))
                        .child(format!("{duration} | {num_turns} turns | ${cost_usd:.4}")),
                ),
        )
}
