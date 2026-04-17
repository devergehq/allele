//! ComposeBar — multi-line text input at the bottom of RichView.
//!
//! Handles prompt composition and submission. Sits at the bottom of the
//! activity feed. Cmd+Enter submits, bare Enter inserts a newline.
//!
//! v0.1: Basic multi-line text area with send button.
//! v0.2: Paste collapsing, image/file attachments.

use gpui::*;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

// ── Actions ───────────────────────────────────────────────────────

actions!(
    compose_bar,
    [
        ComposeBackspace,
        ComposeDelete,
        ComposeLeft,
        ComposeRight,
        ComposeUp,
        ComposeDown,
        ComposeSelectLeft,
        ComposeSelectRight,
        ComposeSelectAll,
        ComposeHome,
        ComposeEnd,
        ComposePaste,
        ComposeCut,
        ComposeCopy,
        ComposeSubmit,
        ComposeNewline,
    ]
);

const KEY_CONTEXT: &str = "ComposeBar";

/// Register compose bar key bindings. Call once during app startup.
pub fn bind_compose_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", ComposeBackspace, Some(KEY_CONTEXT)),
        KeyBinding::new("delete", ComposeDelete, Some(KEY_CONTEXT)),
        KeyBinding::new("left", ComposeLeft, Some(KEY_CONTEXT)),
        KeyBinding::new("right", ComposeRight, Some(KEY_CONTEXT)),
        KeyBinding::new("up", ComposeUp, Some(KEY_CONTEXT)),
        KeyBinding::new("down", ComposeDown, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-left", ComposeSelectLeft, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-right", ComposeSelectRight, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-a", ComposeSelectAll, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-v", ComposePaste, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-c", ComposeCopy, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-x", ComposeCut, Some(KEY_CONTEXT)),
        KeyBinding::new("home", ComposeHome, Some(KEY_CONTEXT)),
        KeyBinding::new("end", ComposeEnd, Some(KEY_CONTEXT)),
        // Cmd+Enter submits, bare Enter inserts newline
        KeyBinding::new("cmd-enter", ComposeSubmit, Some(KEY_CONTEXT)),
        KeyBinding::new("enter", ComposeNewline, Some(KEY_CONTEXT)),
    ]);
}

// ── Colours (Catppuccin Mocha) ────────────────────────────────────

const SURFACE0: u32 = 0x313244;
const SURFACE1: u32 = 0x45475a;
const TEXT: u32 = 0xcdd6f4;
const SUBTEXT0: u32 = 0xa6adc8;
const BLUE: u32 = 0x89b4fa;
const BASE: u32 = 0x1e1e2e;
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
    /// User submitted a prompt (Cmd+Enter).
    Submit { text: String },
}

// ── View ──────────────────────────────────────────────────────────

pub struct ComposeBar {
    focus_handle: FocusHandle,
    content: String,
    placeholder: String,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// Whether a prompt is currently being processed (disables input).
    busy: bool,
    font_size: f32,
}

impl EventEmitter<ComposeBarEvent> for ComposeBar {}

impl ComposeBar {
    pub fn new(cx: &mut Context<Self>, font_size: f32) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: "Message Claude… (⌘⏎ to send)".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            busy: false,
            font_size,
        }
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    // ── Cursor helpers ────────────────────────────────────────────

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

    fn replace_range(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        self.content = format!(
            "{}{}{}",
            &self.content[..range.start],
            new_text,
            &self.content[range.end..]
        );
        let new_cursor = range.start + new_text.len();
        self.selected_range = new_cursor..new_cursor;
        self.marked_range = None;
        cx.notify();
    }

    // ── Line navigation helpers ───────────────────────────────────

    fn line_start(&self, offset: usize) -> usize {
        self.content[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.content[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.content.len())
    }

    fn col_in_line(&self, offset: usize) -> usize {
        offset - self.line_start(offset)
    }

    // ── Actions ───────────────────────────────────────────────────

    fn backspace(&mut self, _: &ComposeBackspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.selected_range = prev..self.cursor_offset();
        }
        self.replace_range(self.selected_range.clone(), "", cx);
    }

    fn delete(&mut self, _: &ComposeDelete, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.selected_range = self.cursor_offset()..next;
        }
        self.replace_range(self.selected_range.clone(), "", cx);
    }

    fn left(&mut self, _: &ComposeLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &ComposeRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &ComposeUp, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.cursor_offset();
        let col = self.col_in_line(offset);
        let line_start = self.line_start(offset);
        if line_start == 0 {
            // Already on first line — go to start
            self.move_to(0, cx);
        } else {
            // Go to same column on previous line
            let prev_line_end = line_start - 1; // the \n
            let prev_line_start = self.line_start(prev_line_end);
            let prev_line_len = prev_line_end - prev_line_start;
            self.move_to(prev_line_start + col.min(prev_line_len), cx);
        }
    }

    fn down(&mut self, _: &ComposeDown, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.cursor_offset();
        let col = self.col_in_line(offset);
        let line_end = self.line_end(offset);
        if line_end >= self.content.len() {
            // Already on last line — go to end
            self.move_to(self.content.len(), cx);
        } else {
            // Go to same column on next line
            let next_line_start = line_end + 1; // skip \n
            let next_line_end = self.line_end(next_line_start);
            let next_line_len = next_line_end - next_line_start;
            self.move_to(next_line_start + col.min(next_line_len), cx);
        }
    }

    fn select_left(&mut self, _: &ComposeSelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &ComposeSelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &ComposeSelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    fn home(&mut self, _: &ComposeHome, _: &mut Window, cx: &mut Context<Self>) {
        let line_start = self.line_start(self.cursor_offset());
        self.move_to(line_start, cx);
    }

    fn end(&mut self, _: &ComposeEnd, _: &mut Window, cx: &mut Context<Self>) {
        let line_end = self.line_end(self.cursor_offset());
        self.move_to(line_end, cx);
    }

    fn paste(&mut self, _: &ComposePaste, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // TODO v0.2: detect long pastes (>20 lines) and collapse to PasteCard
            self.replace_range(self.selected_range.clone(), &text, cx);
        }
    }

    fn copy(&mut self, _: &ComposeCopy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &ComposeCut, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_range(self.selected_range.clone(), "", cx);
        }
    }

    fn newline(&mut self, _: &ComposeNewline, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        self.replace_range(self.selected_range.clone(), "\n", cx);
    }

    fn submit(&mut self, _: &ComposeSubmit, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy { return; }
        let text = self.content.trim().to_string();
        if text.is_empty() { return; }

        cx.emit(ComposeBarEvent::Submit { text });
        self.content.clear();
        self.selected_range = 0..0;
        self.marked_range = None;
        cx.notify();
    }
}

impl Focusable for ComposeBar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ComposeBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = self.font_size;
        let line_height = font_size * 1.5;

        // Split content into lines for display
        let display_text = if self.content.is_empty() {
            self.placeholder.clone()
        } else {
            self.content.clone()
        };
        let is_placeholder = self.content.is_empty();

        let lines: Vec<&str> = display_text.split('\n').collect();
        let line_count = lines.len();
        // Cap visible height at 8 lines, scroll beyond
        let visible_lines = line_count.min(8).max(1);
        let text_height = visible_lines as f32 * line_height;

        let text_color = if is_placeholder {
            hex(SUBTEXT0)
        } else {
            hex(TEXT)
        };

        // Build the text area content
        let mut text_area = div()
            .id("compose-text-area")
            .overflow_y_scroll()
            .w_full()
            .max_h(px(text_height))
            .min_h(px(line_height))
            .py(px(8.0))
            .px(px(12.0));

        for (i, line) in lines.iter().enumerate() {
            let line_text = if line.is_empty() && !is_placeholder {
                " ".to_string() // ensure empty lines have height
            } else {
                line.to_string()
            };
            text_area = text_area.child(
                div()
                    .text_size(px(font_size))
                    .text_color(text_color)
                    .child(line_text),
            );
        }

        // Send button
        let can_send = !self.content.trim().is_empty() && !self.busy;
        let send_color = if can_send { hex(BLUE) } else { hex_alpha(SUBTEXT0, 0.5) };
        let send_label = if self.busy { "..." } else { "Send" };

        let send_button = div()
            .id("compose-send-btn")
            .flex()
            .items_center()
            .justify_center()
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .bg(hex_alpha(SURFACE1, 0.6))
            .cursor(if can_send { CursorStyle::PointingHand } else { CursorStyle::default() })
            .child(
                div()
                    .text_size(px(font_size - 1.0))
                    .text_color(send_color)
                    .child(send_label),
            );

        // Compose bar container
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::newline))
            // Typing — handle key_down for regular characters
            .on_key_down(cx.listener(|this: &mut Self, event: &KeyDownEvent, _window, cx| {
                if this.busy { return; }
                // Let actions handle special keys; only insert printable characters
                if event.keystroke.key.len() == 1
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.control
                {
                    let ch = &event.keystroke.key;
                    this.replace_range(this.selected_range.clone(), ch, cx);
                }
            }))
            .w_full()
            .p(px(8.0))
            .border_t_1()
            .border_color(hex_alpha(SURFACE1, 0.5))
            .bg(hex(CRUST))
            .flex()
            .gap(px(8.0))
            .items_end()
            .child(
                div()
                    .flex_1()
                    .rounded(px(6.0))
                    .bg(hex_alpha(SURFACE0, 0.6))
                    .border_1()
                    .border_color(hex_alpha(SURFACE1, 0.5))
                    .cursor(CursorStyle::IBeam)
                    .child(text_area),
            )
            .child(send_button)
    }
}
