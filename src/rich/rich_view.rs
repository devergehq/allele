//! RichView — GPUI view that renders the activity feed of a Claude Code
//! session as a READ-ONLY sidecar companion to the PTY terminal view.
//!
//! This view does NOT spawn, drive, or otherwise communicate with the
//! `claude` CLI. It renders events fed in from the outside (the caller
//! tails Claude Code's own JSONL transcript and pipes events in via
//! `apply_event`). User prompts composed here are emitted upward via
//! `RichViewEvent::Submit` for the caller to route through the existing
//! interactive PTY — the same path the Scratch Pad uses.
//!
//! Pattern:
//!   - Entity<RichView> is the view handle
//!   - Caller drives a file tailer and calls `apply_event` per new event
//!   - Caller calls `set_busy` to lock/unlock the ComposeBar
//!   - Render walks the document blocks and produces styled GPUI elements

use gpui::*;

use super::compose_bar::{ComposeBar, ComposeBarEvent};
use super::document::{short_path, Block, BlockKind, RichDocument};
use crate::stream::RichEvent;

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
    /// User pressed Cmd+Enter in the compose bar. The parent is responsible
    /// for routing this into the PTY (via bracketed paste) using the same
    /// path the Scratch Pad uses. `attachments` is the list of files the
    /// user dragged/pasted/picked; the parent decides the on-wire format
    /// (e.g. `@path\n` prefix convention).
    Submit {
        text: String,
        attachments: Vec<super::attachments::Attachment>,
    },
}

// ── View ──────────────────────────────────────────────────────────

pub struct RichView {
    focus_handle: FocusHandle,
    document: RichDocument,
    compose_bar: Entity<ComposeBar>,
    font_size: f32,
    /// Whether the compose bar is locked because a turn is in flight.
    /// Driven externally via `set_busy` — the sidecar view itself never
    /// computes this, since busy state depends on the PTY/transcript
    /// observation loop the parent owns.
    busy: bool,
    /// GPUI virtual list state — only renders visible blocks + overdraw,
    /// giving O(visible) render cost instead of O(total).
    list_state: ListState,
}

impl EventEmitter<RichViewEvent> for RichView {}

impl RichView {
    /// Create a new sidecar Rich view.
    ///
    /// `session_id` is used only for per-session attachment scoping
    /// (`~/.allele/attachments/<session_id>/`). The view does not start a
    /// CLI process and does not tail files itself — the caller drives both.
    pub fn new(
        cx: &mut Context<Self>,
        session_id: String,
        font_size: f32,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let compose_bar = cx.new(|cx| ComposeBar::new(cx, font_size, session_id.clone()));

        // Re-emit compose bar submits upward so the parent can route to
        // the PTY. The view never writes to `claude`'s stdin directly.
        cx.subscribe(&compose_bar, |_this: &mut Self, _bar, event: &ComposeBarEvent, cx| {
            match event {
                ComposeBarEvent::Submit { text, attachments } => {
                    cx.emit(RichViewEvent::Submit {
                        text: text.clone(),
                        attachments: attachments.clone(),
                    });
                }
            }
        })
        .detach();

        let list_state = ListState::new(0, ListAlignment::Bottom, px(200.0));

        Self {
            focus_handle,
            document: RichDocument::new(),
            compose_bar,
            font_size,
            busy: false,
            list_state,
        }
    }

    /// Apply one event from the transcript tailer. The caller is
    /// responsible for watching `~/.claude/projects/<cwd>/<session>.jsonl`
    /// (+ `subagents/*.jsonl`) and feeding each parsed event in order.
    pub fn apply_event(&mut self, event: RichEvent, cx: &mut Context<Self>) {
        let old_count = self.document.block_count();
        self.document.apply_event(event);
        let new_count = self.document.block_count();
        self.sync_list_state(old_count, new_count);
        cx.notify();
    }

    /// Echo a user prompt into the document. The parent should call this
    /// after routing a ComposeBar submit into the PTY, so the user's text
    /// appears in the feed immediately without waiting for Claude Code to
    /// write it to the transcript.
    pub fn push_user_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        let old_count = self.document.block_count();
        self.document.push_user_prompt(text);
        self.document.push_awaiting_indicator();
        let new_count = self.document.block_count();
        self.sync_list_state(old_count, new_count);
        cx.notify();
    }

    fn sync_list_state(&mut self, old_count: usize, new_count: usize) {
        if new_count > old_count {
            self.list_state.splice(old_count..old_count, new_count - old_count);
        } else if new_count < old_count {
            self.list_state.splice(new_count..old_count, 0);
        }
    }

    fn render_block_at(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let font_size = self.font_size;
        let Some(block) = self.document.blocks().get(ix) else {
            return div().into_any_element();
        };
        render_block(block, font_size, cx).into_any_element()
    }

    fn render_scrollbar(&self) -> Div {
        let max_offset = self.list_state.max_offset_for_scrollbar().height;
        let current_offset = -self.list_state.scroll_px_offset_for_scrollbar().y;
        let viewport = self.list_state.viewport_bounds();
        let viewport_height = viewport.size.height;

        let show = max_offset > px(0.0) && viewport_height > px(0.0);

        if !show {
            return div();
        }

        let total_height = max_offset + viewport_height;
        let thumb_ratio = (viewport_height / total_height).min(1.0);
        let thumb_height = (viewport_height * thumb_ratio).max(px(20.0));
        let scroll_fraction = current_offset / max_offset;
        let thumb_top = (viewport_height - thumb_height) * scroll_fraction;

        div()
            .absolute()
            .right(px(1.0))
            .top(px(0.0))
            .bottom(px(0.0))
            .w(px(8.0))
            .child(
                div()
                    .absolute()
                    .right(px(1.0))
                    .w(px(6.0))
                    .h(thumb_height)
                    .top(thumb_top)
                    .rounded(px(3.0))
                    .bg(hex_alpha(TEXT, 0.25)),
            )
    }

}

impl Render for RichView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = self.font_size;
        let busy = self.busy;
        self.compose_bar.update(cx, |bar, cx| bar.set_busy(busy, cx));

        // Empty state — show placeholder instead of the virtual list.
        if self.document.block_count() == 0 {
            let message = if busy {
                "Waiting for response..."
            } else {
                "Send a message to start."
            };
            return div()
                .track_focus(&self.focus_handle)
                .size_full()
                .overflow_hidden()
                .flex()
                .flex_col()
                .bg(hex(BASE))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_color(hex(SUBTEXT0))
                                .text_size(px(font_size))
                                .child(message),
                        ),
                )
                .child(self.compose_bar.clone());
        }

        // Virtual list — only renders visible blocks + overdraw.
        let feed_list = list(
            self.list_state.clone(),
            cx.processor(Self::render_block_at),
        )
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .p(px(12.0))
        .size_full();

        let scrollbar = self.render_scrollbar();

        // `size_full()` is critical — see comment in previous version.
        // The internal flex-col distributes height between the feed
        // (`flex_1`) and compose bar (`flex_shrink_0`).
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(hex(BASE))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(feed_list)
                    .child(scrollbar),
            )
            .child(self.compose_bar.clone())
    }
}

// ── Block renderers ───────────────────────────────────────────────

fn render_block(block: &Block, font_size: f32, cx: &mut Context<RichView>) -> Div {
    let indent = if block.parent_agent_id.is_some() {
        px(24.0)
    } else {
        px(0.0)
    };

    // `w_full()` + `min_w_0()` are load-bearing: without them the list's
    // `Auto` sizing lets each block grow to the intrinsic width of its
    // widest text run (long diff lines, stringified JSON, etc.) and the
    // viewport has no horizontal scroll — so text runs off the page.
    // Constrained here, long text wraps at the list's own width.
    let mut wrapper = div().w_full().min_w_0().pl(indent).mb(px(4.0));

    let block_id = block.id;

    match &block.kind {
        BlockKind::Text { content, streaming } => {
            wrapper = wrapper.child(render_text_block(content, *streaming, font_size));
        }
        BlockKind::Thinking { content } => {
            wrapper = wrapper.child(render_thinking_block(
                block_id,
                content,
                block.collapsed,
                font_size,
                cx,
            ));
        }
        BlockKind::ToolCall {
            tool_name,
            input_summary,
            input_full,
            result,
            ..
        } => {
            wrapper = wrapper.child(render_tool_call(
                block_id,
                tool_name,
                input_summary,
                input_full,
                block.collapsed,
                result.as_ref(),
                font_size,
                cx,
            ));
        }
        BlockKind::Diff {
            file_path,
            old_string,
            new_string,
            result,
            ..
        } => {
            wrapper = wrapper.child(render_diff(
                block_id,
                file_path,
                old_string,
                new_string,
                block.collapsed,
                result.as_ref(),
                font_size,
                cx,
            ));
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
    // Claude's prose IS the main content of the transcript — tool calls
    // and thinking are supporting context. Visually: a speaker pill on
    // the left mirroring the "You" user-prompt pattern, and generous
    // vertical rhythm to separate it from surrounding cards.
    div()
        .w_full()
        .min_w_0()
        .my(px(8.0))
        .px(px(10.0))
        .py(px(6.0))
        .flex()
        .gap(px(8.0))
        .items_start()
        .child(
            div()
                .flex_shrink_0()
                .text_color(hex(LAVENDER))
                .text_size(px(font_size - 1.0))
                .font_weight(FontWeight::BOLD)
                .child("Claude"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(super::markdown::render(content, streaming, font_size)),
        )
}

// ── Thinking block (collapsed by default, subtle) ─────────────────

/// Chevron for collapsible blocks. `▸` = collapsed, `▾` = expanded.
fn chevron(collapsed: bool) -> &'static str {
    if collapsed { "▸" } else { "▾" }
}

fn render_thinking_block(
    block_id: super::document::BlockId,
    content: &str,
    collapsed: bool,
    font_size: f32,
    cx: &mut Context<RichView>,
) -> Div {
    let header = div()
        .id(ElementId::Name(format!("thinking-header-{block_id}").into()))
        .flex()
        .gap(px(6.0))
        .items_center()
        .cursor(gpui::CursorStyle::PointingHand)
        .child(
            div()
                .text_color(hex(OVERLAY0))
                .text_size(px(font_size - 2.0))
                .child(chevron(collapsed)),
        )
        .child(
            div()
                .text_color(hex(OVERLAY0))
                .text_size(px(font_size - 1.0))
                .child("thinking"),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.document.toggle_collapsed(block_id);
                cx.notify();
            }),
        );

    let mut block = div()
        .w_full()
        .min_w_0()
        .py(px(2.0))
        .pl(px(8.0))
        .border_l_2()
        .border_color(hex_alpha(OVERLAY0, 0.3))
        .child(header);

    if !collapsed {
        block = block.child(
            div()
                .w_full()
                .min_w_0()
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
    block_id: super::document::BlockId,
    tool_name: &str,
    input_summary: &str,
    input_full: &serde_json::Value,
    collapsed: bool,
    result: Option<&super::document::ToolCallResult>,
    font_size: f32,
    cx: &mut Context<RichView>,
) -> Div {
    let status_color = match result {
        Some(r) if r.is_error => hex(RED),
        Some(_) => hex(GREEN),
        None => hex(PEACH), // still running
    };

    let mut card = div()
        .w_full()
        .min_w_0()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(hex_alpha(SURFACE0, 0.6))
        .border_l_2()
        .border_color(status_color);

    // Header: chevron + tool name + summary (clickable).
    card = card.child(
        div()
            .id(ElementId::Name(format!("toolcall-header-{block_id}").into()))
            .w_full()
            .min_w_0()
            .flex()
            .gap(px(8.0))
            .items_center()
            .cursor(gpui::CursorStyle::PointingHand)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(hex(SUBTEXT0))
                    .text_size(px(font_size - 2.0))
                    .child(chevron(collapsed)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(hex(BLUE))
                    .text_size(px(font_size - 1.0))
                    .font_weight(FontWeight::BOLD)
                    .child(tool_name.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(hex(SUBTEXT0))
                    .text_size(px(font_size - 1.0))
                    .child(input_summary.to_string()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.document.toggle_collapsed(block_id);
                    cx.notify();
                }),
            ),
    );

    // Expanded: pretty-printed input JSON. Each line becomes its own
    // child so long values wrap at the card's width instead of the
    // whole blob being rendered as one un-wrappable run. Falls back to
    // Debug repr on serialisation failure so we never panic on
    // malformed input.
    if !collapsed {
        let pretty = serde_json::to_string_pretty(input_full)
            .unwrap_or_else(|_| format!("{input_full:?}"));
        let mut json_block = div()
            .w_full()
            .min_w_0()
            .mt(px(6.0))
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(3.0))
            .bg(hex_alpha(SURFACE1, 0.4))
            .text_color(hex(SUBTEXT1))
            .text_size(px(font_size - 2.0))
            .font_family("JetBrains Mono");
        for line in pretty.lines() {
            json_block = json_block.child(
                div()
                    .w_full()
                    .min_w_0()
                    .child(line.to_string()),
            );
        }
        card = card.child(json_block);
    }

    // Result (if available and is error) — always shown regardless of
    // collapsed state so errors are never hidden.
    if let Some(r) = result {
        if r.is_error {
            let preview = if r.content.len() > 200 {
                format!("{}...", &r.content[..197])
            } else {
                r.content.clone()
            };
            card = card.child(
                div()
                    .w_full()
                    .min_w_0()
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
    block_id: super::document::BlockId,
    file_path: &str,
    old_string: &str,
    new_string: &str,
    collapsed: bool,
    _result: Option<&super::document::ToolCallResult>,
    font_size: f32,
    cx: &mut Context<RichView>,
) -> Div {
    let code_size = font_size - 1.0;

    // Line-delta summary for the collapsed header. Newline count is a
    // good-enough proxy for git-style +/- since Edit replaces a chunk;
    // it lets a long edit turn read as a tidy list instead of a wall.
    let removed = old_string.lines().count();
    let added = new_string.lines().count();

    let mut diff = div()
        .w_full()
        .min_w_0()
        .rounded(px(4.0))
        .bg(hex_alpha(SURFACE0, 0.4))
        .overflow_hidden();

    // File path header (clickable — toggles collapsed state).
    // When collapsed, the header IS the whole card: chevron, "Edit"
    // verb, shortened path, then line-delta pills. When expanded the
    // before/after body follows below.
    let short = short_path(file_path);
    diff = diff.child(
        div()
            .id(ElementId::Name(format!("diff-header-{block_id}").into()))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .py(px(4.0))
            .bg(hex_alpha(SURFACE1, 0.6))
            .cursor(gpui::CursorStyle::PointingHand)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(hex(SUBTEXT0))
                    .text_size(px(code_size - 1.0))
                    .child(chevron(collapsed)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(hex(PEACH))
                    .text_size(px(code_size))
                    .font_weight(FontWeight::BOLD)
                    .child("Edit"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(hex(SUBTEXT1))
                    .text_size(px(code_size))
                    .font_family("JetBrains Mono")
                    .child(short),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(hex(GREEN))
                    .text_size(px(code_size - 1.0))
                    .font_family("JetBrains Mono")
                    .child(format!("+{added}")),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(hex(RED))
                    .text_size(px(code_size - 1.0))
                    .font_family("JetBrains Mono")
                    .child(format!("−{removed}")),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    this.document.toggle_collapsed(block_id);
                    cx.notify();
                }),
            ),
    );

    // +/- body hidden when collapsed — header + chevron stay visible so
    // the user still sees the file path and an affordance to expand.
    if collapsed {
        return diff;
    }

    // Old lines (red). `flex_1().min_w_0()` on the line text lets it
    // wrap at the card width instead of pushing the row past the
    // viewport. Monospace + ligatures-off matches the terminal feel.
    for line in old_string.lines() {
        diff = diff.child(
            div()
                .w_full()
                .min_w_0()
                .px(px(10.0))
                .py(px(1.0))
                .bg(hex_alpha(RED, 0.1))
                .flex()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(hex(RED))
                        .text_size(px(code_size))
                        .font_family("JetBrains Mono")
                        .child("-"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(hex_alpha(RED, 0.8))
                        .text_size(px(code_size))
                        .font_family("JetBrains Mono")
                        .child(line.to_string()),
                ),
        );
    }

    // New lines (green)
    for line in new_string.lines() {
        diff = diff.child(
            div()
                .w_full()
                .min_w_0()
                .px(px(10.0))
                .py(px(1.0))
                .bg(hex_alpha(GREEN, 0.1))
                .flex()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(hex(GREEN))
                        .text_size(px(code_size))
                        .font_family("JetBrains Mono")
                        .child("+"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(hex_alpha(GREEN, 0.8))
                        .text_size(px(code_size))
                        .font_family("JetBrains Mono")
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
    _num_turns: u32,
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
        .w_full()
        .min_w_0()
        .mt(px(4.0))
        .mb(px(4.0))
        .px(px(6.0))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(color)
                        .text_size(px(font_size - 2.0))
                        .child(label),
                )
                .child(
                    div()
                        .flex_shrink_0()
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
                    .w_full()
                    .min_w_0()
                    .mt(px(6.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(hex_alpha(SURFACE0, 0.4))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
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
        .w_full()
        .min_w_0()
        .my(px(6.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(hex_alpha(BLUE, 0.08))
        .border_l_2()
        .border_color(hex_alpha(BLUE, 0.5))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .gap(px(8.0))
                .items_start()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(hex(BLUE))
                        .text_size(px(font_size - 1.0))
                        .font_weight(FontWeight::BOLD)
                        .child("You"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
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

