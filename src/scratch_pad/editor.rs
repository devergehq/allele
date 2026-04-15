//! Multi-line text editor for the Scratch Pad.
//!
//! A hand-rolled editor because GPUI has no reusable text-input widget in this
//! codebase. Supports cursor navigation (arrows / Home / End / word motion /
//! document motion), selection (shift-extend + Cmd+A), selection-replace, and
//! clipboard (Cmd+C / Cmd+X / Cmd+V). No undo.
//!
//! Rendering uses a char-per-span layout and a zero-width cursor bar overlay,
//! so we don't need precise font measurement — GPUI's flex layout gives us the
//! positions for free.
//!
//! Indices are **char** indices, not byte indices, so multi-byte codepoints
//! behave correctly under arrow keys.

use gpui::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize, // char index
}

pub enum KeyOutcome {
    Handled,
    Send,   // Cmd+Enter — caller should submit
    Close,  // Escape — caller should close
    Ignored,
}

pub struct ScratchEditor {
    lines: Vec<String>, // never empty; blank doc = vec![String::new()]
    cursor: Pos,
    anchor: Option<Pos>, // selection anchor; cursor is the extent
    pub focus: FocusHandle,
}

impl ScratchEditor {
    pub fn new(cx: &mut App) -> Self {
        Self {
            lines: vec![String::new()],
            cursor: Pos { line: 0, col: 0 },
            anchor: None,
            focus: cx.focus_handle(),
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Immutable view of the line buffer — used by the parent view to
    /// render per-line click targets.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Current cursor position.
    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    /// Char count of a given line, clamped if `line` is out of range.
    pub fn line_char_count(&self, line: usize) -> usize {
        self.line_len(line)
    }

    /// Move the cursor to `pos`, clamping to the valid document range.
    /// When `extend` is true, preserves / creates a selection anchor so
    /// the current selection grows to the new cursor (shift+click).
    /// When false, clears any selection.
    pub fn set_cursor(&mut self, pos: Pos, extend: bool) {
        let last_line = self.lines.len().saturating_sub(1);
        let line = pos.line.min(last_line);
        let col = pos.col.min(self.line_len(line));
        let clamped = Pos { line, col };
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = clamped;
    }

    // ── selection helpers ─────────────────────────────────────────────
    pub fn selection_range(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        let (start, end) = if anchor < self.cursor { (anchor, self.cursor) } else { (self.cursor, anchor) };
        Some((start, end))
    }

    fn clear_selection(&mut self) {
        self.anchor = None;
    }

    fn start_or_keep_selection(&mut self, shift: bool) {
        if shift {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else { return false; };
        if start.line == end.line {
            let line = &mut self.lines[start.line];
            let s_byte = char_to_byte(line, start.col);
            let e_byte = char_to_byte(line, end.col);
            line.replace_range(s_byte..e_byte, "");
        } else {
            let first_head = {
                let line = &self.lines[start.line];
                line[..char_to_byte(line, start.col)].to_string()
            };
            let last_tail = {
                let line = &self.lines[end.line];
                line[char_to_byte(line, end.col)..].to_string()
            };
            self.lines[start.line] = first_head + &last_tail;
            self.lines.drain((start.line + 1)..=end.line);
        }
        self.cursor = start;
        self.anchor = None;
        true
    }

    // ── cursor motion primitives ──────────────────────────────────────
    fn line_len(&self, line: usize) -> usize {
        self.lines.get(line).map(|l| l.chars().count()).unwrap_or(0)
    }

    fn move_left(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = self.line_len(self.cursor.line);
        }
    }

    fn move_right(&mut self) {
        let len = self.line_len(self.cursor.line);
        if self.cursor.col < len {
            self.cursor.col += 1;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor.line > 0 {
            self.cursor.line -= 1;
            self.cursor.col = self.cursor.col.min(self.line_len(self.cursor.line));
        } else {
            self.cursor.col = 0;
        }
    }

    fn move_down(&mut self) {
        if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.col = self.cursor.col.min(self.line_len(self.cursor.line));
        } else {
            self.cursor.col = self.line_len(self.cursor.line);
        }
    }

    fn move_word_left(&mut self) {
        if self.cursor.col == 0 {
            self.move_left();
            return;
        }
        let line: Vec<char> = self.lines[self.cursor.line].chars().collect();
        let mut i = self.cursor.col;
        while i > 0 && !line[i - 1].is_alphanumeric() { i -= 1; }
        while i > 0 && line[i - 1].is_alphanumeric() { i -= 1; }
        self.cursor.col = i;
    }

    fn move_word_right(&mut self) {
        let line: Vec<char> = self.lines[self.cursor.line].chars().collect();
        if self.cursor.col >= line.len() {
            self.move_right();
            return;
        }
        let mut i = self.cursor.col;
        while i < line.len() && !line[i].is_alphanumeric() { i += 1; }
        while i < line.len() && line[i].is_alphanumeric() { i += 1; }
        self.cursor.col = i;
    }

    // ── mutating primitives ───────────────────────────────────────────
    fn insert_text(&mut self, text: &str) {
        self.delete_selection();
        let mut chunks = text.split('\n').peekable();
        let first = chunks.next().unwrap_or("");
        {
            let line = &mut self.lines[self.cursor.line];
            let at = char_to_byte(line, self.cursor.col);
            line.insert_str(at, first);
        }
        self.cursor.col += first.chars().count();

        if chunks.peek().is_some() {
            let cur_line = &self.lines[self.cursor.line];
            let split_at = char_to_byte(cur_line, self.cursor.col);
            let tail = cur_line[split_at..].to_string();
            self.lines[self.cursor.line].truncate(split_at);

            let mut new_lines: Vec<String> = chunks.map(|s| s.to_string()).collect();
            if let Some(last) = new_lines.last_mut() {
                let new_col = last.chars().count();
                last.push_str(&tail);
                let insert_at = self.cursor.line + 1;
                let added = new_lines.len();
                self.lines.splice(insert_at..insert_at, new_lines);
                self.cursor.line += added;
                self.cursor.col = new_col;
            }
        }
    }

    fn insert_newline(&mut self) {
        self.delete_selection();
        let line = &self.lines[self.cursor.line];
        let split_at = char_to_byte(line, self.cursor.col);
        let tail = line[split_at..].to_string();
        self.lines[self.cursor.line].truncate(split_at);
        self.lines.insert(self.cursor.line + 1, tail);
        self.cursor.line += 1;
        self.cursor.col = 0;
    }

    fn backspace(&mut self) {
        if self.delete_selection() { return; }
        if self.cursor.col > 0 {
            let line = &mut self.lines[self.cursor.line];
            let prev_byte = char_to_byte(line, self.cursor.col - 1);
            let cur_byte = char_to_byte(line, self.cursor.col);
            line.replace_range(prev_byte..cur_byte, "");
            self.cursor.col -= 1;
        } else if self.cursor.line > 0 {
            let cur = self.lines.remove(self.cursor.line);
            let prev_len = self.line_len(self.cursor.line - 1);
            self.lines[self.cursor.line - 1].push_str(&cur);
            self.cursor.line -= 1;
            self.cursor.col = prev_len;
        }
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() { return; }
        let len = self.line_len(self.cursor.line);
        if self.cursor.col < len {
            let line = &mut self.lines[self.cursor.line];
            let cur_byte = char_to_byte(line, self.cursor.col);
            let next_byte = char_to_byte(line, self.cursor.col + 1);
            line.replace_range(cur_byte..next_byte, "");
        } else if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        }
    }

    fn select_all(&mut self) {
        self.anchor = Some(Pos { line: 0, col: 0 });
        let last_line = self.lines.len() - 1;
        self.cursor = Pos { line: last_line, col: self.line_len(last_line) };
    }

    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        if start.line == end.line {
            let line = &self.lines[start.line];
            let s = char_to_byte(line, start.col);
            let e = char_to_byte(line, end.col);
            return Some(line[s..e].to_string());
        }
        let mut out = String::new();
        let first = &self.lines[start.line];
        out.push_str(&first[char_to_byte(first, start.col)..]);
        out.push('\n');
        for i in (start.line + 1)..end.line {
            out.push_str(&self.lines[i]);
            out.push('\n');
        }
        let last = &self.lines[end.line];
        out.push_str(&last[..char_to_byte(last, end.col)]);
        Some(out)
    }

    // ── event entry point ─────────────────────────────────────────────
    pub fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut App,
    ) -> KeyOutcome {
        let key = event.keystroke.key.as_str();
        let mods = &event.keystroke.modifiers;
        let shift = mods.shift;
        let cmd = mods.platform;
        let alt = mods.alt;

        // Cmd+Enter = send; Escape = close (handled before selection/insert)
        if key == "enter" && cmd {
            return KeyOutcome::Send;
        }
        if key == "escape" {
            return KeyOutcome::Close;
        }

        // Clipboard (Cmd+C/X/V text only; image paste handled by parent)
        if cmd && !alt && !shift {
            match key {
                "a" => { self.select_all(); return KeyOutcome::Handled; }
                "c" => {
                    if let Some(text) = self.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    return KeyOutcome::Handled;
                }
                "x" => {
                    if let Some(text) = self.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.delete_selection();
                    }
                    return KeyOutcome::Handled;
                }
                "v" => {
                    if let Some(item) = cx.read_from_clipboard() {
                        if let Some(text) = item.text() {
                            self.insert_text(&text);
                        }
                    }
                    return KeyOutcome::Handled;
                }
                _ => {}
            }
        }

        // Motion
        match key {
            "left" => {
                self.start_or_keep_selection(shift);
                if cmd { self.cursor.col = 0; }
                else if alt { self.move_word_left(); }
                else { self.move_left(); }
                if !shift { self.clear_selection(); }
                return KeyOutcome::Handled;
            }
            "right" => {
                self.start_or_keep_selection(shift);
                if cmd { self.cursor.col = self.line_len(self.cursor.line); }
                else if alt { self.move_word_right(); }
                else { self.move_right(); }
                if !shift { self.clear_selection(); }
                return KeyOutcome::Handled;
            }
            "up" => {
                self.start_or_keep_selection(shift);
                if cmd { self.cursor = Pos { line: 0, col: 0 }; }
                else { self.move_up(); }
                if !shift { self.clear_selection(); }
                return KeyOutcome::Handled;
            }
            "down" => {
                self.start_or_keep_selection(shift);
                if cmd {
                    let last = self.lines.len() - 1;
                    self.cursor = Pos { line: last, col: self.line_len(last) };
                } else { self.move_down(); }
                if !shift { self.clear_selection(); }
                return KeyOutcome::Handled;
            }
            "home" => {
                self.start_or_keep_selection(shift);
                self.cursor.col = 0;
                if !shift { self.clear_selection(); }
                return KeyOutcome::Handled;
            }
            "end" => {
                self.start_or_keep_selection(shift);
                self.cursor.col = self.line_len(self.cursor.line);
                if !shift { self.clear_selection(); }
                return KeyOutcome::Handled;
            }
            _ => {}
        }

        // Editing
        match key {
            "enter" => { self.insert_newline(); return KeyOutcome::Handled; }
            "backspace" => { self.backspace(); return KeyOutcome::Handled; }
            "delete" => { self.delete_forward(); return KeyOutcome::Handled; }
            "tab" => { self.insert_text("  "); return KeyOutcome::Handled; }
            _ => {}
        }

        // Printable character
        if !cmd && !mods.control {
            if let Some(ch) = event.keystroke.key_char.as_deref() {
                if !ch.is_empty() {
                    self.insert_text(ch);
                    return KeyOutcome::Handled;
                }
            }
        }

        KeyOutcome::Ignored
    }

}

/// Convert a char index into a byte index within `s`.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}
