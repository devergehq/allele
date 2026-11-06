//! Document model — the stateful tree that RichEvents mutate and the renderer reads.
//!
//! Inspired by observability-trace models: each block is a "span" with an optional
//! parent (for subagent nesting). The tree is append-heavy with rare in-place updates
//! (status changes on existing nodes).

use crate::rich::narrative::{Annotation, NarrativeProjector};
use crate::rich::permissions::{DecisionLog, PermissionAction, PermissionRequest};
use crate::rich::tool_rail::{classify_tool, default_collapsed, RoutineRailSummary};
use crate::stream::{NoticeKind, RichEvent};
use std::collections::HashMap;

/// Unique identifier for a block in the document.
pub type BlockId = usize;

/// A single block in the activity feed.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    /// If this block belongs to a subagent, the tool_use_id of the Agent call.
    pub parent_agent_id: Option<String>,
    /// Visual state — collapsed, expanded, etc.
    pub collapsed: bool,
    /// Cached layout height in pixels (set during render, used for virtual scroll).
    pub cached_height: Option<f32>,
}

/// The content variants — each gets a distinct visual treatment.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum BlockKind {
    /// Streaming or complete text from the assistant.
    Text {
        content: String,
        /// True if content is still being appended (streaming).
        streaming: bool,
    },

    /// Thinking/reasoning block — lightest visual weight.
    Thinking { content: String },

    /// A tool call with structured input.
    ToolCall {
        tool_use_id: String,
        tool_name: String,
        input_summary: String,
        /// Full input JSON for expansion.
        input_full: serde_json::Value,
        /// Result once available.
        result: Option<ToolCallResult>,
    },

    /// Specialised: Edit tool call rendered as a diff.
    Diff {
        tool_use_id: String,
        file_path: String,
        old_string: String,
        new_string: String,
        /// Result once available.
        result: Option<ToolCallResult>,
    },

    /// Session completed.
    SessionEnd {
        duration_ms: u64,
        cost_usd: f64,
        num_turns: u32,
        is_error: bool,
        result_text: Option<String>,
    },

    /// A prompt the user submitted (echoed into the feed on submit).
    UserPrompt { content: String },

    /// A compact one-line session annotation — PR opened, artifact published,
    /// file edited outside Claude, plan accepted, hook error (DEV-321).
    Notice {
        kind: NoticeKind,
        content: String,
        link: Option<String>,
    },

    /// Transient "thinking" indicator shown while the CLI is processing
    /// but hasn't produced any output blocks yet. Removed when the first
    /// real block arrives or when the session ends.
    AwaitingResponse,

    /// Permission prompt — Claude is blocked waiting for user approval.
    /// Injected by the parent when the hook system detects AwaitingInput,
    /// removed when the session transitions out of that state.
    PermissionRequest {
        tool_name: Option<String>,
        summary: Option<String>,
        /// Raw tool input, when known — drives the risk/purpose card (DEV-34).
        input: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub content: String,
    pub is_error: bool,
}

/// The document model — an append-only list of blocks with index lookups.
pub struct RichDocument {
    blocks: Vec<Block>,
    /// Map from tool_use_id → block index, for attaching results to calls.
    tool_use_index: HashMap<String, BlockId>,
    /// Current text block being streamed into (if any).
    current_text_block: Option<BlockId>,
    /// Index of the active AwaitingResponse placeholder block (if any).
    awaiting_block: Option<BlockId>,
    /// Index of the active PermissionRequest block (if any).
    permission_block: Option<BlockId>,
    next_id: BlockId,
    /// Streaming narrative projector (DEV-29) — recognises Locus phases,
    /// conversational turns, and narrative roles as events arrive.
    projector: NarrativeProjector,
    /// Per-block narrative annotation, keyed by stable BlockId.
    annotations: HashMap<BlockId, Annotation>,
    /// Retained history of resolved permission decisions (DEV-34).
    decisions: DecisionLog,
    /// Per-tool rail visibility overrides (DEV-81), keyed by tool name.
    /// `true` = force collapsed, `false` = force expanded.
    tool_visibility: HashMap<String, bool>,
}

impl RichDocument {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            tool_use_index: HashMap::new(),
            current_text_block: None,
            awaiting_block: None,
            permission_block: None,
            next_id: 0,
            projector: NarrativeProjector::new(),
            annotations: HashMap::new(),
            decisions: DecisionLog::new(),
            tool_visibility: HashMap::new(),
        }
    }

    /// Set the per-tool rail visibility overrides (DEV-81). Applied to tool
    /// calls created after this point.
    pub fn set_tool_visibility(&mut self, overrides: HashMap<String, bool>) {
        self.tool_visibility = overrides;
    }

    /// Narrative annotation for a block, if one was recorded.
    /// The routine-tool run containing `ix`, as `(start, len)` (DEV-575).
    ///
    /// `None` when this block is not part of a run worth aggregating. Most
    /// tool calls in a session are routine reads and shell probes —
    /// individually low-signal, collectively loud enough to bury the prose
    /// that carries the turn's meaning. A run of them collapses to one line.
    ///
    /// Same grouping shape as [`RichDocument::run_len_at`] over a different
    /// predicate, deliberately: one mechanism for "these adjacent blocks draw
    /// as one thing", not two.
    ///
    /// A run of one is not a run. Aggregating a single call into "1 read"
    /// costs a click and saves nothing.
    pub fn rail_run_at(&self, ix: usize) -> Option<(usize, usize)> {
        if !self.rails_at(ix) {
            return None;
        }
        let mut start = ix;
        while start > 0 && self.rails_with(start - 1, start) {
            start -= 1;
        }
        let mut end = ix + 1;
        while end < self.blocks.len() && self.rails_with(end - 1, end) {
            end += 1;
        }
        (end - start > 1).then_some((start, end - start))
    }

    /// Whether the block at `ix` can join a rail: a routine tool call that has
    /// not errored.
    ///
    /// An error leaves the rail the moment its result lands. Grouping is
    /// recomputed from current state on every render, so that happens on its
    /// own — a failure is never hidden behind a summary line.
    fn rails_at(&self, ix: usize) -> bool {
        let Some(block) = self.blocks.get(ix) else {
            return false;
        };
        let BlockKind::ToolCall {
            tool_name, result, ..
        } = &block.kind
        else {
            return false;
        };
        if result.as_ref().is_some_and(|r| r.is_error) {
            return false;
        }
        classify_tool(tool_name).is_routine()
    }

    /// Whether two adjacent blocks belong to the same rail run: both railable,
    /// same turn, same owning agent.
    fn rails_with(&self, a: usize, b: usize) -> bool {
        if !self.rails_at(a) || !self.rails_at(b) {
            return false;
        }
        let (Some(ba), Some(bb)) = (self.blocks.get(a), self.blocks.get(b)) else {
            return false;
        };
        if ba.parent_agent_id != bb.parent_agent_id {
            return false;
        }
        self.annotation(ba.id).map(|a| a.turn) == self.annotation(bb.id).map(|a| a.turn)
    }

    /// One-line summary of the rail run at `start`, e.g.
    /// "6 reads, 2 shell · parser.rs, ledger.rs, …", with the number of calls
    /// it stands for.
    ///
    /// The count comes from the summary rather than the run length so the
    /// number the reader sees is the number of calls actually folded in.
    pub fn rail_summary(&self, start: usize, len: usize) -> (String, u32) {
        let mut summary = RoutineRailSummary::new();
        for block in self.blocks.iter().skip(start).take(len) {
            if let BlockKind::ToolCall {
                tool_name,
                input_summary,
                ..
            } = &block.kind
            {
                let target = (!input_summary.trim().is_empty()).then_some(input_summary.as_str());
                summary.record(tool_name, target);
            }
        }
        (summary.headline(3), summary.total())
    }

    /// How many blocks starting at `ix` render as a single unit (DEV-574).
    ///
    /// `1` for anything that renders on its own. `0` means this block was
    /// already absorbed by a run that started earlier, so the renderer should
    /// emit nothing for it. Greater than 1 is a run of settled text blocks
    /// that belong to one turn and render as one markdown document.
    ///
    /// Merging is a *view* concern, which is why this reports a grouping
    /// rather than rewriting the blocks. The ledger, the jump index and the
    /// search index all address the same blocks they always did, and a run
    /// that is still growing does not have to rewrite history as it streams.
    ///
    /// A run only ever covers **adjacent** text blocks. That is what makes it
    /// safe: merging across an intervening tool call would move prose that
    /// came after the call to before it.
    pub fn run_len_at(&self, ix: usize) -> usize {
        if !self.merges_at(ix) {
            return 1;
        }
        if ix > 0 && self.merges_with(ix - 1, ix) {
            return 0; // absorbed by the run that started earlier
        }
        let mut end = ix + 1;
        while end < self.blocks.len() && self.merges_with(end - 1, end) {
            end += 1;
        }
        end - ix
    }

    /// Whether the block at `ix` is the kind that can join a run: settled
    /// prose. A streaming block never merges — it changes on every token, and
    /// absorbing it would re-measure the whole run each time (DEV-574).
    fn merges_at(&self, ix: usize) -> bool {
        matches!(
            self.blocks.get(ix).map(|b| &b.kind),
            Some(BlockKind::Text {
                streaming: false,
                ..
            })
        )
    }

    /// Whether two adjacent blocks belong to the same run: both settled prose,
    /// same conversational turn, same owning agent.
    fn merges_with(&self, a: usize, b: usize) -> bool {
        if !self.merges_at(a) || !self.merges_at(b) {
            return false;
        }
        let (Some(ba), Some(bb)) = (self.blocks.get(a), self.blocks.get(b)) else {
            return false;
        };
        if ba.parent_agent_id != bb.parent_agent_id {
            return false;
        }
        self.annotation(ba.id).map(|a| a.turn) == self.annotation(bb.id).map(|a| a.turn)
    }

    /// The markdown source for the run starting at `ix`, joined so that a
    /// construct split across blocks — a heading in one and its paragraph in
    /// the next — parses as one document with one spacing context.
    ///
    /// A blank line is the separator because it is the one join that cannot
    /// fuse two block-level constructs into a single one.
    pub fn run_content(&self, ix: usize, len: usize) -> String {
        self.blocks
            .iter()
            .skip(ix)
            .take(len)
            .filter_map(|b| match &b.kind {
                BlockKind::Text { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Whether the block at `ix` is the first prose of its turn, and so the
    /// one place that turn's speaker should be named (DEV-574).
    ///
    /// One label per turn, not one per block: a turn emits several text blocks
    /// and a label on each stuttered down the left edge, which is what DEV-572
    /// removed. But removing it entirely left a block reached by search with no
    /// visible owner at all, so it comes back here — once, where the turn does.
    pub fn starts_turn_prose(&self, ix: usize) -> bool {
        let Some(block) = self.blocks.get(ix) else {
            return false;
        };
        if !matches!(block.kind, BlockKind::Text { .. }) {
            return false;
        }
        if block.parent_agent_id.is_some() {
            // Subagents are attributed once per run by their own header.
            return false;
        }
        let turn = self.annotation(block.id).map(|a| a.turn);
        !self.blocks[..ix].iter().rev().any(|earlier| {
            let same_turn = self.annotation(earlier.id).map(|a| a.turn) == turn;
            same_turn
                && earlier.parent_agent_id.is_none()
                && matches!(earlier.kind, BlockKind::Text { .. })
        })
    }

    pub fn annotation(&self, id: BlockId) -> Option<&Annotation> {
        self.annotations.get(&id)
    }

    /// Append a UserPrompt block (echoed when the user submits via ComposeBar).
    ///
    /// If an AwaitingResponse indicator exists it is moved to AFTER the new
    /// prompt so it always sits at the tail of the block list. This keeps
    /// `sync_list_state` splices position-accurate: when the awaiting block
    /// is later cleared it is always the last item, so the height accounting
    /// in GPUI's virtual list stays correct. Incorrect positions cause the
    /// viewport's total-height estimate to be wrong, which hides new content
    /// behind a scroll-track that's shorter than the actual content.
    pub fn push_user_prompt(&mut self, content: String) -> BlockId {
        // Temporarily remove any awaiting indicator so it ends up AFTER
        // the new prompt (always the last block in the list).
        let had_awaiting = self.awaiting_block.is_some();
        if had_awaiting {
            self.clear_awaiting_indicator();
        }
        self.close_text_stream();
        let annotation = self.projector.on_user_prompt();
        let id = self.push_block(Block {
            id: self.next_id,
            kind: BlockKind::UserPrompt { content },
            parent_agent_id: None,
            collapsed: false,
            cached_height: None,
        });
        self.annotations.insert(id, annotation);
        if had_awaiting {
            // Re-add at the end so awaiting is always the tail block.
            let awaiting_id = self.push_block(Block {
                id: self.next_id,
                kind: BlockKind::AwaitingResponse,
                parent_agent_id: None,
                collapsed: false,
                cached_height: None,
            });
            self.awaiting_block = Some(awaiting_id);
        }
        id
    }

    /// Show the "thinking" indicator while waiting for the CLI to produce output.
    /// No-op if one is already shown.
    pub fn push_awaiting_indicator(&mut self) {
        if self.awaiting_block.is_some() {
            return;
        }
        let id = self.push_block(Block {
            id: self.next_id,
            kind: BlockKind::AwaitingResponse,
            parent_agent_id: None,
            collapsed: false,
            cached_height: None,
        });
        self.awaiting_block = Some(id);
    }

    /// Remove the "thinking" indicator (call when first real output arrives).
    pub fn clear_awaiting_indicator(&mut self) {
        if let Some(id) = self.awaiting_block.take() {
            if let Some(pos) = self.blocks.iter().position(|b| b.id == id) {
                self.blocks.remove(pos);
            }
        }
    }

    /// Show a permission request block (Claude is blocked on a prompt).
    /// Replaces any existing permission block. Always positioned at the
    /// tail of the block list so it's immediately visible.
    pub fn push_permission_request(
        &mut self,
        tool_name: Option<String>,
        summary: Option<String>,
        input: Option<serde_json::Value>,
    ) -> BlockId {
        self.clear_permission_request();
        let id = self.push_block(Block {
            id: self.next_id,
            kind: BlockKind::PermissionRequest {
                tool_name,
                summary,
                input,
            },
            parent_agent_id: None,
            collapsed: false,
            cached_height: None,
        });
        self.permission_block = Some(id);
        id
    }

    /// Record the user's decision on the active permission prompt into the
    /// durable decision log (DEV-34). No-op if there is no active prompt or it
    /// carries no tool name.
    pub fn record_permission_decision(&mut self, action: PermissionAction) {
        let built = self.permission_block.and_then(|id| {
            let block = self.blocks.iter().find(|b| b.id == id)?;
            if let BlockKind::PermissionRequest {
                tool_name: Some(name),
                input,
                ..
            } = &block.kind
            {
                let value = input.clone().unwrap_or(serde_json::Value::Null);
                Some((PermissionRequest::from_tool(name, &value), id))
            } else {
                None
            }
        });
        if let Some((request, id)) = built {
            self.decisions.record(request, action, id);
        }
    }

    /// The retained permission decision log. Consumed by the audit UI
    /// (follow-up); retained here so the history survives prompt dismissal.
    #[allow(dead_code)]
    pub fn decision_log(&self) -> &DecisionLog {
        &self.decisions
    }

    /// Remove the permission request block (session left AwaitingInput).
    pub fn clear_permission_request(&mut self) {
        if let Some(id) = self.permission_block.take() {
            if let Some(pos) = self.blocks.iter().position(|b| b.id == id) {
                self.blocks.remove(pos);
            }
        }
    }

    pub fn has_permission_block(&self) -> bool {
        self.permission_block.is_some()
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Look up a block by its stable id.
    ///
    /// `BlockId` is a monotonic counter assigned at creation; it is NOT a
    /// vec index. Any `remove()` on `self.blocks` (e.g. clearing the
    /// awaiting-indicator) shifts indices but leaves ids untouched, so
    /// `blocks.get(id)` returns the wrong block. Always route id-based
    /// lookups through these helpers.
    fn index_of(&self, id: BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id == id)
    }

    fn block_mut_by_id(&mut self, id: BlockId) -> Option<&mut Block> {
        let idx = self.index_of(id)?;
        self.blocks.get_mut(idx)
    }

    /// Apply a RichEvent to the document, mutating in place.
    /// Returns the index of any newly created block (for scroll-to-bottom).
    pub fn apply_event(&mut self, event: RichEvent) -> Option<BlockId> {
        // Advance the narrative projection (phase/turn/role) for this event
        // before it is consumed by the match below (DEV-29).
        let annotation = self.projector.on_event(&event);

        // Any incoming content event means the CLI is producing output —
        // clear the "thinking" indicator.
        match &event {
            RichEvent::TextDelta { .. }
            | RichEvent::TextBlock { .. }
            | RichEvent::ThinkingBlock { .. }
            | RichEvent::ToolUse { .. }
            | RichEvent::ToolResult { .. }
            | RichEvent::EditDiff { .. }
            | RichEvent::SessionResult { .. } => {
                self.clear_awaiting_indicator();
            }
            _ => {}
        }

        let created = match event {
            RichEvent::TextDelta {
                text,
                parent_agent_id,
            } => {
                // Append to current streaming text block, or create one
                if let Some(block_id) = self.current_text_block {
                    if let Some(block) = self.block_mut_by_id(block_id) {
                        if let BlockKind::Text { content, .. } = &mut block.kind {
                            content.push_str(&text);
                            block.cached_height = None; // invalidate
                            return None; // no new block
                        }
                    }
                }
                // Create new streaming text block
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::Text {
                        content: text,
                        streaming: true,
                    },
                    parent_agent_id,
                    collapsed: false,
                    cached_height: None,
                });
                self.current_text_block = Some(id);
                Some(id)
            }

            RichEvent::TextBlock {
                text,
                parent_agent_id,
            } => {
                // Complete text block — close any streaming block first
                self.close_text_stream();
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::Text {
                        content: text,
                        streaming: false,
                    },
                    parent_agent_id,
                    collapsed: false,
                    cached_height: None,
                });
                Some(id)
            }

            RichEvent::ThinkingBlock {
                thinking,
                parent_agent_id,
            } => {
                self.close_text_stream();
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::Thinking { content: thinking },
                    parent_agent_id,
                    collapsed: true, // collapsed by default
                    cached_height: None,
                });
                Some(id)
            }

            RichEvent::ToolUse {
                tool_use_id,
                tool_name,
                input,
                parent_agent_id,
            } => {
                self.close_text_stream();
                let summary = summarise_tool_input(&tool_name, &input);
                // Routine reads/searches/shell collapse into the rail; mutations
                // and notable calls stay expanded (DEV-35). Classify before the
                // name is moved into the block.
                // DEV-81: a per-tool visibility override wins over the
                // routine/mutation default (errored results still auto-expand
                // later, in the ToolResult arm).
                let collapsed = self
                    .tool_visibility
                    .get(&tool_name)
                    .copied()
                    .unwrap_or_else(|| default_collapsed(classify_tool(&tool_name), false));
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::ToolCall {
                        tool_use_id: tool_use_id.clone(),
                        tool_name,
                        input_summary: summary,
                        input_full: input,
                        result: None,
                    },
                    parent_agent_id,
                    collapsed,
                    cached_height: None,
                });
                self.tool_use_index.insert(tool_use_id, id);
                Some(id)
            }

            RichEvent::EditDiff {
                tool_use_id,
                file_path,
                old_string,
                new_string,
                parent_agent_id,
            } => {
                self.close_text_stream();
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::Diff {
                        tool_use_id: tool_use_id.clone(),
                        file_path,
                        old_string,
                        new_string,
                        result: None,
                    },
                    parent_agent_id,
                    // Collapsed by default — header shows path + line deltas;
                    // click expands to the old/new body. A noisy edit turn
                    // should look like a short list of file names, not a
                    // wall of coloured lines.
                    collapsed: true,
                    cached_height: None,
                });
                self.tool_use_index.insert(tool_use_id, id);
                Some(id)
            }

            RichEvent::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                // Attach result to existing tool call block
                if let Some(&block_id) = self.tool_use_index.get(&tool_use_id) {
                    if let Some(block) = self.block_mut_by_id(block_id) {
                        let result = ToolCallResult { content, is_error };
                        match &mut block.kind {
                            BlockKind::ToolCall { result: r, .. } => *r = Some(result),
                            BlockKind::Diff { result: r, .. } => *r = Some(result),
                            _ => {}
                        }
                        // Auto-expand failures so an error is never hidden in
                        // the collapsed routine rail (DEV-35).
                        if is_error {
                            block.collapsed = false;
                        }
                        block.cached_height = None; // invalidate
                    }
                }
                None
            }

            RichEvent::SessionResult {
                duration_ms,
                cost_usd,
                num_turns,
                is_error,
                result_text,
            } => {
                self.close_text_stream();
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::SessionEnd {
                        duration_ms,
                        cost_usd,
                        num_turns,
                        is_error,
                        result_text,
                    },
                    parent_agent_id: None,
                    collapsed: false,
                    cached_height: None,
                });
                Some(id)
            }

            RichEvent::Notice {
                kind,
                text,
                link,
                parent_agent_id,
            } => {
                self.close_text_stream();
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::Notice {
                        kind,
                        content: text,
                        link,
                    },
                    parent_agent_id,
                    collapsed: false,
                    cached_height: None,
                });
                Some(id)
            }

            RichEvent::Fallback {
                raw,
                reason,
                parent_agent_id,
            } => {
                // An event we couldn't normalise. Render it collapsed so it
                // stays inspectable without cluttering the narrative — the
                // full raw payload is one click away.
                //
                // The Claude JSONL path no longer produces these for unknown
                // top-level types (DEV-321); they now come only from genuinely
                // corrupt lines and from other agent adapters, so the volume is
                // low enough for a raw dump to be the right call.
                self.close_text_stream();
                let id = self.push_block(Block {
                    id: self.next_id,
                    kind: BlockKind::Text {
                        content: format!("⚠ unsupported event ({reason})\n{raw}"),
                        streaming: false,
                    },
                    parent_agent_id,
                    collapsed: true,
                    cached_height: None,
                });
                Some(id)
            }

            RichEvent::Init { .. } | RichEvent::HookStatus { .. } => None,
        };

        // Attach the narrative annotation to whatever block this event created.
        if let Some(id) = created {
            self.annotations.insert(id, annotation);
        }
        created
    }

    /// Toggle collapsed state of a block.
    pub fn toggle_collapsed(&mut self, block_id: BlockId) {
        if let Some(block) = self.block_mut_by_id(block_id) {
            block.collapsed = !block.collapsed;
            block.cached_height = None;
        }
    }

    /// Invalidate all cached heights (e.g. on resize).
    #[allow(dead_code)]
    pub fn invalidate_heights(&mut self) {
        for block in &mut self.blocks {
            block.cached_height = None;
        }
    }

    // ── Private ───────────────────────────────────────────────────

    fn push_block(&mut self, mut block: Block) -> BlockId {
        let id = self.next_id;
        block.id = id;
        self.next_id += 1;
        self.blocks.push(block);
        id
    }

    fn close_text_stream(&mut self) {
        if let Some(block_id) = self.current_text_block.take() {
            if let Some(block) = self.block_mut_by_id(block_id) {
                if let BlockKind::Text { streaming, .. } = &mut block.kind {
                    *streaming = false;
                }
            }
        }
    }
}

/// Create a short human-readable summary of a tool's input.
fn summarise_tool_input(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "Read" | "read_file" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(short_path)
            .unwrap_or_else(|| "?".into()),
        "Edit" | "edit_file" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(short_path)
            .unwrap_or_else(|| "?".into()),
        "Write" | "write_file" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(short_path)
            .unwrap_or_else(|| "?".into()),
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| {
                if c.len() > 60 {
                    format!("{}…", truncate_to_char_boundary(c, 57))
                } else {
                    c.to_string()
                }
            })
            .unwrap_or_else(|| "?".into()),
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .map(short_path)
                .unwrap_or_default();
            format!("/{pattern}/ {path}")
        }
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        "Agent" => input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("subagent")
            .to_string(),
        _ => {
            // Generic: show first string field value
            if let Some(obj) = input.as_object() {
                for (_, val) in obj.iter().take(1) {
                    if let Some(s) = val.as_str() {
                        return if s.len() > 50 {
                            format!("{}…", truncate_to_char_boundary(s, 47))
                        } else {
                            s.to_string()
                        };
                    }
                }
            }
            String::new()
        }
    }
}

/// Truncate `s` to at most `max_bytes`, backing up to the nearest UTF-8
/// character boundary so the slice can never panic. Fixed-byte-index
/// truncation previously aborted the app when a multi-byte character
/// straddled the cut (DEV-15).
pub fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Shorten a file path to just the last 2 components.
pub fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        parts.first().unwrap_or(&"?").to_string()
    }
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate_to_char_boundary;

    #[test]
    fn shorter_than_max_is_unchanged() {
        assert_eq!(truncate_to_char_boundary("hello", 10), "hello");
    }

    #[test]
    fn ascii_cuts_exactly_at_max() {
        assert_eq!(truncate_to_char_boundary("hello world", 5), "hello");
    }

    #[test]
    fn backs_up_when_cut_lands_inside_a_multibyte_char() {
        // "a—b": '—' is 3 bytes starting at index 1; cutting at 2 or 3
        // would split it (the DEV-15 crash), so we back up to 1.
        let s = "a\u{2014}b";
        assert_eq!(truncate_to_char_boundary(s, 2), "a");
        assert_eq!(truncate_to_char_boundary(s, 3), "a");
        assert_eq!(truncate_to_char_boundary(s, 4), "a\u{2014}");
    }

    #[test]
    fn emoji_heavy_input_never_panics() {
        let s = "🧬🧬🧬🧬🧬"; // 4 bytes each
        for max in 0..=s.len() {
            let t = truncate_to_char_boundary(s, max);
            assert!(t.len() <= max);
            assert!(s.starts_with(t));
        }
    }
}

#[cfg(test)]
mod permission_wiring_tests {
    use super::*;
    use crate::rich::permissions::{PermissionAction, RiskLevel};

    #[test]
    fn permission_decision_recorded_with_assessed_risk() {
        let mut doc = RichDocument::new();
        doc.push_permission_request(
            Some("Bash".into()),
            Some("rm -rf build".into()),
            Some(serde_json::json!({ "command": "rm -rf build" })),
        );
        doc.record_permission_decision(PermissionAction::Allow);

        let log = doc.decision_log();
        assert_eq!(log.len(), 1);
        let d = &log.decisions()[0];
        assert_eq!(d.action, PermissionAction::Allow);
        assert_eq!(d.request.tool_name, "Bash");
        assert_eq!(
            d.request.risk,
            RiskLevel::High,
            "rm -rf must assess as high risk"
        );
    }

    #[test]
    fn no_decision_recorded_without_active_prompt() {
        let mut doc = RichDocument::new();
        doc.record_permission_decision(PermissionAction::Allow);
        assert!(doc.decision_log().is_empty());
    }

    #[test]
    fn reject_and_open_terminal_actions_are_recorded() {
        for action in [PermissionAction::Reject, PermissionAction::OpenTerminal] {
            let mut doc = RichDocument::new();
            doc.push_permission_request(
                Some("Write".into()),
                Some("/etc/hosts".into()),
                Some(serde_json::json!({ "file_path": "/etc/hosts" })),
            );
            doc.record_permission_decision(action);
            let log = doc.decision_log();
            assert_eq!(log.len(), 1);
            assert_eq!(log.decisions()[0].action, action);
            assert_eq!(log.decisions()[0].request.tool_name, "Write");
        }
    }
}

#[cfg(test)]
mod narrative_wiring_tests {
    use super::*;
    use crate::rich::narrative::{LocusPhase, NarrativeRole};

    #[test]
    fn phase_header_block_is_annotated() {
        let mut doc = RichDocument::new();
        let id = doc
            .apply_event(RichEvent::TextBlock {
                text: "Phase 1: OBSERVE (1/7)".into(),
                parent_agent_id: None,
            })
            .unwrap();
        let ann = doc.annotation(id).expect("annotation attached");
        assert_eq!(ann.role, NarrativeRole::PhaseHeader(LocusPhase::Observe));
    }

    #[test]
    fn user_prompt_and_following_text_share_turn_and_phase() {
        let mut doc = RichDocument::new();
        let prompt = doc.push_user_prompt("do the thing".into());
        assert_eq!(doc.annotation(prompt).unwrap().role, NarrativeRole::Prompt);
        assert_eq!(doc.annotation(prompt).unwrap().turn, 1);

        doc.apply_event(RichEvent::TextBlock {
            text: "## PLAN".into(),
            parent_agent_id: None,
        });
        let prose = doc
            .apply_event(RichEvent::TextBlock {
                text: "sequencing the work".into(),
                parent_agent_id: None,
            })
            .unwrap();
        let ann = doc.annotation(prose).unwrap();
        assert_eq!(ann.phase, Some(LocusPhase::Plan));
        assert_eq!(ann.turn, 1);
    }
}

#[cfg(test)]
mod rail_tests {
    use super::*;

    fn tool_use(id: &str, name: &str) -> RichEvent {
        RichEvent::ToolUse {
            tool_use_id: id.into(),
            tool_name: name.into(),
            input: serde_json::json!({"file_path": "/tmp/x.rs"}),
            parent_agent_id: None,
        }
    }

    fn collapsed_of(doc: &RichDocument, id: BlockId) -> bool {
        doc.blocks().iter().find(|b| b.id == id).unwrap().collapsed
    }

    #[test]
    fn routine_tool_starts_collapsed_mutation_expanded() {
        let mut doc = RichDocument::new();
        let read_id = doc.apply_event(tool_use("t1", "Read")).unwrap();
        let write_id = doc.apply_event(tool_use("t2", "Write")).unwrap();
        assert!(
            collapsed_of(&doc, read_id),
            "routine Read should collapse into the rail"
        );
        assert!(
            !collapsed_of(&doc, write_id),
            "Write is a mutation and stays prominent"
        );
    }

    #[test]
    fn errored_routine_result_auto_expands() {
        let mut doc = RichDocument::new();
        let read_id = doc.apply_event(tool_use("t1", "Read")).unwrap();
        assert!(collapsed_of(&doc, read_id));
        doc.apply_event(RichEvent::ToolResult {
            tool_use_id: "t1".into(),
            content: "No such file".into(),
            is_error: true,
            parent_agent_id: None,
        });
        assert!(
            !collapsed_of(&doc, read_id),
            "an errored routine call must auto-expand"
        );
    }

    #[test]
    fn tool_visibility_override_wins_over_default() {
        let mut doc = RichDocument::new();
        let mut prefs = HashMap::new();
        prefs.insert("Read".to_string(), false); // force a routine read expanded
        prefs.insert("Write".to_string(), true); // force a mutation collapsed
        doc.set_tool_visibility(prefs);

        let read_id = doc.apply_event(tool_use("t1", "Read")).unwrap();
        let write_id = doc.apply_event(tool_use("t2", "Write")).unwrap();
        assert!(!collapsed_of(&doc, read_id), "Read overridden to expanded");
        assert!(
            collapsed_of(&doc, write_id),
            "Write overridden to collapsed"
        );

        // A tool with no override still uses the classification default.
        let grep_id = doc.apply_event(tool_use("t3", "Grep")).unwrap();
        assert!(
            collapsed_of(&doc, grep_id),
            "Grep (routine, no override) collapses"
        );

        // An error still auto-expands, even a force-collapsed tool.
        doc.apply_event(RichEvent::ToolResult {
            tool_use_id: "t2".into(),
            content: "denied".into(),
            is_error: true,
            parent_agent_id: None,
        });
        assert!(
            !collapsed_of(&doc, write_id),
            "error overrides the force-collapse pref"
        );
    }
}

#[cfg(test)]
mod turn_grouping_tests {
    use super::*;

    fn text(s: &str) -> RichEvent {
        RichEvent::TextBlock {
            text: s.into(),
            parent_agent_id: None,
        }
    }

    fn subagent_text(s: &str, agent: &str) -> RichEvent {
        RichEvent::TextBlock {
            text: s.into(),
            parent_agent_id: Some(agent.into()),
        }
    }

    fn tool(id: &str) -> RichEvent {
        RichEvent::ToolUse {
            tool_use_id: id.into(),
            tool_name: "Read".into(),
            input: serde_json::json!({}),
            parent_agent_id: None,
        }
    }

    /// The run lengths for every index, which is the whole grouping decision
    /// in one readable line per test.
    fn runs(doc: &RichDocument) -> Vec<usize> {
        (0..doc.blocks().len()).map(|i| doc.run_len_at(i)).collect()
    }

    #[test]
    fn adjacent_prose_in_one_turn_becomes_a_single_run() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("# Heading"));
        doc.apply_event(text("The paragraph under it."));
        // prompt renders alone; the two text blocks render as one, and the
        // second reports 0 because the first already covers it.
        assert_eq!(runs(&doc), vec![1, 2, 0]);
    }

    #[test]
    fn a_split_heading_and_paragraph_share_one_document() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("# Heading"));
        doc.apply_event(text("The paragraph under it."));
        let merged = doc.run_content(1, 2);
        assert_eq!(merged, "# Heading\n\nThe paragraph under it.");
        assert!(
            merged.contains("\n\n"),
            "a blank line is the only join that cannot fuse two constructs"
        );
    }

    #[test]
    fn a_tool_call_breaks_the_run() {
        // Merging across a tool call would move prose that came AFTER the call
        // to before it. Adjacency is what makes merging safe.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("before"));
        doc.apply_event(tool("t1"));
        doc.apply_event(text("after"));
        assert_eq!(runs(&doc), vec![1, 1, 1, 1]);
    }

    #[test]
    fn a_run_never_crosses_a_turn_boundary() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("first".into());
        doc.apply_event(text("a"));
        doc.push_user_prompt("second".into());
        doc.apply_event(text("b"));
        assert_eq!(runs(&doc), vec![1, 1, 1, 1]);
    }

    #[test]
    fn a_run_never_crosses_an_agent_boundary() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("main"));
        doc.apply_event(subagent_text("delegated", "toolu_1"));
        assert_eq!(runs(&doc), vec![1, 1, 1]);
    }

    #[test]
    fn two_subagents_do_not_merge_into_each_other() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(subagent_text("from a", "toolu_a"));
        doc.apply_event(subagent_text("from b", "toolu_b"));
        assert_eq!(runs(&doc), vec![1, 1, 1]);
    }

    #[test]
    fn a_streaming_tail_is_never_absorbed() {
        // If the in-flight block joined the run, every token would re-measure
        // a screen-tall element in the virtual list.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("settled"));
        doc.apply_event(RichEvent::TextDelta {
            text: "still arriving".into(),
            parent_agent_id: None,
        });
        assert_eq!(runs(&doc), vec![1, 1, 1]);
    }

    #[test]
    fn a_long_run_reports_its_full_length_once() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        for i in 0..4 {
            doc.apply_event(text(&format!("part {i}")));
        }
        assert_eq!(runs(&doc), vec![1, 4, 0, 0, 0]);
        assert_eq!(
            doc.run_content(1, 4),
            "part 0\n\npart 1\n\npart 2\n\npart 3"
        );
    }

    #[test]
    fn the_speaker_is_named_once_per_turn() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("first"));
        doc.apply_event(tool("t1"));
        doc.apply_event(text("second"));
        let anchors: Vec<bool> = (0..doc.blocks().len())
            .map(|i| doc.starts_turn_prose(i))
            .collect();
        assert_eq!(
            anchors,
            vec![false, true, false, false],
            "only the turn's first prose block names the speaker"
        );
    }

    #[test]
    fn each_turn_names_its_speaker_again() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("one".into());
        doc.apply_event(text("a"));
        doc.push_user_prompt("two".into());
        doc.apply_event(text("b"));
        assert!(doc.starts_turn_prose(1));
        assert!(doc.starts_turn_prose(3), "a new turn is named again");
    }

    #[test]
    fn a_turn_with_no_prose_names_nobody() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1"));
        assert!((0..doc.blocks().len()).all(|i| !doc.starts_turn_prose(i)));
    }

    #[test]
    fn subagent_prose_is_not_labelled_as_the_main_agent() {
        // `render_agent_header` attributes a delegated run once, on its own.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(subagent_text("delegated", "toolu_1"));
        assert!(!doc.starts_turn_prose(1));
    }

    #[test]
    fn an_absorbed_block_still_holds_its_list_slot() {
        // Runs are a view grouping, not a model rewrite: the block count is
        // unchanged, so every index the jump and search indexes hold stays
        // valid.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(text("a"));
        doc.apply_event(text("b"));
        assert_eq!(doc.blocks().len(), 3, "no block is removed by merging");
    }
}

#[cfg(test)]
mod rail_grouping_tests {
    use super::*;

    fn tool(id: &str, name: &str, target: &str) -> RichEvent {
        RichEvent::ToolUse {
            tool_use_id: id.into(),
            tool_name: name.into(),
            input: serde_json::json!({ "file_path": target }),
            parent_agent_id: None,
        }
    }

    fn subagent_tool(id: &str, name: &str, agent: &str) -> RichEvent {
        RichEvent::ToolUse {
            tool_use_id: id.into(),
            tool_name: name.into(),
            input: serde_json::json!({ "file_path": "/tmp/x.rs" }),
            parent_agent_id: Some(agent.into()),
        }
    }

    fn text(s: &str) -> RichEvent {
        RichEvent::TextBlock {
            text: s.into(),
            parent_agent_id: None,
        }
    }

    fn failed(id: &str) -> RichEvent {
        RichEvent::ToolResult {
            tool_use_id: id.into(),
            content: "No such file".into(),
            is_error: true,
            parent_agent_id: None,
        }
    }

    /// The rail run each index belongs to, as `(start, len)`, which puts the
    /// whole grouping decision on one readable line per test.
    fn rails(doc: &RichDocument) -> Vec<Option<(usize, usize)>> {
        (0..doc.blocks().len())
            .map(|i| doc.rail_run_at(i))
            .collect()
    }

    #[test]
    fn a_run_of_routine_calls_becomes_one_rail() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(tool("t2", "Read", "/b.rs"));
        doc.apply_event(tool("t3", "Bash", "cargo test"));
        assert_eq!(
            rails(&doc),
            vec![None, Some((1, 3)), Some((1, 3)), Some((1, 3))]
        );
    }

    #[test]
    fn the_summary_counts_by_kind() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(tool("t2", "Read", "/b.rs"));
        doc.apply_event(tool("t3", "Bash", "cargo test"));
        let (summary, count) = doc.rail_summary(1, 3);
        assert!(summary.contains("2 reads"), "got {summary}");
        assert!(summary.contains("1 shell"), "got {summary}");
        assert_eq!(count, 3, "the count is what was folded in, not the span");
    }

    #[test]
    fn a_single_routine_call_is_not_railed() {
        // Aggregating one call into "1 read" costs a click and saves nothing.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        assert_eq!(rails(&doc), vec![None, None]);
    }

    #[test]
    fn a_mutation_never_joins_a_rail() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(tool("t2", "Write", "/b.rs"));
        doc.apply_event(tool("t3", "Read", "/c.rs"));
        assert_eq!(rails(&doc), vec![None, None, None, None]);
    }

    #[test]
    fn a_notable_call_never_joins_a_rail() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(tool("t2", "Task", "delegate"));
        doc.apply_event(tool("t3", "Read", "/c.rs"));
        assert_eq!(rails(&doc), vec![None, None, None, None]);
    }

    #[test]
    fn an_errored_call_leaves_the_rail_and_splits_it() {
        // A failure must never sit behind a summary line. When the result
        // lands the run is recomputed and the error stands on its own.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(tool("t2", "Read", "/missing.rs"));
        doc.apply_event(tool("t3", "Read", "/c.rs"));
        assert_eq!(
            rails(&doc)[1],
            Some((1, 3)),
            "all three rail together while none has failed"
        );
        doc.apply_event(failed("t2"));
        assert_eq!(
            rails(&doc),
            vec![None, None, None, None],
            "the failure splits the run, leaving two runs of one — neither railed"
        );
    }

    #[test]
    fn prose_between_two_groups_breaks_the_run() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(tool("t2", "Read", "/b.rs"));
        doc.apply_event(text("some prose"));
        doc.apply_event(tool("t3", "Read", "/c.rs"));
        doc.apply_event(tool("t4", "Read", "/d.rs"));
        assert_eq!(
            rails(&doc),
            vec![
                None,
                Some((1, 2)),
                Some((1, 2)),
                None,
                Some((4, 2)),
                Some((4, 2))
            ]
        );
    }

    #[test]
    fn a_rail_never_crosses_a_turn_boundary() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("first".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.push_user_prompt("second".into());
        doc.apply_event(tool("t2", "Read", "/b.rs"));
        assert_eq!(rails(&doc), vec![None, None, None, None]);
    }

    #[test]
    fn a_rail_never_crosses_an_agent_boundary() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(tool("t1", "Read", "/a.rs"));
        doc.apply_event(subagent_tool("t2", "Read", "toolu_x"));
        assert_eq!(rails(&doc), vec![None, None, None]);
    }

    #[test]
    fn two_subagents_do_not_rail_together() {
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        doc.apply_event(subagent_tool("t1", "Read", "toolu_a"));
        doc.apply_event(subagent_tool("t2", "Read", "toolu_b"));
        assert_eq!(rails(&doc), vec![None, None, None]);
    }

    #[test]
    fn every_member_reports_the_same_run() {
        // The view relies on this: a block asks which run it is in, and only
        // the start draws the summary.
        let mut doc = RichDocument::new();
        doc.push_user_prompt("go".into());
        for i in 0..5 {
            doc.apply_event(tool(&format!("t{i}"), "Read", &format!("/{i}.rs")));
        }
        for ix in 1..6 {
            assert_eq!(doc.rail_run_at(ix), Some((1, 5)), "index {ix}");
        }
    }
}
