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

use crate::theme::{theme, with_alpha};
use gpui::*;
use similar::{ChangeTag, TextDiff};

use super::compose_bar::{ComposeBar, ComposeBarEvent};
use super::document::{
    short_path, truncate_to_char_boundary, Block, BlockId, BlockKind, RichDocument,
};
use super::narrative::{Annotation, LocusPhase, NarrativeRole};
use super::permissions::{PermissionAction, PermissionRequest, RiskLevel};
use super::reader::{NarrativeIndex, NavCounts};
use crate::stream::RichEvent;

// ── Catppuccin Mocha palette (matching terminal) ──────────────────

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
    /// User clicked "Allow" on a permission request block. The parent
    /// should send Enter to the session's PTY to approve the tool call.
    AllowPermission,
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
    /// Navigation index over the narrative (DEV-31): powers the jump strip
    /// and (later) search. Populated as blocks are added.
    index: NarrativeIndex,
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
        tool_visibility: std::collections::HashMap<String, bool>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let compose_bar = cx.new(|cx| ComposeBar::new(cx, font_size, session_id.clone()));

        // Re-emit compose bar submits upward so the parent can route to
        // the PTY. The view never writes to `claude`'s stdin directly.
        cx.subscribe(
            &compose_bar,
            |_this: &mut Self, _bar, event: &ComposeBarEvent, cx| match event {
                ComposeBarEvent::Submit { text, attachments } => {
                    cx.emit(RichViewEvent::Submit {
                        text: text.clone(),
                        attachments: attachments.clone(),
                    });
                }
            },
        )
        .detach();

        let list_state = ListState::new(0, ListAlignment::Bottom, px(200.0));

        let mut document = RichDocument::new();
        document.set_tool_visibility(tool_visibility);

        Self {
            focus_handle,
            document,
            compose_bar,
            font_size,
            busy: false,
            list_state,
            index: NarrativeIndex::new(),
        }
    }

    /// Record a freshly-created block into the navigation index (DEV-31).
    fn index_block(&mut self, id: BlockId) {
        let Some((text, artifact, annotation)) =
            self.document.blocks().iter().find(|b| b.id == id).map(|b| {
                let (text, artifact) = block_index_fields(&b.kind);
                (text, artifact, self.document.annotation(id).cloned())
            })
        else {
            return;
        };
        if let Some(annotation) = annotation {
            self.index
                .record(id, &annotation, &text, artifact.as_deref());
        }
    }

    /// Counts of navigable points, for the navigation strip.
    #[allow(dead_code)]
    pub fn nav_counts(&self) -> NavCounts {
        self.index.counts()
    }

    /// Apply one event from the transcript tailer. The caller is
    /// responsible for watching `~/.claude/projects/<cwd>/<session>.jsonl`
    /// (+ `subagents/*.jsonl`) and feeding each parsed event in order.
    pub fn apply_event(&mut self, event: RichEvent, cx: &mut Context<Self>) {
        let old_count = self.document.block_count();
        let created = self.document.apply_event(event);
        if let Some(id) = created {
            self.index_block(id);
        }
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
        let prompt_id = self.document.push_user_prompt(text);
        self.index_block(prompt_id);
        self.document.push_awaiting_indicator();
        let new_count = self.document.block_count();
        self.sync_list_state(old_count, new_count);
        cx.notify();
    }

    /// Show a permission request in the transcript. Called by the parent
    /// when the hook system detects the session entered AwaitingInput with
    /// a permission prompt. Replaces any prior permission block.
    pub fn push_permission_request(
        &mut self,
        tool_name: Option<String>,
        summary: Option<String>,
        input: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        let old_count = self.document.block_count();
        self.document
            .push_permission_request(tool_name, summary, input);
        let new_count = self.document.block_count();
        self.sync_list_state(old_count, new_count);
        cx.notify();
    }

    /// Remove the permission request block. Called by the parent when
    /// the session leaves AwaitingInput.
    pub fn clear_permission_request(&mut self, cx: &mut Context<Self>) {
        if !self.document.has_permission_block() {
            return;
        }
        let old_count = self.document.block_count();
        self.document.clear_permission_request();
        let new_count = self.document.block_count();
        self.sync_list_state(old_count, new_count);
        cx.notify();
    }

    fn sync_list_state(&mut self, old_count: usize, new_count: usize) {
        if new_count > old_count {
            self.list_state
                .splice(old_count..old_count, new_count - old_count);
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
        let annotation = self.document.annotation(block.id);
        render_block(block, annotation, font_size, cx).into_any_element()
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
                    .rounded(px(6.0))
                    .bg(with_alpha(theme().text_primary, 0.25)),
            )
    }
}

impl Render for RichView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = self.font_size;
        let busy = self.busy;
        self.compose_bar
            .update(cx, |bar, cx| bar.set_busy(busy, cx));

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
                .bg(theme().bg_base)
                .child(
                    div().flex_1().flex().items_center().justify_center().child(
                        div()
                            .text_color(theme().text_secondary)
                            .text_size(px(font_size))
                            .child(message),
                    ),
                )
                .child(self.compose_bar.clone());
        }

        // Virtual list — only renders visible blocks + overdraw.
        let feed_list = list(self.list_state.clone(), cx.processor(Self::render_block_at))
            .with_sizing_behavior(ListSizingBehavior::Auto)
            .p(px(12.0))
            .size_full();

        let scrollbar = self.render_scrollbar();
        let nav_counts = self.index.counts();

        // `size_full()` is critical — see comment in previous version.
        // The internal flex-col distributes height between the feed
        // (`flex_1`) and compose bar (`flex_shrink_0`).
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .bg(theme().bg_base)
            // DEV-31 navigation strip: a compact tally of navigable points
            // (phases / decisions / errors / files) once any exist.
            .children(
                nav_counts
                    .any()
                    .then(|| render_nav_strip(nav_counts, font_size)),
            )
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

fn render_block(
    block: &Block,
    annotation: Option<&Annotation>,
    font_size: f32,
    cx: &mut Context<RichView>,
) -> Div {
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
    let mut wrapper = div().w_full().min_w_0().pl(indent).pb(px(16.0));

    // DEV-29 narrative emphasis: a left accent bar on salient roles
    // (decisions, outcomes) and a phase-divider pill above Locus phase
    // headers, so a reader's eye lands on the meaningful moments.
    let role = annotation.map(|a| &a.role);
    if let Some(color) = role.and_then(role_accent) {
        wrapper = wrapper
            .border_l_2()
            .border_color(color)
            .pl(indent + px(8.0));
    }
    if let Some(NarrativeRole::PhaseHeader(phase)) = role {
        wrapper = wrapper.child(phase_pill(*phase, font_size));
    }

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
        BlockKind::PermissionRequest {
            tool_name,
            summary,
            input,
        } => {
            wrapper = wrapper.child(render_permission_request(
                tool_name.as_deref(),
                summary.as_deref(),
                input.as_ref(),
                font_size,
                cx,
            ));
        }
    }

    wrapper
}

// ── Navigation strip (DEV-31) ─────────────────────────────────────

/// Extract (searchable text, artifact path) for the nav index from a block.
fn block_index_fields(kind: &BlockKind) -> (String, Option<String>) {
    match kind {
        BlockKind::Text { content, .. } => (content.clone(), None),
        BlockKind::Thinking { content } => (content.clone(), None),
        BlockKind::ToolCall {
            tool_name,
            input_summary,
            ..
        } => (format!("{tool_name} {input_summary}"), None),
        BlockKind::Diff { file_path, .. } => (file_path.clone(), Some(file_path.clone())),
        BlockKind::UserPrompt { content } => (content.clone(), None),
        BlockKind::SessionEnd { .. } => ("session complete".to_string(), None),
        _ => (String::new(), None),
    }
}

/// A compact tally of navigable points, shown above the feed.
fn render_nav_strip(counts: NavCounts, font_size: f32) -> Div {
    let chip = |n: usize, label: &str, color: Hsla| {
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .text_color(color)
                    .text_size(px(font_size - 3.0))
                    .font_weight(FontWeight::BOLD)
                    .child(format!("{n}")),
            )
            .child(
                div()
                    .text_color(theme().text_faint)
                    .text_size(px(font_size - 3.0))
                    .child(label.to_string()),
            )
    };

    let mut row = div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap(px(12.0))
        .items_center()
        .px(px(12.0))
        .py(px(4.0))
        .border_b_1()
        .border_color(with_alpha(theme().text_faint, 0.15))
        .bg(theme().bg_base);
    if counts.phases > 0 {
        row = row.child(chip(counts.phases, "phases", theme().ready));
    }
    if counts.decisions > 0 {
        row = row.child(chip(counts.decisions, "decisions", theme().attention));
    }
    if counts.outcomes > 0 {
        row = row.child(chip(counts.outcomes, "outcomes", theme().success));
    }
    if counts.errors > 0 {
        row = row.child(chip(counts.errors, "errors", theme().danger));
    }
    if counts.artifacts > 0 {
        row = row.child(chip(counts.artifacts, "files", theme().text_secondary));
    }
    row
}

// ── Narrative emphasis (DEV-29) ───────────────────────────────────

/// Accent colour for an emphasised narrative role, or `None` for roles that
/// need no left bar (prose, activity, and phase headers — the latter get a
/// pill instead).
fn role_accent(role: &NarrativeRole) -> Option<Hsla> {
    match role {
        NarrativeRole::Decision => Some(with_alpha(theme().attention, 0.85)),
        NarrativeRole::Outcome => Some(with_alpha(theme().success, 0.85)),
        _ => None,
    }
}

/// A small "LOCUS · PHASE" divider pill rendered above a phase-header block.
fn phase_pill(phase: LocusPhase, font_size: f32) -> Div {
    div()
        .mb(px(6.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(div().w(px(5.0)).h(px(5.0)).rounded_full().bg(theme().ready))
        .child(
            div()
                .text_color(theme().ready)
                .text_size(px(font_size - 2.0))
                .font_weight(FontWeight::BOLD)
                .child(format!("LOCUS · {}", phase.label())),
        )
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
        .my(px(12.0))
        .px(px(10.0))
        .py(px(6.0))
        .flex()
        .gap(px(8.0))
        .items_start()
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme().ready)
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

/// Chevron icon for collapsible blocks: right = collapsed, down = expanded.
fn chevron(collapsed: bool) -> gpui::Svg {
    crate::icon::icon(
        if collapsed {
            crate::icon::name::CHEVRON_RIGHT
        } else {
            crate::icon::name::CHEVRON_DOWN
        },
        10.0,
        theme().text_faint,
    )
}

fn render_thinking_block(
    block_id: super::document::BlockId,
    content: &str,
    collapsed: bool,
    font_size: f32,
    cx: &mut Context<RichView>,
) -> Div {
    let header = div()
        .id(ElementId::Name(
            format!("thinking-header-{block_id}").into(),
        ))
        .flex()
        .gap(px(6.0))
        .items_center()
        .cursor(gpui::CursorStyle::PointingHand)
        .child(
            div()
                .text_color(theme().text_faint)
                .text_size(px(font_size - 2.0))
                .child(chevron(collapsed)),
        )
        .child(
            div()
                .text_color(theme().text_faint)
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
        .border_color(with_alpha(theme().text_faint, 0.3))
        .child(header);

    if !collapsed {
        block = block.child(
            div()
                .w_full()
                .min_w_0()
                .mt(px(4.0))
                .text_color(with_alpha(theme().text_faint, 0.7))
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
        Some(r) if r.is_error => theme().danger,
        Some(_) => theme().success,
        None => theme().attention, // still running
    };

    let mut card = div()
        .w_full()
        .min_w_0()
        .px(px(10.0))
        .py(px(8.0))
        .rounded(px(6.0))
        .bg(with_alpha(theme().bg_raised, 0.6))
        .border_l_2()
        .border_color(status_color);

    // Header: chevron + tool name + summary (clickable).
    card = card.child(
        div()
            .id(ElementId::Name(
                format!("toolcall-header-{block_id}").into(),
            ))
            .w_full()
            .min_w_0()
            .flex()
            .gap(px(8.0))
            .items_center()
            .cursor(gpui::CursorStyle::PointingHand)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme().text_secondary)
                    .text_size(px(font_size - 2.0))
                    .child(chevron(collapsed)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme().accent)
                    .text_size(px(font_size - 1.0))
                    .font_weight(FontWeight::BOLD)
                    .child(tool_name.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(theme().text_secondary)
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

    if !collapsed {
        card = card.child(render_tool_expanded_input(tool_name, input_full, font_size));
    }

    if let Some(r) = result {
        if r.is_error {
            let cleaned = strip_ansi(&r.content);
            let preview = if cleaned.len() > 200 {
                format!("{}...", truncate_to_char_boundary(&cleaned, 197))
            } else {
                cleaned
            };
            card = card.child(
                div()
                    .w_full()
                    .min_w_0()
                    .mt(px(4.0))
                    .text_color(theme().danger)
                    .text_size(px(font_size - 1.0))
                    .child(preview),
            );
        } else if !collapsed && !r.content.trim().is_empty() {
            card = card.child(render_tool_result_output(&r.content, font_size));
        }
    }

    card
}

// ── Tool expanded input (smart per-tool formatting) ──────────────

fn render_tool_expanded_input(tool_name: &str, input: &serde_json::Value, font_size: f32) -> Div {
    let code_size = font_size - 2.0;
    match tool_name {
        "Bash" => {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let description = input.get("description").and_then(|v| v.as_str());
            let mut block = div().w_full().min_w_0().mt(px(6.0));
            if let Some(desc) = description {
                block = block.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .mb(px(4.0))
                        .text_color(theme().text_secondary)
                        .text_size(px(code_size))
                        .child(desc.to_string()),
                );
            }
            let mut code = div()
                .w_full()
                .min_w_0()
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(with_alpha(theme().bg_hover, 0.4))
                .text_color(theme().success)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO);
            for line in command.lines() {
                code = code.child(div().w_full().min_w_0().child(line.to_string()));
            }
            block.child(code)
        }
        "Read" | "read_file" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let offset = input.get("offset").and_then(|v| v.as_u64());
            let limit = input.get("limit").and_then(|v| v.as_u64());
            let mut detail = path.to_string();
            if let Some(o) = offset {
                detail.push_str(&format!(" (from line {o}"));
                if let Some(l) = limit {
                    detail.push_str(&format!(", {l} lines"));
                }
                detail.push(')');
            } else if let Some(l) = limit {
                detail.push_str(&format!(" ({l} lines)"));
            }
            div()
                .w_full()
                .min_w_0()
                .mt(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .bg(with_alpha(theme().bg_hover, 0.4))
                .text_color(theme().text_body)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO)
                .child(detail)
        }
        "Write" | "write_file" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let line_count = content.lines().count();
            div()
                .w_full()
                .min_w_0()
                .mt(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .bg(with_alpha(theme().bg_hover, 0.4))
                .text_color(theme().text_body)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO)
                .child(format!("{path} ({line_count} lines)"))
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|p| super::document::short_path(p))
                .unwrap_or_default();
            let include = input.get("include").and_then(|v| v.as_str());
            let mut detail = format!("/{pattern}/");
            if !path.is_empty() {
                detail.push_str(&format!(" in {path}"));
            }
            if let Some(inc) = include {
                detail.push_str(&format!(" ({inc})"));
            }
            div()
                .w_full()
                .min_w_0()
                .mt(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .bg(with_alpha(theme().bg_hover, 0.4))
                .text_color(theme().text_body)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO)
                .child(detail)
        }
        "Agent" => {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("subagent");
            let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            let mut block = div().w_full().min_w_0().mt(px(6.0));
            block = block.child(
                div()
                    .w_full()
                    .min_w_0()
                    .mb(px(4.0))
                    .text_color(theme().ready)
                    .text_size(px(code_size))
                    .font_weight(FontWeight::BOLD)
                    .child(desc.to_string()),
            );
            if !prompt.is_empty() {
                let preview = if prompt.len() > 500 {
                    format!("{}…", truncate_to_char_boundary(&prompt, 497))
                } else {
                    prompt.to_string()
                };
                block = block.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .bg(with_alpha(theme().bg_hover, 0.4))
                        .text_color(theme().text_secondary)
                        .text_size(px(code_size))
                        .child(preview),
                );
            }
            block
        }
        _ => {
            let pretty =
                serde_json::to_string_pretty(input).unwrap_or_else(|_| format!("{input:?}"));
            let mut json_block = div()
                .w_full()
                .min_w_0()
                .mt(px(6.0))
                .px(px(8.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .bg(with_alpha(theme().bg_hover, 0.4))
                .text_color(theme().text_body)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO);
            for line in pretty.lines().take(40) {
                json_block = json_block.child(div().w_full().min_w_0().child(line.to_string()));
            }
            let total_lines = pretty.lines().count();
            if total_lines > 40 {
                json_block = json_block.child(
                    div()
                        .mt(px(4.0))
                        .text_color(with_alpha(theme().text_faint, 0.7))
                        .child(format!("…{} more lines", total_lines - 40)),
                );
            }
            json_block
        }
    }
}

// ── Tool result output (shown when expanded) ─────────────────────

const MAX_RESULT_LINES: usize = 80;

fn render_tool_result_output(content: &str, font_size: f32) -> Div {
    let code_size = font_size - 2.0;
    let cleaned = strip_ansi(content);
    let lines: Vec<&str> = cleaned.lines().collect();
    let truncated = lines.len() > MAX_RESULT_LINES;
    let visible = if truncated {
        &lines[..MAX_RESULT_LINES]
    } else {
        &lines
    };

    let mut block = div()
        .w_full()
        .min_w_0()
        .mt(px(8.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .bg(with_alpha(theme().bg_hover, 0.3))
        .text_color(theme().text_secondary)
        .text_size(px(code_size))
        .font_family(crate::theme::FONT_MONO);

    for line in visible {
        block = block.child(div().w_full().min_w_0().child(line.to_string()));
    }

    if truncated {
        let remaining = lines.len() - MAX_RESULT_LINES;
        block = block.child(
            div()
                .mt(px(4.0))
                .text_color(with_alpha(theme().text_faint, 0.7))
                .text_size(px(code_size))
                .child(format!("…{remaining} more lines")),
        );
    }

    block
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

    let text_diff = TextDiff::from_lines(old_string, new_string);
    let mut added = 0usize;
    let mut removed = 0usize;
    for change in text_diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }

    let mut diff = div()
        .w_full()
        .min_w_0()
        .rounded(px(6.0))
        .bg(with_alpha(theme().bg_raised, 0.4))
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
            .bg(with_alpha(theme().bg_hover, 0.6))
            .cursor(gpui::CursorStyle::PointingHand)
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme().text_secondary)
                    .text_size(px(code_size - 1.0))
                    .child(chevron(collapsed)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme().attention)
                    .text_size(px(code_size))
                    .font_weight(FontWeight::BOLD)
                    .child("Edit"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(theme().text_body)
                    .text_size(px(code_size))
                    .font_family(crate::theme::FONT_MONO)
                    .child(short),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme().success)
                    .text_size(px(code_size - 1.0))
                    .font_family(crate::theme::FONT_MONO)
                    .child(format!("+{added}")),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme().danger)
                    .text_size(px(code_size - 1.0))
                    .font_family(crate::theme::FONT_MONO)
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

    if collapsed {
        return diff;
    }

    let text_diff = TextDiff::from_lines(old_string, new_string);
    let changes: Vec<_> = text_diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.value().trim_end_matches('\n').to_string()))
        .collect();

    let mut i = 0;
    while i < changes.len() {
        // Try to pair consecutive Delete+Insert runs for intraline highlighting.
        let del_start = i;
        while i < changes.len() && changes[i].0 == ChangeTag::Delete {
            i += 1;
        }
        let del_end = i;
        let ins_start = i;
        while i < changes.len() && changes[i].0 == ChangeTag::Insert {
            i += 1;
        }
        let ins_end = i;

        let del_count = del_end - del_start;
        let ins_count = ins_end - ins_start;

        if del_count > 0 || ins_count > 0 {
            // Hunk-level similarity: compare the entire deleted block
            // against the entire inserted block. If they're structurally
            // different, render all reds then all greens (no interleaving).
            let del_block: String = (del_start..del_end)
                .map(|k| changes[k].1.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let ins_block: String = (ins_start..ins_end)
                .map(|k| changes[k].1.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let hunk_ratio = strsim_ratio(&del_block, &ins_block);

            if hunk_ratio < 0.4 {
                // Structural replacement — group reds then greens.
                for j in del_start..del_end {
                    diff = diff.child(render_diff_line_plain(
                        "-",
                        &changes[j].1,
                        code_size,
                        with_alpha(theme().danger, 0.8),
                        with_alpha(theme().danger, 0.1),
                    ));
                }
                for j in ins_start..ins_end {
                    diff = diff.child(render_diff_line_plain(
                        "+",
                        &changes[j].1,
                        code_size,
                        with_alpha(theme().success, 0.8),
                        with_alpha(theme().success, 0.1),
                    ));
                }
            } else {
                // Similar hunk — interleave with per-line intraline.
                let paired = del_count.min(ins_count);
                for j in 0..paired {
                    let del_line = &changes[del_start + j].1;
                    let ins_line = &changes[ins_start + j].1;
                    let ratio = strsim_ratio(del_line, ins_line);
                    if ratio > 0.4 {
                        diff = diff.child(render_diff_line_intraline(
                            "-", del_line, ins_line, true, code_size,
                        ));
                        diff = diff.child(render_diff_line_intraline(
                            "+", ins_line, del_line, false, code_size,
                        ));
                    } else {
                        diff = diff.child(render_diff_line_plain(
                            "-",
                            del_line,
                            code_size,
                            with_alpha(theme().danger, 0.8),
                            with_alpha(theme().danger, 0.1),
                        ));
                        diff = diff.child(render_diff_line_plain(
                            "+",
                            ins_line,
                            code_size,
                            with_alpha(theme().success, 0.8),
                            with_alpha(theme().success, 0.1),
                        ));
                    }
                }
                for j in paired..del_count {
                    diff = diff.child(render_diff_line_plain(
                        "-",
                        &changes[del_start + j].1,
                        code_size,
                        with_alpha(theme().danger, 0.8),
                        with_alpha(theme().danger, 0.1),
                    ));
                }
                for j in paired..ins_count {
                    diff = diff.child(render_diff_line_plain(
                        "+",
                        &changes[ins_start + j].1,
                        code_size,
                        with_alpha(theme().success, 0.8),
                        with_alpha(theme().success, 0.1),
                    ));
                }
            }
            continue;
        }

        // Equal line.
        if i < changes.len() && changes[i].0 == ChangeTag::Equal {
            diff = diff.child(render_diff_line_plain(
                " ",
                &changes[i].1,
                code_size,
                with_alpha(theme().text_secondary, 0.5),
                with_alpha(theme().bg_raised, 0.0),
            ));
            i += 1;
        }
    }

    diff
}

fn strsim_ratio(a: &str, b: &str) -> f64 {
    let diff = TextDiff::from_words(a, b);
    let matching: usize = diff
        .iter_all_changes()
        .filter(|c| c.tag() == ChangeTag::Equal)
        .map(|c| c.value().len())
        .sum();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    (2 * matching) as f64 / total as f64
}

fn render_diff_line_plain(
    prefix: &'static str,
    text: &str,
    code_size: f32,
    text_color: Hsla,
    bg_color: Hsla,
) -> Div {
    div()
        .w_full()
        .min_w_0()
        .px(px(10.0))
        .py(px(1.0))
        .bg(bg_color)
        .flex()
        .gap(px(6.0))
        .child(
            div()
                .flex_shrink_0()
                .w(px(10.0))
                .text_color(text_color)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO)
                .child(prefix),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(text_color)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO)
                .child(text.to_string()),
        )
}

fn render_diff_line_intraline(
    prefix: &'static str,
    this_line: &str,
    other_line: &str,
    is_delete: bool,
    code_size: f32,
) -> Div {
    let (base_color, bg_color, highlight_bg) = if is_delete {
        (
            with_alpha(theme().danger, 0.8),
            with_alpha(theme().danger, 0.1),
            with_alpha(theme().danger, 0.3),
        )
    } else {
        (
            with_alpha(theme().success, 0.8),
            with_alpha(theme().success, 0.1),
            with_alpha(theme().success, 0.3),
        )
    };

    let word_diff = TextDiff::from_words(other_line, this_line);
    let mono = Font {
        family: crate::theme::FONT_MONO.into(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        features: FontFeatures::disable_ligatures(),
        fallbacks: None,
    };

    let mut full_text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();

    for change in word_diff.iter_all_changes() {
        let val = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                full_text.push_str(val);
                runs.push(TextRun {
                    len: val.len(),
                    font: mono.clone(),
                    color: base_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                });
            }
            ChangeTag::Insert => {
                full_text.push_str(val);
                runs.push(TextRun {
                    len: val.len(),
                    font: mono.clone(),
                    color: base_color,
                    background_color: Some(highlight_bg),
                    underline: None,
                    strikethrough: None,
                });
            }
            ChangeTag::Delete => {
                // Words only in the other line — skip for this line's render.
            }
        }
    }

    // Merge adjacent runs with same styling to reduce render overhead.
    let mut merged: Vec<TextRun> = Vec::new();
    for run in runs {
        if let Some(last) = merged.last_mut() {
            if last.background_color == run.background_color {
                last.len += run.len;
                continue;
            }
        }
        merged.push(run);
    }

    let styled = StyledText::new(SharedString::from(full_text)).with_runs(merged);

    div()
        .w_full()
        .min_w_0()
        .px(px(10.0))
        .py(px(1.0))
        .bg(bg_color)
        .flex()
        .gap(px(6.0))
        .child(
            div()
                .flex_shrink_0()
                .w(px(10.0))
                .text_color(base_color)
                .text_size(px(code_size))
                .font_family(crate::theme::FONT_MONO)
                .child(prefix),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(code_size))
                .child(styled),
        )
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
    let color = if is_error {
        theme().danger
    } else {
        theme().teal
    };
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
                        .text_color(with_alpha(theme().text_secondary, 0.7))
                        .text_size(px(font_size - 2.0))
                        .child(format!("{duration} · ${cost_usd:.4}")),
                ),
        );

    // Show error text if available
    if let Some(text) = result_text {
        if !text.is_empty() {
            let preview = if text.len() > 500 {
                format!("{}...", truncate_to_char_boundary(&text, 497))
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
                    .rounded(px(6.0))
                    .bg(with_alpha(theme().bg_raised, 0.4))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_color(if is_error {
                                theme().danger
                            } else {
                                theme().text_secondary
                            })
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
        .rounded(px(6.0))
        .bg(with_alpha(theme().accent, 0.08))
        .border_l_2()
        .border_color(with_alpha(theme().accent, 0.5))
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
                        .text_color(theme().accent)
                        .text_size(px(font_size - 1.0))
                        .font_weight(FontWeight::BOLD)
                        .child("You"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme().text_primary)
                        .text_size(px(font_size))
                        .child(content.to_string()),
                ),
        )
}

// ── Permission request ────────────────────────────────────────────

fn render_permission_request(
    tool_name: Option<&str>,
    summary: Option<&str>,
    input: Option<&serde_json::Value>,
    font_size: f32,
    cx: &mut Context<RichView>,
) -> Div {
    // DEV-34: build the normalized request so the card can show purpose,
    // target, and an assessed risk level rather than a bare tool name.
    let request = tool_name
        .map(|name| PermissionRequest::from_tool(name, input.unwrap_or(&serde_json::Value::Null)));

    let label = match tool_name {
        Some(name) => format!("{name} wants permission"),
        None => "Permission requested".to_string(),
    };

    let mut card = div()
        .w_full()
        .min_w_0()
        .my(px(8.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(6.0))
        .bg(with_alpha(theme().attention, 0.1))
        .border_l_2()
        .border_color(theme().attention);

    // Header row: icon + tool label + risk badge
    card = card.child(
        div()
            .w_full()
            .min_w_0()
            .flex()
            .gap(px(8.0))
            .items_center()
            .child(crate::icon::icon(
                crate::icon::name::PAUSE,
                font_size,
                theme().attention,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(theme().attention)
                    .text_size(px(font_size))
                    .font_weight(FontWeight::BOLD)
                    .child(label),
            )
            .children(request.as_ref().map(|r| risk_badge(r.risk, font_size))),
    );

    // Purpose line ("run a shell command", "modify a file", …).
    if let Some(req) = request.as_ref() {
        card = card.child(
            div()
                .w_full()
                .min_w_0()
                .mt(px(4.0))
                .pl(px(24.0))
                .text_color(theme().text_secondary)
                .text_size(px(font_size - 1.0))
                .child(req.purpose.clone()),
        );
    }

    // Summary line (command, file path, etc.)
    if let Some(text) = summary {
        card = card.child(
            div()
                .w_full()
                .min_w_0()
                .mt(px(4.0))
                .pl(px(24.0))
                .text_color(theme().text_body)
                .text_size(px(font_size - 1.0))
                .font_family(crate::theme::FONT_MONO)
                .child(text.to_string()),
        );
    }

    // Allow button
    card = card.child(
        div().w_full().min_w_0().mt(px(8.0)).pl(px(24.0)).child(
            div()
                .id("permission-allow-btn")
                .px(px(12.0))
                .py(px(4.0))
                .rounded(px(6.0))
                .bg(with_alpha(theme().success, 0.15))
                .text_color(theme().success)
                .text_size(px(font_size - 1.0))
                .font_weight(FontWeight::BOLD)
                .cursor(gpui::CursorStyle::PointingHand)
                .child("Allow")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        // DEV-34: retain the decision before resolving.
                        this.document
                            .record_permission_decision(PermissionAction::Allow);
                        cx.emit(RichViewEvent::AllowPermission);
                    }),
                ),
        ),
    );

    card
}

/// A small coloured risk badge ("LOW" / "MEDIUM" / "HIGH") for a permission
/// card (DEV-34).
fn risk_badge(risk: RiskLevel, font_size: f32) -> Div {
    let color = match risk {
        RiskLevel::Low => theme().success,
        RiskLevel::Medium => theme().attention,
        RiskLevel::High => theme().danger,
    };
    div()
        .flex_shrink_0()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(with_alpha(color, 0.15))
        .text_color(color)
        .text_size(px(font_size - 3.0))
        .font_weight(FontWeight::BOLD)
        .child(risk.label().to_uppercase())
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
        .child(crate::icon::icon(
            crate::icon::name::CIRCLE_FILL,
            font_size - 4.0,
            theme().attention,
        ))
        .child(
            div()
                .text_color(theme().text_secondary)
                .text_size(px(font_size - 1.0))
                .child("Thinking…"),
        )
}

// ── ANSI escape stripping ────────────────────────────────────────

/// Strip ANSI escape sequences (CSI, OSC, simple ESC) from text.
/// Tool output (especially Bash/test runners) contains colour codes
/// that the transcript view should display as plain text.
fn strip_ansi(input: &str) -> String {
    if !input.contains('\x1b') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.clone().next() {
                Some('[') => {
                    chars.next(); // consume '['
                                  // CSI: parameter bytes (0x30–0x3F), intermediate (0x20–0x2F), final (0x40–0x7E)
                    loop {
                        match chars.next() {
                            Some(ch) if ('@'..='~').contains(&ch) => break,
                            None => break,
                            _ => {}
                        }
                    }
                }
                Some(']') => {
                    chars.next(); // consume ']'
                                  // OSC: consume until BEL or ST (ESC \)
                    loop {
                        match chars.next() {
                            Some('\x07') => break,
                            Some('\x1b') => {
                                chars.next();
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                } // two-byte ESC sequence
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strip_sgr_codes() {
        let input = "\x1b[90mTests:\x1b[39m \x1b[33;1m2 deprecated\x1b[39;22m";
        assert_eq!(strip_ansi(input), "Tests: 2 deprecated");
    }

    #[test]
    fn strip_cursor_movement() {
        assert_eq!(
            strip_ansi("\x1b[1A\x1b[90mParallel:\x1b[39m 16"),
            "Parallel: 16"
        );
    }

    #[test]
    fn passthrough_clean_text() {
        let clean = "no escapes here";
        assert_eq!(strip_ansi(clean), clean);
    }

    #[test]
    fn preserves_utf8() {
        let input = "\x1b[32m✓ passed\x1b[0m — done";
        assert_eq!(strip_ansi(input), "✓ passed — done");
    }
}
