//! Multi-line prompt input for Rich Mode.
//!
//! Uses GPUI's `EntityInputHandler` for OS-level character input, IME
//! composition, and click-to-position — the same pattern as
//! `src/text_input.rs`, extended to multiple lines.
//!
//! All macOS text-editing conventions are declared as GPUI actions in
//! this file (see the `actions!` block below) and bound to keystrokes in
//! `assets/default-keymap.json` via the loader in `src/keymap.rs`.
//!
//! - Cursor movement: arrow keys, Option+arrow (word), Cmd+arrow (line)
//! - Selection: Shift+variants of all movement commands
//! - Deletion: Backspace, Delete, Option+Backspace (word), Cmd+Backspace (line)
//! - Clipboard: Cmd+C, Cmd+X, Cmd+V
//! - Newline: Enter
//! - Submit: Cmd+Enter
//! - Character palette: Ctrl+Cmd+Space
//!
//! Action names use the `compose.*` namespace (dotted snake_case) in the
//! keymap — the dispatch table in `keymap.rs` maps those strings to the
//! concrete action structs declared here.

use std::ops::Range;

use gpui::{
    actions, fill, point, prelude::*, px, relative, rgb, rgba, size, App, Bounds, ClipboardItem,
    Context, CursorStyle, ElementId, ElementInputHandler, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, Hsla, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Rgba, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;

// ── Actions ───────────────────────────────────────────────────────

actions!(
    compose,
    [
        CursorLeft,
        CursorRight,
        CursorUp,
        CursorDown,
        CursorWordLeft,
        CursorWordRight,
        CursorLineStart,
        CursorLineEnd,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectLineStart,
        SelectLineEnd,
        SelectAll,
        DeleteLeft,
        DeleteRight,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteLineLeft,
        Copy,
        Cut,
        Paste,
        InsertNewline,
        Submit,
        ShowCharacterPalette,
    ]
);

/// Key context gating compose-bar bindings. Must match the `"context"`
/// value in `assets/default-keymap.json`. The render fn applies it via
/// `div().key_context(KEY_CONTEXT)` so bindings fire only when a compose
/// bar is focused.
const KEY_CONTEXT: &str = "ComposeBar";

// ── Colours (Catppuccin Mocha) ────────────────────────────────────

const SURFACE0: u32 = 0x313244;
const SURFACE1: u32 = 0x45475a;
const TEXT: u32 = 0xcdd6f4;
const SUBTEXT0: u32 = 0xa6adc8;
const BLUE: u32 = 0x89b4fa;
const CRUST: u32 = 0x11111b;

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

// ── Events ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ComposeBarEvent {
    /// User submitted a prompt (Cmd+Enter or Send button).
    Submit { text: String },
}

// ── Layout state (computed during paint) ──────────────────────────

struct MultilineLayout {
    /// Shaped line per content line (one entry per `\n`-separated segment).
    lines: Vec<ShapedLine>,
    /// Byte offset at the start of each line in `content`.
    line_starts: Vec<usize>,
    /// Line height in pixels (from GPUI's text system).
    line_height: Pixels,
}

// ── View ──────────────────────────────────────────────────────────

pub struct ComposeBar {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    /// Selected range in byte offsets. Cursor is at `start` or `end`
    /// depending on `selection_reversed`.
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// IME composition range.
    marked_range: Option<Range<usize>>,
    /// Layout state populated by the TextElement during paint.
    last_layout: Option<MultilineLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    busy: bool,
    font_size: f32,
}

impl EventEmitter<ComposeBarEvent> for ComposeBar {}

impl ComposeBar {
    pub fn new(cx: &mut Context<Self>, font_size: f32) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: "Message Claude…   (⌘⏎ to send)".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            busy: false,
            font_size,
        }
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        if self.busy != busy {
            self.busy = busy;
            cx.notify();
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    // ── Cursor / selection helpers ────────────────────────────────

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    // ── Word boundary helpers ─────────────────────────────────────

    /// Previous word boundary from `offset` (Option+Left behaviour).
    fn previous_word_boundary(&self, offset: usize) -> usize {
        // Find all word boundaries preceding `offset`.
        // Use split_word_bound_indices to get positions.
        let mut last_start = 0usize;
        for (idx, word) in self.content.split_word_bound_indices() {
            if idx >= offset {
                break;
            }
            // Only stop at "real" word starts (skip whitespace-only segments).
            if word.chars().any(|c| c.is_alphanumeric() || c == '_') {
                last_start = idx;
            }
        }
        last_start
    }

    /// Next word boundary from `offset` (Option+Right behaviour).
    fn next_word_boundary(&self, offset: usize) -> usize {
        let mut passed_word = false;
        for (idx, word) in self.content.split_word_bound_indices() {
            let end = idx + word.len();
            if end <= offset {
                continue;
            }
            // Is this segment a "real" word?
            let is_word = word.chars().any(|c| c.is_alphanumeric() || c == '_');
            if is_word && idx <= offset {
                // Currently inside a word — jump to its end.
                passed_word = true;
                return end;
            }
            if is_word && passed_word {
                return end;
            }
            if is_word {
                // Skip to end of this next word.
                return end;
            }
            if idx >= offset && !is_word {
                // Whitespace after cursor — continue until we find a word.
                continue;
            }
        }
        self.content.len()
    }

    // ── Line boundary helpers ─────────────────────────────────────

    fn line_start(&self, offset: usize) -> usize {
        self.content[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.content[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.content.len())
    }

    /// Column (byte offset) within the current line.
    fn col_in_line(&self, offset: usize) -> usize {
        offset - self.line_start(offset)
    }

    // ── Cursor up/down with column preservation ──────────────────

    fn cursor_up_offset(&self) -> usize {
        let offset = self.cursor_offset();
        let col = self.col_in_line(offset);
        let ls = self.line_start(offset);
        if ls == 0 {
            return 0;
        }
        let prev_line_end = ls - 1; // the \n
        let prev_line_start = self.line_start(prev_line_end);
        let prev_line_len = prev_line_end - prev_line_start;
        prev_line_start + col.min(prev_line_len)
    }

    fn cursor_down_offset(&self) -> usize {
        let offset = self.cursor_offset();
        let col = self.col_in_line(offset);
        let le = self.line_end(offset);
        if le >= self.content.len() {
            return self.content.len();
        }
        let next_line_start = le + 1;
        let next_line_end = self.line_end(next_line_start);
        let next_line_len = next_line_end - next_line_start;
        next_line_start + col.min(next_line_len)
    }

    // ── Text replacement ──────────────────────────────────────────

    fn replace_selection(&mut self, new_text: &str, cx: &mut Context<Self>) {
        let range = self.selected_range.clone();
        self.content.replace_range(range.clone(), new_text);
        let new_cursor = range.start + new_text.len();
        self.selected_range = new_cursor..new_cursor;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    // ── Actions: cursor movement ──────────────────────────────────

    fn cursor_left(&mut self, _: &CursorLeft, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.move_to(self.selected_range.start, cx);
        } else {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        }
    }

    fn cursor_right(&mut self, _: &CursorRight, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.move_to(self.selected_range.end, cx);
        } else {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        }
    }

    fn cursor_up(&mut self, _: &CursorUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.cursor_up_offset();
        self.move_to(target, cx);
    }

    fn cursor_down(&mut self, _: &CursorDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.cursor_down_offset();
        self.move_to(target, cx);
    }

    fn cursor_word_left(&mut self, _: &CursorWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_word_boundary(self.cursor_offset());
        self.move_to(target, cx);
    }

    fn cursor_word_right(&mut self, _: &CursorWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_word_boundary(self.cursor_offset());
        self.move_to(target, cx);
    }

    fn cursor_line_start(&mut self, _: &CursorLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.line_start(self.cursor_offset());
        self.move_to(target, cx);
    }

    fn cursor_line_end(&mut self, _: &CursorLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.line_end(self.cursor_offset());
        self.move_to(target, cx);
    }

    // ── Actions: selection ────────────────────────────────────────

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.cursor_up_offset();
        self.select_to(target, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.cursor_down_offset();
        self.select_to(target, cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_word_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_word_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.line_start(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.line_end(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    // ── Actions: deletion ─────────────────────────────────────────

    fn delete_left(&mut self, _: &DeleteLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.selected_range = prev..self.cursor_offset();
        }
        self.replace_selection("", cx);
    }

    fn delete_right(&mut self, _: &DeleteRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.selected_range = self.cursor_offset()..next;
        }
        self.replace_selection("", cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.previous_word_boundary(self.cursor_offset());
            self.selected_range = target..self.cursor_offset();
        }
        self.replace_selection("", cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.next_word_boundary(self.cursor_offset());
            self.selected_range = self.cursor_offset()..target;
        }
        self.replace_selection("", cx);
    }

    fn delete_line_left(&mut self, _: &DeleteLineLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let target = self.line_start(self.cursor_offset());
            self.selected_range = target..self.cursor_offset();
        }
        self.replace_selection("", cx);
    }

    // ── Actions: clipboard ────────────────────────────────────────

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let text = self.content[self.selected_range.clone()].to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            let text = self.content[self.selected_range.clone()].to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.replace_selection("", cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let t = text.clone();
            self.replace_selection(&t, cx);
        }
    }

    // ── Actions: newline / submit ─────────────────────────────────

    fn insert_newline(&mut self, _: &InsertNewline, _: &mut Window, cx: &mut Context<Self>) {
        self.replace_selection("\n", cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let text = self.content.trim().to_string();
        if text.is_empty() {
            return;
        }
        cx.emit(ComposeBarEvent::Submit { text });
        self.content.clear();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    // ── Mouse helpers ─────────────────────────────────────────────

    fn index_for_mouse_position(&self, position: gpui::Point<Pixels>) -> usize {
        let (Some(bounds), Some(layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        // Which line does y fall into?
        let y_in = position.y - bounds.top();
        let mut line_index =
            (f32::from(y_in) / f32::from(layout.line_height)).floor() as usize;
        if line_index >= layout.lines.len() {
            line_index = layout.lines.len().saturating_sub(1);
        }
        let line_start = layout.line_starts.get(line_index).copied().unwrap_or(0);
        let line = &layout.lines[line_index];
        let x_in = position.x - bounds.left();
        let offset_in_line = line.closest_index_for_x(x_in);
        line_start + offset_in_line
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        let idx = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(idx, cx);
        } else {
            self.move_to(idx, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let idx = self.index_for_mouse_position(event.position);
            self.select_to(idx, cx);
        }
    }

    // ── UTF-16 range helpers (for OS text input) ─────────────────

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }
}

// ── EntityInputHandler (OS text input pathway) ───────────────────

impl EntityInputHandler for ComposeBar {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content.replace_range(range.clone(), new_text);
        let new_cursor = range.start + new_text.len();
        self.selected_range = new_cursor..new_cursor;
        self.selection_reversed = false;
        self.marked_range.take();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content.replace_range(range.clone(), new_text);
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        // Use the start of the range for the bounds (good enough for IME popup).
        let line_index = line_index_for_offset(range.start, &layout.line_starts);
        let line_start = layout.line_starts[line_index];
        let line = layout.lines.get(line_index)?;
        let x = line.x_for_index(range.start - line_start);
        let y = layout.line_height * (line_index as f32);
        Some(Bounds::from_corners(
            point(bounds.left() + x, bounds.top() + y),
            point(
                bounds.left() + x,
                bounds.top() + y + layout.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let idx = self.index_for_mouse_position(point);
        Some(self.offset_to_utf16(idx))
    }
}

impl Focusable for ComposeBar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn line_index_for_offset(offset: usize, line_starts: &[usize]) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

// ── Custom Element: multi-line text rendering ─────────────────────

struct TextElement {
    input: gpui::Entity<ComposeBar>,
}

struct PrepaintState {
    layout: MultilineLayout,
    cursor: Option<PaintQuad>,
    selection_rects: Vec<PaintQuad>,
    /// Total height (lines × line_height).
    total_height: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = f32; // line_height
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let content = &input.content;
        let is_placeholder = content.is_empty();
        let line_count = if is_placeholder {
            1
        } else {
            content.chars().filter(|c| *c == '\n').count() + 1
        };
        let line_h = window.line_height();
        let total = line_h * (line_count as f32);

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = total.into();
        (
            window.request_layout(style, [], cx),
            f32::from(line_h),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor_offset = input.cursor_offset();
        let marked_range = input.marked_range.clone();
        let placeholder = input.placeholder.clone();
        let style = window.text_style();
        let line_height = window.line_height();

        let is_placeholder = content.is_empty();
        let (display_text, text_color) = if is_placeholder {
            (placeholder.to_string(), hex(SUBTEXT0))
        } else {
            (content.clone(), hex(TEXT))
        };

        // Split into lines (keep line boundaries)
        let mut line_starts = Vec::new();
        let mut pos = 0usize;
        for line in display_text.split('\n') {
            line_starts.push(pos);
            pos += line.len() + 1; // +1 for the \n
        }

        // Shape each line.
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut shaped_lines = Vec::with_capacity(line_starts.len());
        for (i, line) in display_text.split('\n').enumerate() {
            let line_start = line_starts[i];
            let line_len = line.len();
            let run = TextRun {
                len: line_len,
                font: style.font(),
                color: text_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            // Build runs — if this line contains the marked range, underline it
            let runs = if let Some(mr) = marked_range.as_ref() {
                build_line_runs(line_start, line_len, mr, &run)
            } else {
                vec![run]
            };
            let shaped = window
                .text_system()
                .shape_line(line.to_string().into(), font_size, &runs, None);
            shaped_lines.push(shaped);
        }

        // For placeholder case, we only have one line of placeholder text;
        // the content-based line_starts are the ones used for cursor mapping.
        let content_line_starts = if is_placeholder {
            vec![0]
        } else {
            let mut starts = Vec::new();
            let mut p = 0;
            for line in content.split('\n') {
                starts.push(p);
                p += line.len() + 1;
            }
            starts
        };

        // Compute cursor position (only when not placeholder).
        let cursor_quad = if !is_placeholder {
            let line_index = line_index_for_offset(cursor_offset, &content_line_starts);
            let line_start = content_line_starts[line_index];
            let cursor_in_line = cursor_offset - line_start;
            let shaped = &shaped_lines[line_index];
            let x = shaped.x_for_index(cursor_in_line);
            let y = line_height * (line_index as f32);
            Some(fill(
                Bounds::new(
                    point(bounds.left() + x, bounds.top() + y),
                    size(px(1.5), line_height),
                ),
                rgb(BLUE),
            ))
        } else {
            // Placeholder — still show cursor at position 0
            Some(fill(
                Bounds::new(
                    point(bounds.left(), bounds.top()),
                    size(px(1.5), line_height),
                ),
                rgb(BLUE),
            ))
        };

        // Compute selection rectangles (one per covered line).
        let mut selection_rects = Vec::new();
        if !selected_range.is_empty() && !is_placeholder {
            let start_line = line_index_for_offset(selected_range.start, &content_line_starts);
            let end_line = line_index_for_offset(selected_range.end, &content_line_starts);
            for li in start_line..=end_line {
                let line_start = content_line_starts[li];
                let line = &shaped_lines[li];
                let (sel_start_x, sel_end_x) = if li == start_line && li == end_line {
                    (
                        line.x_for_index(selected_range.start - line_start),
                        line.x_for_index(selected_range.end - line_start),
                    )
                } else if li == start_line {
                    (
                        line.x_for_index(selected_range.start - line_start),
                        line.width + px(6.0),
                    )
                } else if li == end_line {
                    (px(0.0), line.x_for_index(selected_range.end - line_start))
                } else {
                    (px(0.0), line.width + px(6.0))
                };
                let y = line_height * (li as f32);
                selection_rects.push(fill(
                    Bounds::from_corners(
                        point(bounds.left() + sel_start_x, bounds.top() + y),
                        point(
                            bounds.left() + sel_end_x,
                            bounds.top() + y + line_height,
                        ),
                    ),
                    rgba(0x89b4fa55),
                ));
            }
        }

        let total_height = line_height * (shaped_lines.len() as f32);

        PrepaintState {
            layout: MultilineLayout {
                lines: shaped_lines,
                line_starts: content_line_starts,
                line_height,
            },
            cursor: cursor_quad,
            selection_rects,
            total_height,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        // Paint selection backgrounds first (behind text)
        for rect in prepaint.selection_rects.drain(..) {
            window.paint_quad(rect);
        }

        // Paint each line of text
        let line_height = prepaint.layout.line_height;
        for (i, line) in prepaint.layout.lines.iter().enumerate() {
            let y = line_height * (i as f32);
            let origin = point(bounds.origin.x, bounds.origin.y + y);
            line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx)
                .ok();
        }

        // Paint cursor on top if focused
        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        // Update the input with the current layout + bounds so mouse handlers
        // can map cursor/mouse positions back to character offsets.
        let layout = MultilineLayout {
            lines: std::mem::take(&mut prepaint.layout.lines),
            line_starts: std::mem::take(&mut prepaint.layout.line_starts),
            line_height: prepaint.layout.line_height,
        };
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(layout);
            input.last_bounds = Some(bounds);
        });
    }
}

fn build_line_runs(
    line_start: usize,
    line_len: usize,
    marked: &Range<usize>,
    base: &TextRun,
) -> Vec<TextRun> {
    // If marked range doesn't overlap this line, single run
    let line_end = line_start + line_len;
    if marked.end <= line_start || marked.start >= line_end {
        return vec![base.clone()];
    }
    let overlap_start = marked.start.max(line_start) - line_start;
    let overlap_end = marked.end.min(line_end) - line_start;
    let mut runs = Vec::new();
    if overlap_start > 0 {
        runs.push(TextRun {
            len: overlap_start,
            ..base.clone()
        });
    }
    runs.push(TextRun {
        len: overlap_end - overlap_start,
        underline: Some(UnderlineStyle {
            color: Some(base.color),
            thickness: px(1.0),
            wavy: false,
        }),
        ..base.clone()
    });
    if overlap_end < line_len {
        runs.push(TextRun {
            len: line_len - overlap_end,
            ..base.clone()
        });
    }
    runs
}

// ── Render ────────────────────────────────────────────────────────

impl Render for ComposeBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = self.font_size;
        let can_send = !self.content.trim().is_empty() && !self.busy;
        let send_color = if can_send {
            hex(BLUE)
        } else {
            hex_alpha(SUBTEXT0, 0.5)
        };
        let send_label = if self.busy { "…" } else { "Send" };

        // Vertical padding accounted for: 16px total (py 8 top + bottom).
        // Outer bar has its own p(8). Text area shows up to 8 lines tall.
        let line_h = self.font_size * 1.5;
        let visible_lines = 8.0_f32;
        let max_h = visible_lines * line_h + 16.0; // + py
        let min_h = line_h + 16.0;

        let text_area = gpui::div()
            .id("compose-text-area")
            .overflow_y_scroll()
            .w_full()
            .max_h(px(max_h))
            .min_h(px(min_h))
            .py(px(8.0))
            .px(px(12.0))
            .cursor(CursorStyle::IBeam)
            .flex()
            .child(TextElement {
                input: cx.entity(),
            });

        let send_button = gpui::div()
            .id("compose-send-btn")
            .flex()
            .items_center()
            .justify_center()
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .bg(hex_alpha(SURFACE1, 0.6))
            .cursor(if can_send {
                CursorStyle::PointingHand
            } else {
                CursorStyle::default()
            })
            .child(
                gpui::div()
                    .text_size(px(font_size - 1.0))
                    .text_color(send_color)
                    .child(send_label),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this: &mut Self, _event, window, cx| {
                    this.submit(&Submit, window, cx);
                }),
            );

        gpui::div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            // Cursor movement
            .on_action(cx.listener(Self::cursor_left))
            .on_action(cx.listener(Self::cursor_right))
            .on_action(cx.listener(Self::cursor_up))
            .on_action(cx.listener(Self::cursor_down))
            .on_action(cx.listener(Self::cursor_word_left))
            .on_action(cx.listener(Self::cursor_word_right))
            .on_action(cx.listener(Self::cursor_line_start))
            .on_action(cx.listener(Self::cursor_line_end))
            // Selection
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::select_all))
            // Deletion
            .on_action(cx.listener(Self::delete_left))
            .on_action(cx.listener(Self::delete_right))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_line_left))
            // Clipboard
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            // Newline / submit
            .on_action(cx.listener(Self::insert_newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::show_character_palette))
            // Mouse selection
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .flex_shrink_0()
            .p(px(8.0))
            .border_t_1()
            .border_color(hex_alpha(SURFACE1, 0.5))
            .bg(hex(CRUST))
            .flex()
            .gap(px(8.0))
            .items_end()
            .child(
                gpui::div()
                    .flex_1()
                    .rounded(px(6.0))
                    .bg(hex_alpha(SURFACE0, 0.6))
                    .border_1()
                    .border_color(hex_alpha(SURFACE1, 0.5))
                    .child(text_area),
            )
            .child(send_button)
    }
}
