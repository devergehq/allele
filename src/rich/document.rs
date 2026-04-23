//! Document model — the stateful tree that RichEvents mutate and the renderer reads.
//!
//! Inspired by observability-trace models: each block is a "span" with an optional
//! parent (for subagent nesting). The tree is append-heavy with rare in-place updates
//! (status changes on existing nodes).

use crate::stream::RichEvent;
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

    /// Transient "thinking" indicator shown while the CLI is processing
    /// but hasn't produced any output blocks yet. Removed when the first
    /// real block arrives or when the session ends.
    AwaitingResponse,
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
    next_id: BlockId,
}

impl RichDocument {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            tool_use_index: HashMap::new(),
            current_text_block: None,
            awaiting_block: None,
            next_id: 0,
        }
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
        let id = self.push_block(Block {
            id: self.next_id,
            kind: BlockKind::UserPrompt { content },
            parent_agent_id: None,
            collapsed: false,
            cached_height: None,
        });
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

        match event {
            RichEvent::TextDelta { text, parent_agent_id } => {
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

            RichEvent::TextBlock { text, parent_agent_id } => {
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

            RichEvent::ThinkingBlock { thinking, parent_agent_id } => {
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

            RichEvent::ToolUse { tool_use_id, tool_name, input, parent_agent_id } => {
                self.close_text_stream();
                let summary = summarise_tool_input(&tool_name, &input);
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
                    // Collapsed by default — header shows name + summary; click
                    // expands to the full JSON input.
                    collapsed: true,
                    cached_height: None,
                });
                self.tool_use_index.insert(tool_use_id, id);
                Some(id)
            }

            RichEvent::EditDiff { tool_use_id, file_path, old_string, new_string, parent_agent_id } => {
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

            RichEvent::ToolResult { tool_use_id, content, is_error, .. } => {
                // Attach result to existing tool call block
                if let Some(&block_id) = self.tool_use_index.get(&tool_use_id) {
                    if let Some(block) = self.block_mut_by_id(block_id) {
                        let result = ToolCallResult { content, is_error };
                        match &mut block.kind {
                            BlockKind::ToolCall { result: r, .. } => *r = Some(result),
                            BlockKind::Diff { result: r, .. } => *r = Some(result),
                            _ => {}
                        }
                        block.cached_height = None; // invalidate
                    }
                }
                None
            }

            RichEvent::SessionResult { duration_ms, cost_usd, num_turns, is_error, result_text } => {
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

            RichEvent::Init { .. } | RichEvent::HookStatus { .. } => None,
        }
    }

    /// Toggle collapsed state of a block.
    pub fn toggle_collapsed(&mut self, block_id: BlockId) {
        if let Some(block) = self.block_mut_by_id(block_id) {
            block.collapsed = !block.collapsed;
            block.cached_height = None;
        }
    }

    /// Invalidate all cached heights (e.g. on resize).
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
            .map(|p| short_path(p))
            .unwrap_or_else(|| "?".into()),
        "Edit" | "edit_file" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| short_path(p))
            .unwrap_or_else(|| "?".into()),
        "Write" | "write_file" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| short_path(p))
            .unwrap_or_else(|| "?".into()),
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| {
                if c.len() > 60 {
                    format!("{}…", &c[..57])
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
                .map(|p| short_path(p))
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
                            format!("{}…", &s[..47])
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

/// Shorten a file path to just the last 2 components.
pub fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    if parts.len() == 2 {
        format!("{}/{}", parts[1], parts[0])
    } else {
        parts.first().unwrap_or(&"?").to_string()
    }
}
