//! Markdown rendering for Rich Mode text blocks.
//!
//! Parses the assistant's markdown output with `pulldown-cmark` and produces a
//! GPUI tree. Block elements (paragraphs, headings, code blocks, lists) become
//! sibling divs. Inline elements (bold, italic, inline code, links) become
//! `TextRun`s inside a `StyledText` line.
//!
//! Pure function: `render(content, streaming, font_size) -> Div`. No memoisation,
//! no `Window` parameter — fonts are constructed inline.

use gpui::{
    div, px, Div, Font, FontFeatures, FontStyle, FontWeight, Hsla,
    ParentElement as _, Rgba, SharedString, StyledText, Styled as _, TextRun, UnderlineStyle,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

// ── Palette (Catppuccin Mocha — matches rich_view.rs) ─────────────

const SURFACE0: u32 = 0x313244;
const TEXT: u32 = 0xcdd6f4;
const SUBTEXT0: u32 = 0xa6adc8;
const SUBTEXT1: u32 = 0xbac2de;
const BLUE: u32 = 0x89b4fa;
const PEACH: u32 = 0xfab387;
const GREEN: u32 = 0xa6e3a1;

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

const MONO_FAMILY: &str = "JetBrains Mono";

fn body_font(bold: bool, italic: bool) -> Font {
    Font {
        family: "".into(),
        weight: if bold { FontWeight::BOLD } else { FontWeight::NORMAL },
        style: if italic { FontStyle::Italic } else { FontStyle::Normal },
        features: FontFeatures::default(),
        fallbacks: None,
    }
}

fn mono_font(bold: bool, italic: bool) -> Font {
    Font {
        family: MONO_FAMILY.into(),
        weight: if bold { FontWeight::BOLD } else { FontWeight::NORMAL },
        style: if italic { FontStyle::Italic } else { FontStyle::Normal },
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
        let color = if self.link { hex(BLUE) } else { base_color };
        let font = if self.code {
            mono_font(self.bold, self.italic)
        } else {
            body_font(self.bold, self.italic)
        };
        let background = if self.code {
            Some(hex_alpha(SURFACE0, 0.6))
        } else {
            None
        };
        let underline = if self.link {
            Some(UnderlineStyle {
                color: Some(hex(BLUE)),
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

/// Accumulates a single paragraph/heading's worth of inline text + runs.
struct InlineBuilder {
    text: String,
    runs: Vec<TextRun>,
    style: InlineStyle,
}

impl InlineBuilder {
    fn new() -> Self {
        Self { text: String::new(), runs: Vec::new(), style: InlineStyle::default() }
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

/// Render markdown-formatted text as a GPUI div tree. Pure function — safe to
/// re-call every frame; pulldown-cmark parses at hundreds of MB/s for the sizes
/// involved here.
pub fn render(content: &str, streaming: bool, font_size: f32) -> Div {
    let base_color = if streaming { hex(SUBTEXT1) } else { hex(TEXT) };

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(content, opts);

    let mut container = div().flex().flex_col();

    let mut inline = InlineBuilder::new();
    let mut current_heading: Option<HeadingLevel> = None;
    let mut in_code_block = false;
    let mut code_buffer = String::new();
    // list_stack entries: Some(n) = ordered list with next-number n, None = unordered.
    let mut list_stack: Vec<Option<u64>> = Vec::new();

    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => {
                // No-op: paragraph content accumulates into `inline`.
            }
            Event::End(TagEnd::Paragraph) => {
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(paragraph_element(text, runs, font_size));
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                // Flush any dangling inline content as a paragraph first.
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(paragraph_element(text, runs, font_size));
                }
                current_heading = Some(level);
                inline.style.bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                let level = current_heading.take().unwrap_or(HeadingLevel::H6);
                inline.style.bold = false;
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(heading_element(level, text, runs, font_size));
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(paragraph_element(text, runs, font_size));
                }
                in_code_block = true;
                code_buffer.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                container = container.child(code_block_element(
                    std::mem::take(&mut code_buffer),
                    font_size,
                ));
            }
            Event::Start(Tag::List(first_number)) => {
                // Flush any pending paragraph.
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(paragraph_element(text, runs, font_size));
                }
                list_stack.push(first_number);
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                let depth = list_stack.len().saturating_sub(1);
                let indent = " ".repeat(depth * 2);
                let prefix = match list_stack.last_mut() {
                    Some(Some(n)) => {
                        let p = format!("{indent}{n}. ");
                        *n += 1;
                        p
                    }
                    Some(None) => format!("{indent}• "),
                    None => String::new(),
                };
                inline.push(&prefix, base_color);
            }
            Event::End(TagEnd::Item) => {
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(list_item_element(text, runs, font_size));
                }
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
                if in_code_block {
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
                if let Some((text, runs)) = std::mem::replace(&mut inline, InlineBuilder::new()).finish() {
                    container = container.child(paragraph_element(text, runs, font_size));
                }
                container = container.child(
                    div()
                        .my(px(6.0))
                        .h(px(1.0))
                        .bg(hex_alpha(SUBTEXT0, 0.25)),
                );
            }
            _ => {
                // Ignore unhandled events (tables, images, footnotes, etc.) — v0.2 scope.
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
        container = container.child(code_block_element(code_buffer, font_size));
    }

    container
}

// ── Block builders ────────────────────────────────────────────────

fn paragraph_element(text: SharedString, runs: Vec<TextRun>, font_size: f32) -> Div {
    div()
        .py(px(2.0))
        .text_size(px(font_size))
        .child(StyledText::new(text).with_runs(runs))
}

fn heading_element(level: HeadingLevel, text: SharedString, runs: Vec<TextRun>, font_size: f32) -> Div {
    let (size, top, bottom) = match level {
        HeadingLevel::H1 => (font_size + 8.0, 10.0, 6.0),
        HeadingLevel::H2 => (font_size + 5.0, 8.0, 5.0),
        HeadingLevel::H3 => (font_size + 3.0, 6.0, 4.0),
        HeadingLevel::H4 => (font_size + 2.0, 5.0, 3.0),
        HeadingLevel::H5 => (font_size + 1.0, 4.0, 3.0),
        HeadingLevel::H6 => (font_size, 4.0, 2.0),
    };
    div()
        .mt(px(top))
        .mb(px(bottom))
        .text_size(px(size))
        .child(StyledText::new(text).with_runs(runs))
}

fn code_block_element(code: String, font_size: f32) -> Div {
    let code_size = font_size - 1.0;
    let base_color = hex(GREEN);
    // Preserve blank trailing newline handling: trim exactly one trailing \n.
    let trimmed = if code.ends_with('\n') {
        &code[..code.len() - 1]
    } else {
        &code
    };

    let mut block = div()
        .my(px(6.0))
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .bg(hex_alpha(SURFACE0, 0.6))
        .border_l_2()
        .border_color(hex_alpha(PEACH, 0.5))
        .flex()
        .flex_col();

    if trimmed.is_empty() {
        // Empty fenced code block — still give it some visible height so the
        // reader knows a block existed.
        block = block.child(
            div()
                .text_size(px(code_size))
                .h(px(code_size + 4.0))
                .text_color(hex_alpha(SUBTEXT0, 0.5))
                .child(""),
        );
        return block;
    }

    for line in trimmed.split('\n') {
        let line_len = line.len();
        let run = TextRun {
            len: line_len,
            font: mono_font(false, false),
            color: base_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        block = block.child(
            div()
                .text_size(px(code_size))
                .child(StyledText::new(SharedString::from(line.to_string())).with_runs(vec![run])),
        );
    }
    block
}

fn list_item_element(text: SharedString, runs: Vec<TextRun>, font_size: f32) -> Div {
    div()
        .py(px(1.0))
        .pl(px(4.0))
        .text_size(px(font_size))
        .child(StyledText::new(text).with_runs(runs))
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

```
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

1. first
2. second

---

Mid-stream **unterminated
"#;
        // Streaming = true, then false. Both paths must not panic.
        let _ = render(sample, true, 14.0);
        let _ = render(sample, false, 14.0);
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
}
