//! Markdown rendering for Rich Mode text blocks.
//!
//! Parses the assistant's markdown output with `pulldown-cmark` and produces a
//! GPUI tree. Block elements (paragraphs, headings, code blocks, lists) become
//! sibling divs. Inline elements (bold, italic, inline code, links) become
//! `TextRun`s inside a `StyledText` line.
//!
//! Pure function: `render(content, streaming, font_size) -> Div`. No memoisation,
//! no `Window` parameter — fonts are constructed inline.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::rc::Rc;

use crate::reader::highlight::{self, HlLine, TokenColors};
use crate::rich::column::prose_width;
use crate::theme::{theme, with_alpha};
use gpui::{
    div, px, Div, Font, FontFeatures, FontStyle, FontWeight, Hsla, ParentElement as _,
    SharedString, Styled as _, StyledText, TextRun, UnderlineStyle,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

// ── Palette (Catppuccin Mocha — matches rich_view.rs) ─────────────

const MONO_FAMILY: &str = crate::theme::FONT_MONO;

/// Vertical space between consecutive paragraphs. Heading margins are set
/// against this — a heading that does not clear it separates nothing.
const PARAGRAPH_GAP: f32 = 8.0;

fn body_font(bold: bool, italic: bool) -> Font {
    Font {
        family: "".into(),
        weight: if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
        features: FontFeatures::default(),
        fallbacks: None,
    }
}

fn mono_font(bold: bool, italic: bool) -> Font {
    Font {
        family: MONO_FAMILY.into(),
        weight: if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
        features: FontFeatures::disable_ligatures(),
        fallbacks: None,
    }
}

// ── Inline style flags ────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    code: bool,
    link: bool,
    strike: bool,
}

impl InlineStyle {
    fn to_run(self, len: usize, base_color: Hsla) -> TextRun {
        let color = if self.link {
            theme().accent
        } else {
            base_color
        };
        let font = if self.code {
            mono_font(self.bold, self.italic)
        } else {
            body_font(self.bold, self.italic)
        };
        let background = if self.code {
            Some(with_alpha(theme().bg_raised, 0.6))
        } else {
            None
        };
        let underline = if self.link {
            Some(UnderlineStyle {
                color: Some(theme().accent),
                thickness: px(1.0),
                wavy: false,
            })
        } else {
            None
        };
        let strikethrough = if self.strike {
            Some(gpui::StrikethroughStyle {
                color: Some(base_color),
                thickness: px(1.0),
            })
        } else {
            None
        };
        TextRun {
            len,
            font,
            color,
            background_color: background,
            underline,
            strikethrough,
        }
    }
}

/// A list item's bullet, number or checkbox.
///
/// Held apart from the item's content rather than pushed into the same
/// `StyledText`, so a wrapped line hangs under the text instead of under the
/// marker, and so nesting can be real indentation rather than leading spaces.
/// Both matter much more since DEV-571 narrowed the prose measure: list items
/// wrap far more often than they used to.
#[derive(Clone)]
struct ListMarker {
    text: String,
    color: Hsla,
}

/// Build the marker for the item starting now, advancing an ordered list's
/// counter. `None` for an item with no enclosing list, which malformed
/// markdown can produce.
fn list_marker(list_stack: &mut [Option<u64>], base_color: Hsla) -> Option<ListMarker> {
    match list_stack.last_mut() {
        Some(Some(n)) => {
            let text = format!("{n}.");
            *n += 1;
            Some(ListMarker {
                text,
                color: base_color,
            })
        }
        Some(None) => Some(ListMarker {
            text: "•".to_string(),
            color: base_color,
        }),
        None => None,
    }
}

/// One rendered table cell: its inline text and runs, or `None` when empty.
type TableCell = Option<(SharedString, Vec<TextRun>)>;

/// Accumulates a table's cells as pulldown-cmark walks it.
///
/// The subtlety this exists to hold: **pulldown-cmark wraps the header cells in
/// `TableHead` directly and emits no `TableRow` for them.** Committing the
/// header only on `End(TableRow)` therefore dropped it on the floor, and every
/// table rendered headerless. Alternating row backgrounds used to disguise
/// that; a single hairline rule does not.
#[derive(Default)]
struct TableBuilder {
    in_head: bool,
    current: Vec<TableCell>,
    header: Vec<TableCell>,
    body: Vec<Vec<TableCell>>,
}

impl TableBuilder {
    fn start_table(&mut self) {
        *self = Self::default();
    }

    fn start_head(&mut self) {
        self.in_head = true;
        self.current.clear();
    }

    /// Commit the header here, because no `TableRow` end will arrive for it.
    fn end_head(&mut self) {
        if !self.current.is_empty() {
            self.header = std::mem::take(&mut self.current);
        }
        self.in_head = false;
    }

    fn start_row(&mut self) {
        self.current.clear();
    }

    /// Kept branching on `in_head` as well: a parser that *does* emit a row
    /// inside the head must not push the header into the body.
    fn end_row(&mut self) {
        let row = std::mem::take(&mut self.current);
        if self.in_head {
            self.header = row;
        } else {
            self.body.push(row);
        }
    }

    fn push_cell(&mut self, cell: TableCell) {
        self.current.push(cell);
    }

    fn finish(&mut self) -> (Vec<TableCell>, Vec<Vec<TableCell>>) {
        (
            std::mem::take(&mut self.header),
            std::mem::take(&mut self.body),
        )
    }
}

/// Accumulates a single paragraph/heading's worth of inline text + runs.
struct InlineBuilder {
    text: String,
    runs: Vec<TextRun>,
    style: InlineStyle,
}

impl InlineBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            runs: Vec::new(),
            style: InlineStyle::default(),
        }
    }

    fn push(&mut self, segment: &str, base_color: Hsla) {
        if segment.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(segment);
        let len = self.text.len() - start;
        self.runs.push(self.style.to_run(len, base_color));
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn finish(self) -> Option<(SharedString, Vec<TextRun>)> {
        if self.text.is_empty() {
            return None;
        }
        debug_assert_eq!(
            self.runs.iter().map(|r| r.len).sum::<usize>(),
            self.text.len(),
            "TextRun len sum must equal text byte length"
        );
        Some((self.text.into(), self.runs))
    }
}

// ── Public API ────────────────────────────────────────────────────

/// Close off whatever inline content has accumulated and append it to
/// `container`, as a list item when we are inside a list and a paragraph
/// otherwise. `marker` is consumed, so the second paragraph of a loose list
/// item indents to match its first without repeating the bullet.
fn flush_inline(
    container: Div,
    inline: &mut InlineBuilder,
    list_depth: usize,
    marker: &mut Option<ListMarker>,
    font_size: f32,
) -> Div {
    let Some((text, runs)) = std::mem::replace(inline, InlineBuilder::new()).finish() else {
        return container;
    };
    if list_depth > 0 {
        container.child(list_item_element(
            marker.take(),
            list_depth,
            text,
            runs,
            font_size,
        ))
    } else {
        container.child(paragraph_element(text, runs, font_size))
    }
}

/// Render markdown-formatted text as a GPUI div tree. Pure function — safe to
/// re-call every frame; pulldown-cmark parses at hundreds of MB/s for the sizes
/// involved here.
pub fn render(content: &str, streaming: bool, font_size: f32) -> Div {
    let base_color = if streaming {
        theme().text_body
    } else {
        theme().text_primary
    };

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(content, opts);

    let mut container = div().flex().flex_col();
    // Parent containers while inside blockquotes: on BlockQuote start we swap
    // `container` for a fresh body and push the parent here; on end we wrap the
    // body and append it back to the popped parent. Supports nesting.
    let mut parents: Vec<Div> = Vec::new();

    // Image capture: alt text arrives as Text events between Start/End(Image).
    let mut in_image = false;
    let mut image_alt = String::new();
    let mut image_dest = String::new();

    let mut inline = InlineBuilder::new();
    let mut current_heading: Option<HeadingLevel> = None;
    let mut in_code_block = false;
    let mut code_buffer = String::new();
    let mut code_lang = String::new();
    // list_stack entries: Some(n) = ordered list with next-number n, None = unordered.
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    // The current item's marker, held until its content is flushed so
    // TaskListMarker can replace a bullet with a checkbox first. It is never
    // pushed into `inline` — that is what cost the hanging indent.
    let mut pending_marker: Option<ListMarker> = None;

    let mut table = TableBuilder::default();

    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => {
                // No-op: paragraph content accumulates into `inline`.
            }
            Event::End(TagEnd::Paragraph) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
            }
            Event::Start(Tag::Heading { level, .. }) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                current_heading = Some(level);
                inline.style.bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                let level = current_heading.take().unwrap_or(HeadingLevel::H6);
                inline.style.bold = false;
                if let Some((text, runs)) =
                    std::mem::replace(&mut inline, InlineBuilder::new()).finish()
                {
                    container = container.child(heading_element(level, text, runs, font_size));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                in_code_block = true;
                code_buffer.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => lang.to_string(),
                    _ => String::new(),
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                container = container.child(code_block_element(
                    std::mem::take(&mut code_buffer),
                    std::mem::take(&mut code_lang),
                    font_size,
                ));
            }
            Event::Start(Tag::List(first_number)) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                list_stack.push(first_number);
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                pending_marker = list_marker(&mut list_stack, base_color);
            }
            Event::End(TagEnd::Item) => {
                // Tight items flush here. A loose item flushed at its paragraph
                // end, which consumed the marker, so this is a no-op for those
                // and for an item with no content at all.
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                pending_marker = None;
            }
            // ── Task list checkboxes ──────────────────────────────
            Event::TaskListMarker(checked) => {
                // The checkbox replaces the bullet outright.
                pending_marker = Some(if checked {
                    ListMarker {
                        text: "☑".to_string(),
                        color: theme().success,
                    }
                } else {
                    ListMarker {
                        text: "☐".to_string(),
                        color: with_alpha(theme().text_secondary, 0.6),
                    }
                });
            }
            Event::Start(Tag::Emphasis) => inline.style.italic = true,
            Event::End(TagEnd::Emphasis) => inline.style.italic = false,
            Event::Start(Tag::Strong) => inline.style.bold = true,
            Event::End(TagEnd::Strong) => inline.style.bold = false,
            Event::Start(Tag::Strikethrough) => inline.style.strike = true,
            Event::End(TagEnd::Strikethrough) => inline.style.strike = false,
            Event::Start(Tag::Link { .. }) => inline.style.link = true,
            Event::End(TagEnd::Link) => inline.style.link = false,
            Event::Code(s) => {
                let was = inline.style.code;
                inline.style.code = true;
                inline.push(&s, base_color);
                inline.style.code = was;
            }
            Event::Text(s) => {
                if in_image {
                    // Alt text — collected for the image placeholder, not body.
                    image_alt.push_str(&s);
                } else if in_code_block {
                    code_buffer.push_str(&s);
                } else {
                    inline.push(&s, base_color);
                }
            }
            Event::SoftBreak => {
                if !in_code_block {
                    inline.push(" ", base_color);
                }
            }
            Event::HardBreak => {
                if !in_code_block {
                    inline.push("\n", base_color);
                }
            }
            Event::Rule => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                container = container.child(
                    div()
                        .my(px(6.0))
                        .h(px(1.0))
                        .bg(with_alpha(theme().text_secondary, 0.25)),
                );
            }
            // ── Tables ────────────────────────────────────────────
            Event::Start(Tag::Table(_)) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                table.start_table();
            }
            Event::End(TagEnd::Table) => {
                let (header, rows) = table.finish();
                container = container.child(table_element(header, rows, font_size));
            }
            Event::Start(Tag::TableHead) => table.start_head(),
            Event::End(TagEnd::TableHead) => table.end_head(),
            Event::Start(Tag::TableRow) => table.start_row(),
            Event::End(TagEnd::TableRow) => table.end_row(),
            Event::Start(Tag::TableCell) => {
                inline = InlineBuilder::new();
            }
            Event::End(TagEnd::TableCell) => {
                let cell = std::mem::replace(&mut inline, InlineBuilder::new()).finish();
                table.push_cell(cell);
            }
            // ── Blockquotes ───────────────────────────────────────
            Event::Start(Tag::BlockQuote(_)) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                // Start a fresh body for the quote; remember the parent.
                parents.push(std::mem::replace(&mut container, div().flex().flex_col()));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                let body = std::mem::replace(&mut container, div());
                let parent = parents.pop().unwrap_or_else(|| div().flex().flex_col());
                let quoted = div()
                    .my(px(8.0))
                    .pl(px(12.0))
                    .border_l_2()
                    .border_color(with_alpha(theme().text_secondary, 0.4))
                    .child(body);
                container = parent.child(quoted);
            }
            // ── Images ────────────────────────────────────────────
            Event::Start(Tag::Image { dest_url, .. }) => {
                in_image = true;
                image_alt.clear();
                image_dest = dest_url.to_string();
            }
            Event::End(TagEnd::Image) => {
                in_image = false;
                // Flush any inline text before the image sits on its own line.
                container = flush_inline(
                    container,
                    &mut inline,
                    list_stack.len(),
                    &mut pending_marker,
                    font_size,
                );
                container = container.child(image_element(
                    std::mem::take(&mut image_alt),
                    std::mem::take(&mut image_dest),
                    font_size,
                ));
            }
            _ => {
                // Ignore still-unhandled events (footnotes, inline HTML, …).
            }
        }
    }

    // Flush any trailing inline content (streaming: last paragraph may not be
    // terminated yet because the assistant is still generating).
    if !inline.is_empty() {
        if let Some((text, runs)) = inline.finish() {
            container = container.child(paragraph_element(text, runs, font_size));
        }
    }

    // Flush any trailing code block content that never got an End event
    // (streaming: the closing ``` has not arrived yet).
    if in_code_block && !code_buffer.is_empty() {
        container = container.child(code_block_element(
            code_buffer,
            std::mem::take(&mut code_lang),
            font_size,
        ));
    }

    // Unwind any unterminated blockquotes (streaming: closing not seen yet).
    while let Some(parent) = parents.pop() {
        let body = std::mem::replace(&mut container, div());
        let quoted = div()
            .my(px(8.0))
            .pl(px(12.0))
            .border_l_2()
            .border_color(with_alpha(theme().text_secondary, 0.4))
            .child(body);
        container = parent.child(quoted);
    }

    container
}

/// Placeholder for an image (we don't fetch/decode image data): an icon plus
/// the alt text, with the destination shown muted when both are present.
fn image_element(alt: String, dest: String, font_size: f32) -> Div {
    let has_alt = !alt.trim().is_empty();
    let primary = if has_alt {
        alt
    } else if !dest.is_empty() {
        dest.clone()
    } else {
        "image".to_string()
    };
    let mut row = div()
        .my(px(6.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .bg(with_alpha(theme().bg_raised, 0.5))
        .border_1()
        .border_color(with_alpha(theme().text_secondary, 0.25))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .text_size(px(font_size - 1.0))
        .child(div().text_color(theme().text_secondary).child("🖼"))
        .child(div().text_color(theme().text_secondary).child(primary));
    if has_alt && !dest.is_empty() {
        row = row.child(
            div()
                .text_size(px((font_size - 3.0).max(9.0)))
                .text_color(with_alpha(theme().text_secondary, 0.6))
                .child(dest),
        );
    }
    row
}

// ── Block builders ────────────────────────────────────────────────

fn paragraph_element(text: SharedString, runs: Vec<TextRun>, font_size: f32) -> Div {
    // DEV-571: running text wraps at the prose measure, not the block frame.
    // Code blocks and tables deliberately keep the full frame — their width is
    // determined by their content, and clamping them loses information.
    div()
        .max_w(prose_width(font_size))
        .py(px(PARAGRAPH_GAP / 2.0))
        .text_size(px(font_size))
        .child(StyledText::new(text).with_runs(runs))
}

/// Type size and vertical margins for a heading level, as
/// `(size, margin_top, margin_bottom)`.
///
/// DEV-573: paragraphs sit [`PARAGRAPH_GAP`] apart, so the old 10px heading
/// top-margin separated nothing — hierarchy did not read at all. Top space now
/// clearly exceeds inter-paragraph space, and stays larger than the bottom, so
/// a heading binds to the text it introduces rather than to the text above it.
fn heading_spacing(level: HeadingLevel, font_size: f32) -> (f32, f32, f32) {
    match level {
        HeadingLevel::H1 => (font_size + 8.0, 22.0, 6.0),
        HeadingLevel::H2 => (font_size + 5.0, 18.0, 5.0),
        HeadingLevel::H3 => (font_size + 3.0, 14.0, 4.0),
        HeadingLevel::H4 => (font_size + 2.0, 11.0, 3.0),
        HeadingLevel::H5 => (font_size + 1.0, 9.0, 3.0),
        HeadingLevel::H6 => (font_size, 9.0, 2.0),
    }
}

fn heading_element(
    level: HeadingLevel,
    text: SharedString,
    runs: Vec<TextRun>,
    font_size: f32,
) -> Div {
    let (size, top, bottom) = heading_spacing(level, font_size);
    let text_div = div()
        .max_w(prose_width(font_size))
        .text_size(px(size))
        .child(StyledText::new(text).with_runs(runs));

    // No left accent bar: DEV-29 uses a left bar to mark narrative roles
    // (decisions, outcomes), and the same token cannot mean two things in one
    // column. Size and space carry the hierarchy on their own.
    div().mt(px(top)).mb(px(bottom)).child(text_div)
}

// Note: this reaches into `reader::highlight`, while `reader` renders Markdown
// through this module — a cycle between two feature modules. The shared
// highlighter wants to live in a neutral module both consume; tracked
// separately rather than widening this change.

/// Cache of highlighted fences, keyed by content + language + palette.
///
/// `render` is documented as safe to call every frame, and GPUI does exactly
/// that for every visible block. Only the tree-sitter *configuration* is
/// cached upstream (`reader::ts_highlight`) — `Highlighter::highlight` still
/// reparses the whole fence on each call, so without this a screenful of code
/// would be reparsed at frame rate. The palette is part of the key because
/// token colours are baked into the cached `TextRun`s, so a theme change must
/// miss rather than serve stale colours.
///
/// Thread-local because highlighting runs on the render thread, matching the
/// grammar cache it sits in front of.
type FenceCache = HashMap<u64, Rc<Vec<HlLine>>>;

thread_local! {
    static FENCE_CACHE: RefCell<FenceCache> = RefCell::new(HashMap::new());
}

/// Entries retained before the cache is dropped wholesale. A transcript shows
/// far fewer distinct fences than this at once; the bound exists so a very long
/// session cannot grow it without limit.
const FENCE_CACHE_CAPACITY: usize = 128;

fn fence_key(code: &str, ext: &str, colors: &TokenColors) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    ext.hash(&mut hasher);
    // Hsla is not Hash; its bit patterns identify the palette well enough to
    // detect a theme swap.
    for c in [colors.text, colors.comment, colors.keyword, colors.string] {
        c.h.to_bits().hash(&mut hasher);
        c.s.to_bits().hash(&mut hasher);
        c.l.to_bits().hash(&mut hasher);
        c.a.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// Highlight a fence, reusing the previous frame's result when nothing changed.
fn highlighted_fence(code: &str, ext: &str, colors: TokenColors) -> Rc<Vec<HlLine>> {
    let key = fence_key(code, ext, &colors);
    FENCE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(hit) = cache.get(&key) {
            return Rc::clone(hit);
        }
        if cache.len() >= FENCE_CACHE_CAPACITY {
            cache.clear();
        }
        let lines = Rc::new(highlight::highlight(code, ext, colors));
        cache.insert(key, Rc::clone(&lines));
        lines
    })
}

/// Map a fence's language tag to the file extension the highlighter keys on.
/// Anything not listed renders as plain monospace rather than being guessed at.
fn ext_for_fence_lang(lang: &str) -> Option<&'static str> {
    Some(match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rs",
        "typescript" | "ts" => "ts",
        "tsx" | "jsx" | "javascript" | "js" => "tsx",
        "python" | "py" => "py",
        "go" | "golang" => "go",
        "json" => "json",
        "bash" | "sh" | "shell" | "zsh" | "console" => "sh",
        "php" => "php",
        _ => return None,
    })
}

fn code_block_element(code: String, lang: String, font_size: f32) -> Div {
    let code_size = font_size - 1.0;
    let trimmed = if code.ends_with('\n') {
        &code[..code.len() - 1]
    } else {
        &code
    };

    // DEV-573: the surface tint alone says "this is code". The old block also
    // spent a peach left border and a peach language label saying the same
    // thing, and painted every token one flat green — three colour channels
    // encoding a single bit, with none left over for syntax. Highlighting
    // needs those channels back.
    let mut block = div()
        .my(px(10.0))
        .rounded(px(6.0))
        .bg(with_alpha(theme().bg_raised, 0.6))
        .flex()
        .flex_col();

    let has_lang = !lang.is_empty();
    if has_lang {
        block = block.child(
            div()
                .px(px(10.0))
                .pt(px(5.0))
                .pb(px(1.0))
                .text_size(px(font_size - 3.0).max(px(9.0)))
                .text_color(theme().text_faint)
                .font_family(MONO_FAMILY)
                .child(lang.clone()),
        );
    }

    let padding_top = if has_lang { px(3.0) } else { px(6.0) };

    if trimmed.is_empty() {
        block = block.child(
            div()
                .px(px(10.0))
                .pt(padding_top)
                .pb(px(6.0))
                .text_size(px(code_size))
                .h(px(code_size + 4.0))
                .text_color(with_alpha(theme().text_secondary, 0.5))
                .child(""),
        );
        return block;
    }

    let mut content = div()
        .px(px(10.0))
        .pt(padding_top)
        .pb(px(6.0))
        .flex()
        .flex_col();

    // A recognised fence language goes through the Reader's highlighter
    // (DEV-73), sharing its token palette so the same class is the same colour
    // everywhere. An unrecognised or absent tag renders as plain monospace —
    // guessing a grammar colours code wrongly, which is worse than not at all.
    match ext_for_fence_lang(&lang) {
        Some(ext) => {
            for line in highlighted_fence(trimmed, ext, highlight::theme_colors()).iter() {
                content = content.child(
                    div()
                        .text_size(px(code_size))
                        .child(StyledText::new(line.text.clone()).with_runs(line.runs.clone())),
                );
            }
        }
        None => {
            for line in trimmed.split('\n') {
                let run = TextRun {
                    len: line.len(),
                    font: mono_font(false, false),
                    color: theme().text_body,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                content = content.child(div().text_size(px(code_size)).child(
                    StyledText::new(SharedString::from(line.to_string())).with_runs(vec![run]),
                ));
            }
        }
    }
    block.child(content)
}

/// One list item: marker in its own cell, content in a flexible one beside it.
///
/// The two-cell layout is what gives a wrapped line its hanging indent — the
/// content column starts to the right of the marker and stays there. Nesting is
/// left padding rather than leading spaces, for the same reason: spaces live
/// inside the text run and vanish the moment it wraps.
///
/// `marker` is `None` for the second and later paragraphs of a loose item,
/// which align with the item's text but do not repeat its bullet.
fn list_item_element(
    marker: Option<ListMarker>,
    depth: usize,
    text: SharedString,
    runs: Vec<TextRun>,
    font_size: f32,
) -> Div {
    // Wide enough for "10." at the body size, so ordered lists do not shift
    // their content column as they pass nine items.
    let marker_w = px(font_size * 1.7);
    let indent = px(depth.saturating_sub(1) as f32 * font_size * 1.2);

    div()
        .max_w(prose_width(font_size))
        .py(px(1.0))
        .pl(indent)
        .flex()
        .items_start()
        .child(match marker {
            Some(m) => div()
                .flex_shrink_0()
                .w(marker_w)
                .text_size(px(font_size))
                .text_color(m.color)
                .child(m.text),
            None => div().flex_shrink_0().w(marker_w),
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(font_size))
                .child(StyledText::new(text).with_runs(runs)),
        )
}

fn table_element(
    header: Vec<Option<(SharedString, Vec<TextRun>)>>,
    rows: Vec<Vec<Option<(SharedString, Vec<TextRun>)>>>,
    font_size: f32,
) -> Div {
    let cell_size = font_size - 1.0;

    // DEV-573: rows are separated by a hairline rule, not by alternating
    // backgrounds. Row parity carries almost no signal at transcript density,
    // and striping spends a lot of ink to convey it. The rule does the same
    // separating job for a fraction of the weight.
    // 0.35, not the 0.2 this started at: once a cell wraps to two lines a
    // near-invisible rule stops associating the row, and a value can be read
    // against the wrong record.
    let rule = with_alpha(theme().text_faint, 0.35);

    let mut table = div()
        .my(px(8.0))
        .w_full()
        .min_w_0()
        .rounded(px(6.0))
        .overflow_hidden()
        .flex()
        .flex_col();

    if !header.is_empty() {
        let mut row = div()
            .w_full()
            .min_w_0()
            .flex()
            .border_b_1()
            .border_color(with_alpha(theme().text_faint, 0.45));
        for cell in header {
            let content = if let Some((text, runs)) = cell {
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(8.0))
                    .py(px(5.0))
                    .text_size(px(cell_size))
                    .text_color(theme().text_secondary)
                    .font_weight(FontWeight::BOLD)
                    .child(StyledText::new(text).with_runs(runs))
            } else {
                div().flex_1().min_w_0().px(px(8.0)).py(px(5.0))
            };
            row = row.child(content);
        }
        table = table.child(row);
    }

    let last = rows.len().saturating_sub(1);
    for (i, row_cells) in rows.into_iter().enumerate() {
        let mut row = div().w_full().min_w_0().flex();
        if i != last {
            row = row.border_b_1().border_color(rule);
        }
        for cell in row_cells {
            let content = if let Some((text, runs)) = cell {
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(8.0))
                    .py(px(4.0))
                    .text_size(px(cell_size))
                    .text_color(theme().text_primary)
                    .child(StyledText::new(text).with_runs(runs))
            } else {
                div().flex_1().min_w_0().px(px(8.0)).py(px(4.0))
            };
            row = row.child(content);
        }
        table = table.child(row);
    }

    table
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the parser with a representative sample covering every event type
    /// we handle. The debug_asserts inside `InlineBuilder::finish` fire on any
    /// TextRun-length drift, so a successful run here proves the invariant
    /// holds across nested and edge-case markdown.
    #[test]
    fn parses_representative_markdown_without_panic() {
        let sample = r#"# Heading One

A paragraph with **bold**, *italic*, `inline code`, ~~strike~~, and a [link](https://example.com).

## H2

### H3

Nested styles: **bold with *italic inside***.

```rust
fn main() {
    println!("hello");
}
```

```
```

- bullet one
- bullet **two**
  - nested
- [link](x)

- [ ] unchecked task
- [x] completed task
- [ ] another open item

1. first
2. second

| Name | Region | Status |
|------|--------|--------|
| prod | us-east-1 | **running** |
| uat  | ap-southeast-2 | stopped |

---

Mid-stream **unterminated
"#;
        // Streaming = true, then false. Both paths must not panic.
        let _ = render(sample, true, 14.0);
        let _ = render(sample, false, 14.0);
    }

    #[test]
    fn heading_top_margin_clears_the_paragraph_gap() {
        // A heading whose top margin does not exceed the space between two
        // paragraphs is invisible as a separator — the DEV-573 bug.
        for level in [
            HeadingLevel::H1,
            HeadingLevel::H2,
            HeadingLevel::H3,
            HeadingLevel::H4,
            HeadingLevel::H5,
            HeadingLevel::H6,
        ] {
            let (_, top, _) = heading_spacing(level, 14.0);
            assert!(
                top > PARAGRAPH_GAP,
                "{level:?} top margin {top} must exceed the {PARAGRAPH_GAP}px paragraph gap"
            );
        }
    }

    #[test]
    fn headings_bind_downward_to_the_text_they_introduce() {
        for level in [
            HeadingLevel::H1,
            HeadingLevel::H2,
            HeadingLevel::H3,
            HeadingLevel::H4,
            HeadingLevel::H5,
            HeadingLevel::H6,
        ] {
            let (_, top, bottom) = heading_spacing(level, 14.0);
            assert!(
                top > bottom,
                "{level:?} must sit closer to what follows it than to what precedes it"
            );
        }
    }

    #[test]
    fn heading_hierarchy_is_monotonic() {
        let levels = [
            HeadingLevel::H1,
            HeadingLevel::H2,
            HeadingLevel::H3,
            HeadingLevel::H4,
            HeadingLevel::H5,
            HeadingLevel::H6,
        ];
        for pair in levels.windows(2) {
            let (size_a, top_a, _) = heading_spacing(pair[0], 14.0);
            let (size_b, top_b, _) = heading_spacing(pair[1], 14.0);
            assert!(size_a >= size_b, "type size must not grow as depth grows");
            assert!(top_a >= top_b, "top margin must not grow as depth grows");
        }
    }

    #[test]
    fn known_fence_languages_map_to_a_highlighted_extension() {
        assert_eq!(ext_for_fence_lang("rust"), Some("rs"));
        assert_eq!(ext_for_fence_lang("Python"), Some("py"));
        assert_eq!(ext_for_fence_lang("  BASH  "), Some("sh"));
        assert_eq!(ext_for_fence_lang("js"), Some("tsx"));
    }

    #[test]
    fn unknown_fence_languages_are_not_guessed_at() {
        // Guessing a grammar colours code wrongly, which is worse than
        // rendering it plain.
        assert_eq!(ext_for_fence_lang(""), None);
        assert_eq!(ext_for_fence_lang("brainfuck"), None);
        assert_eq!(ext_for_fence_lang("text"), None);
    }

    #[test]
    fn highlighted_and_plain_fences_both_render() {
        let highlighted = "```rust\nfn main() { let x = \"hi\"; }\n```\n";
        let plain = "```brainfuck\n+++++[->+++<]\n```\n";
        let untagged = "```\nno language tag\n```\n";
        for sample in [highlighted, plain, untagged] {
            let _ = render(sample, false, 14.0);
            let _ = render(sample, true, 14.0);
        }
    }

    #[test]
    fn identical_fences_reuse_the_cached_highlight() {
        // The whole point of the cache: a fence that has not changed must not
        // be reparsed on the next frame.
        let first = highlighted_fence("fn main() { let x = 1; }", "rs", highlight::theme_colors());
        let second = highlighted_fence("fn main() { let x = 1; }", "rs", highlight::theme_colors());
        assert!(
            Rc::ptr_eq(&first, &second),
            "an unchanged fence must be served from the cache"
        );
    }

    #[test]
    fn a_different_palette_misses_the_cache() {
        // Token colours are baked into the cached runs, so a theme swap has to
        // miss rather than serve stale colours.
        let colors = highlight::theme_colors();
        let mut swapped = colors;
        swapped.keyword = colors.string;
        assert_ne!(
            fence_key("fn main() {}", "rs", &colors),
            fence_key("fn main() {}", "rs", &swapped),
            "a palette change must change the cache key"
        );
    }

    #[test]
    fn the_fence_cache_stays_bounded() {
        for i in 0..(FENCE_CACHE_CAPACITY * 2) {
            let _ = highlighted_fence(&format!("let x{i} = {i};"), "rs", highlight::theme_colors());
        }
        FENCE_CACHE.with(|cache| {
            assert!(
                cache.borrow().len() <= FENCE_CACHE_CAPACITY,
                "the cache must not grow without bound across a long session"
            );
        });
    }

    #[test]
    fn unordered_items_all_get_the_same_marker() {
        let mut stack = vec![None];
        let c = theme().text_primary;
        for _ in 0..3 {
            assert_eq!(list_marker(&mut stack, c).unwrap().text, "•");
        }
    }

    #[test]
    fn ordered_items_count_up() {
        let mut stack = vec![Some(1)];
        let c = theme().text_primary;
        let seen: Vec<String> = (0..3)
            .map(|_| list_marker(&mut stack, c).unwrap().text)
            .collect();
        assert_eq!(seen, vec!["1.", "2.", "3."]);
    }

    #[test]
    fn an_ordered_list_can_start_anywhere() {
        let mut stack = vec![Some(7)];
        assert_eq!(
            list_marker(&mut stack, theme().text_primary).unwrap().text,
            "7."
        );
    }

    #[test]
    fn an_item_outside_any_list_has_no_marker() {
        // Malformed markdown can emit Item without List; it must not panic.
        let mut stack: Vec<Option<u64>> = Vec::new();
        assert!(list_marker(&mut stack, theme().text_primary).is_none());
    }

    #[test]
    fn markers_carry_no_layout_in_their_text() {
        // The marker used to be a padded string ("  • ") because indentation
        // and spacing lived inside the text run — which is exactly why a
        // wrapped line lost its hanging indent. Both are layout now.
        let mut stack = vec![None, None, None];
        let m = list_marker(&mut stack, theme().text_primary).unwrap();
        assert_eq!(m.text.trim(), m.text, "no padding inside the marker text");
        assert!(!m.text.contains(' '), "no spacing inside the marker text");
    }

    #[test]
    fn nested_and_wrapping_lists_render() {
        let sample = "- a very long bullet that will certainly wrap at any sane \
prose measure and must hang under its own text\n  - nested\n    - deeper\n\n\
1. one\n2. two\n\n- [ ] open\n- [x] done\n\n\
- loose item\n\n  second paragraph of the same item\n";
        let _ = render(sample, false, 14.0);
        let _ = render(sample, true, 14.0);
    }

    #[test]
    fn a_table_header_survives_pulldown_cmarks_event_shape() {
        // pulldown-cmark wraps header cells in TableHead and emits NO TableRow
        // for them. Committing the header only on end_row dropped it entirely,
        // and every table rendered headerless.
        let mut t = TableBuilder::default();
        t.start_table();
        t.start_head();
        t.push_cell(Some(("Element".into(), vec![])));
        t.push_cell(Some(("Before".into(), vec![])));
        t.end_head();
        t.start_row();
        t.push_cell(Some(("code fence".into(), vec![])));
        t.push_cell(Some(("flat green".into(), vec![])));
        t.end_row();
        let (header, body) = t.finish();
        assert_eq!(header.len(), 2, "the header row must survive");
        assert_eq!(body.len(), 1, "and must not be counted as a body row");
    }

    #[test]
    fn a_header_row_is_still_handled_if_one_is_emitted() {
        // Defensive: a parser that does emit a TableRow inside the head must
        // not push the header into the body.
        let mut t = TableBuilder::default();
        t.start_table();
        t.start_head();
        t.start_row();
        t.push_cell(Some(("Element".into(), vec![])));
        t.end_row();
        t.end_head();
        let (header, body) = t.finish();
        assert_eq!(header.len(), 1);
        assert!(body.is_empty());
    }

    #[test]
    fn a_headerless_table_keeps_every_row_in_the_body() {
        let mut t = TableBuilder::default();
        t.start_table();
        for _ in 0..3 {
            t.start_row();
            t.push_cell(Some(("x".into(), vec![])));
            t.end_row();
        }
        let (header, body) = t.finish();
        assert!(header.is_empty());
        assert_eq!(body.len(), 3);
    }

    #[test]
    fn each_table_starts_clean() {
        let mut t = TableBuilder::default();
        t.start_table();
        t.start_head();
        t.push_cell(Some(("stale".into(), vec![])));
        t.end_head();
        t.start_table();
        let (header, body) = t.finish();
        assert!(
            header.is_empty(),
            "a new table must not inherit the last one"
        );
        assert!(body.is_empty());
    }

    #[test]
    fn empty_input_does_not_panic() {
        let _ = render("", false, 14.0);
        let _ = render("", true, 14.0);
    }

    #[test]
    fn plain_text_does_not_panic() {
        let _ = render("Just some normal text without any markdown.", false, 14.0);
    }

    #[test]
    fn task_list_does_not_panic() {
        let _ = render("- [ ] open\n- [x] done\n- [ ] another\n", false, 14.0);
        let _ = render("- [ ] streaming\n- [x] done\n", true, 14.0);
    }
}
