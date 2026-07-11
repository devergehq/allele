//! Lightweight, dependency-free syntax highlighting for the source Reader.
//!
//! This is deliberately not a full parser: it is a per-character lexer that
//! recognises line comments, block comments, string/char literals, numbers,
//! and a broad union of keywords across common languages. The Reader only
//! needs "readable and navigable", not semantic accuracy (DEV-37), so a fast
//! heuristic tokenizer that never panics is the right trade-off.

use gpui::{Font, FontFeatures, FontStyle, FontWeight, Hsla, SharedString, TextRun};

/// Theme colors a token class maps onto. Filled from `theme()` by the caller
/// so highlighting tracks light/dark automatically.
#[derive(Clone, Copy)]
pub(crate) struct TokenColors {
    pub text: Hsla,
    pub comment: Hsla,
    pub string: Hsla,
    pub keyword: Hsla,
    pub number: Hsla,
}

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Text,
    Comment,
    Str,
    Keyword,
    Number,
}

/// Comment/string syntax for a language family.
struct Syntax {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    /// Quote characters that open/close single-line string literals.
    quotes: &'static [char],
    keywords: &'static [&'static str],
}

const KW_RUSTLIKE: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "const",
    "continue",
    "crate",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "class",
    "def",
    "function",
    "var",
    "new",
    "public",
    "private",
    "protected",
    "import",
    "from",
    "export",
    "default",
    "interface",
    "extends",
    "implements",
    "package",
    "func",
    "go",
    "defer",
    "chan",
    "map",
    "range",
    "nil",
    "null",
    "None",
    "True",
    "False",
    "and",
    "or",
    "not",
    "is",
    "lambda",
    "try",
    "except",
    "finally",
    "raise",
    "with",
    "yield",
    "throw",
    "catch",
    "switch",
    "case",
    "typeof",
    "instanceof",
    "void",
    "int",
    "string",
    "bool",
    "float",
    "double",
    "char",
    "long",
    "short",
    "unsigned",
    "signed",
    "template",
    "typename",
    "namespace",
    "using",
    "override",
    "virtual",
    "abstract",
    "final",
    "let",
    "const",
    "elif",
];

fn syntax_for(ext: &str) -> Syntax {
    match ext {
        "rs" => Syntax {
            line_comment: &["//"],
            block: Some(("/*", "*/")),
            quotes: &['"'],
            keywords: KW_RUSTLIKE,
        },
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "java" | "c" | "h" | "cpp" | "hpp" | "cc"
        | "cs" | "go" | "swift" | "kt" | "scala" | "php" | "dart" => Syntax {
            line_comment: &["//"],
            block: Some(("/*", "*/")),
            quotes: &['"', '\'', '`'],
            keywords: KW_RUSTLIKE,
        },
        "py" | "rb" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg"
        | "pl" | "r" => Syntax {
            line_comment: &["#"],
            block: None,
            quotes: &['"', '\''],
            keywords: KW_RUSTLIKE,
        },
        "sql" | "lua" | "hs" | "elm" => Syntax {
            line_comment: &["--"],
            block: Some(("/*", "*/")),
            quotes: &['"', '\''],
            keywords: KW_RUSTLIKE,
        },
        _ => Syntax {
            line_comment: &["//", "#"],
            block: Some(("/*", "*/")),
            quotes: &['"', '\'', '`'],
            keywords: KW_RUSTLIKE,
        },
    }
}

pub(crate) fn mono_font() -> Font {
    Font {
        family: crate::theme::FONT_MONO.into(),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
        features: FontFeatures::disable_ligatures(),
        fallbacks: None,
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A rendered line: the raw text plus the styled runs to hand to `StyledText`.
pub(crate) struct HlLine {
    pub text: SharedString,
    pub runs: Vec<TextRun>,
}

/// Tokenize `contents` into per-line styled runs. Block-comment state is
/// carried across line boundaries so multi-line `/* … */` comments highlight
/// correctly. Never panics on any input.
pub(crate) fn highlight(contents: &str, ext: &str, colors: TokenColors) -> Vec<HlLine> {
    let syntax = syntax_for(ext);
    let font = mono_font();
    let mut in_block = false;
    let mut out = Vec::new();

    for line in contents.split('\n') {
        let mut spans: Vec<(usize, Tok)> = Vec::new(); // (byte len, kind)
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut i = 0usize;

        let push = |spans: &mut Vec<(usize, Tok)>, len: usize, kind: Tok| {
            if len == 0 {
                return;
            }
            if let Some(last) = spans.last_mut() {
                if last.1 == kind {
                    last.0 += len;
                    return;
                }
            }
            spans.push((len, kind));
        };

        while i < chars.len() {
            let (start_byte, c) = chars[i];

            // Continue / detect block comment.
            if in_block {
                if let Some((_, close)) = syntax.block {
                    if line[start_byte..].starts_with(close) {
                        push(&mut spans, close.len(), Tok::Comment);
                        i += close.chars().count();
                        in_block = false;
                        continue;
                    }
                }
                push(&mut spans, c.len_utf8(), Tok::Comment);
                i += 1;
                continue;
            }

            // Line comment → rest of line.
            if let Some(tok) = syntax
                .line_comment
                .iter()
                .find(|p| line[start_byte..].starts_with(**p))
            {
                let _ = tok;
                push(&mut spans, line.len() - start_byte, Tok::Comment);
                break;
            }

            // Block comment open.
            if let Some((open, _)) = syntax.block {
                if line[start_byte..].starts_with(open) {
                    push(&mut spans, open.len(), Tok::Comment);
                    i += open.chars().count();
                    in_block = true;
                    continue;
                }
            }

            // String / char literal. Scan to the closing quote (or end of
            // line), honouring backslash escapes.
            if syntax.quotes.contains(&c) {
                let quote = c;
                let lit_start = start_byte;
                i += 1;
                let mut escaped = false;
                while i < chars.len() {
                    let (_b, ch) = chars[i];
                    if escaped {
                        escaped = false;
                        i += 1;
                        continue;
                    }
                    if ch == '\\' {
                        escaped = true;
                        i += 1;
                        continue;
                    }
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                // Derive the span end from the final cursor so it always lands
                // on a char boundary and never lags behind `i` — every branch
                // above advances `i`, and here we read its byte offset once.
                // (Tracking end_byte per-branch previously undercounted when a
                // literal ended on an escape, crashing StyledText.)
                let end_byte = if i < chars.len() {
                    chars[i].0
                } else {
                    line.len()
                };
                push(&mut spans, end_byte - lit_start, Tok::Str);
                continue;
            }

            // Number.
            if c.is_ascii_digit() {
                let num_start = start_byte;
                while i < chars.len()
                    && (chars[i].1.is_ascii_alphanumeric()
                        || chars[i].1 == '.'
                        || chars[i].1 == '_')
                {
                    i += 1;
                }
                let end = if i < chars.len() {
                    chars[i].0
                } else {
                    line.len()
                };
                push(&mut spans, end - num_start, Tok::Number);
                continue;
            }

            // Word → keyword or plain text.
            if is_word(c) {
                let word_start = start_byte;
                while i < chars.len() && is_word(chars[i].1) {
                    i += 1;
                }
                let end = if i < chars.len() {
                    chars[i].0
                } else {
                    line.len()
                };
                let word = &line[word_start..end];
                let kind = if syntax.keywords.contains(&word) {
                    Tok::Keyword
                } else {
                    Tok::Text
                };
                push(&mut spans, end - word_start, kind);
                continue;
            }

            // Anything else.
            push(&mut spans, c.len_utf8(), Tok::Text);
            i += 1;
        }

        let runs = spans
            .into_iter()
            .map(|(len, kind)| TextRun {
                len,
                font: font.clone(),
                color: match kind {
                    Tok::Text => colors.text,
                    Tok::Comment => colors.comment,
                    Tok::Str => colors.string,
                    Tok::Keyword => colors.keyword,
                    Tok::Number => colors.number,
                },
                background_color: None,
                underline: None,
                strikethrough: None,
            })
            .collect();

        out.push(HlLine {
            text: SharedString::from(line.to_string()),
            runs,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> TokenColors {
        let z = gpui::hsla(0., 0., 0., 1.);
        TokenColors {
            text: z,
            comment: z,
            string: z,
            keyword: z,
            number: z,
        }
    }

    #[test]
    fn does_not_panic_on_unicode_and_unterminated() {
        let src = "let s = \"héllo\nfn café() {} // λ\n/* unclosed\nmore";
        let lines = highlight(src, "rs", colors());
        assert_eq!(lines.len(), 4);
        // Every run length must sum to the line's byte length (no drift).
        for l in &lines {
            let sum: usize = l.runs.iter().map(|r| r.len).sum();
            assert_eq!(sum, l.text.len());
        }
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        let lines = highlight("", "rs", colors());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text.len(), 0);
    }

    /// Regression: strings ending on a backslash escape (very common in real
    /// source) must not undercount run lengths — that crashed StyledText.
    #[test]
    fn run_lengths_cover_strings_with_trailing_escapes() {
        let cases = [
            r#"println!("hello\n");"#,   // escape mid-string, terminated
            r#"let s = "unterminated\"#, // unterminated, ends on backslash
            r#"let s = "esc\"#,          // ends on escape
            r#"let c = '\''"#,           // escaped quote char literal
            r#"path = "C:\\Users\\"#,    // trailing double backslash, unterminated
            "let a = \"\\",              // string then lone backslash
        ];
        for src in cases {
            for l in highlight(src, "rs", colors()) {
                let sum: usize = l.runs.iter().map(|r| r.len).sum();
                assert_eq!(sum, l.text.len(), "run-length mismatch for {src:?}");
            }
        }
    }
}
