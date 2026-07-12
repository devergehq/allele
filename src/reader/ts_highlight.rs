//! Tree-sitter backed syntax highlighting for the source Reader (DEV-73).
//!
//! Produces the same per-line [`HlLine`](super::highlight::HlLine) / `TextRun`
//! output as the built-in lexer, so the render path is unchanged. A curated set
//! of grammars is compiled in; `highlight` returns `None` for any extension
//! without a bundled grammar, and the caller falls back to the lexer.
//!
//! Configurations are parsed once and cached in a thread-local (highlighting
//! runs on the render thread). Long-tail / dynamically-loaded grammars are
//! tracked separately in DEV-63.

use std::cell::RefCell;
use std::collections::HashMap;

use gpui::{Hsla, SharedString, TextRun};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use super::highlight::{mono_font, HlLine, TokenColors};

/// Capture names we configure the highlighter with. `Highlight(i)` from a query
/// indexes into this array; tree-sitter matches the most specific name, so
/// listing both `function` and `function.builtin` is fine. `color_for` maps
/// each back to a theme colour.
const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "function",
    "function.builtin",
    "function.method",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// Map an extension to a bundled grammar id, or `None` to fall back to the lexer.
fn lang_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        // The TSX grammar is a superset that also parses JS/JSX.
        "tsx" | "js" | "jsx" | "mjs" | "cjs" => "tsx",
        "py" | "pyi" => "python",
        "go" => "go",
        "json" => "json",
        "sh" | "bash" | "zsh" => "bash",
        "php" | "phtml" | "php3" | "php4" | "php5" | "phps" => "php",
        _ => return None,
    })
}

/// Build the `HighlightConfiguration` for a grammar id (parses its queries).
fn build_config(lang: &str) -> Option<HighlightConfiguration> {
    let (language, highlights, injections, locals): (
        tree_sitter::Language,
        &str,
        &str,
        &str,
    ) = match lang {
        "rust" => (
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        ),
        "typescript" => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        "tsx" => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        "python" => (
            tree_sitter_python::LANGUAGE.into(),
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        "go" => (
            tree_sitter_go::LANGUAGE.into(),
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        "json" => (
            tree_sitter_json::LANGUAGE.into(),
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
            "",
        ),
        "bash" => (
            tree_sitter_bash::LANGUAGE.into(),
            tree_sitter_bash::HIGHLIGHT_QUERY,
            "",
            "",
        ),
        // LANGUAGE_PHP parses `.php` with its embedded HTML/text regions.
        "php" => (
            tree_sitter_php::LANGUAGE_PHP.into(),
            tree_sitter_php::HIGHLIGHTS_QUERY,
            tree_sitter_php::INJECTIONS_QUERY,
            "",
        ),
        _ => return None,
    };
    let mut config =
        HighlightConfiguration::new(language, lang, highlights, injections, locals).ok()?;
    config.configure(HIGHLIGHT_NAMES);
    Some(config)
}

thread_local! {
    static HIGHLIGHTER: RefCell<Highlighter> = RefCell::new(Highlighter::new());
    /// `None` entries memoize "this grammar failed to build" so we don't retry.
    static CONFIGS: RefCell<HashMap<&'static str, Option<HighlightConfiguration>>> =
        RefCell::new(HashMap::new());
}

/// Highlight `contents` with tree-sitter, or return `None` if no grammar is
/// bundled for `ext` (or parsing fails) so the caller can fall back.
pub(crate) fn highlight(contents: &str, ext: &str, colors: &TokenColors) -> Option<Vec<HlLine>> {
    let lang = lang_for_ext(ext)?;

    // (start_byte, end_byte, highlight_index) spans covering all of `contents`.
    let spans: Vec<(usize, usize, Option<usize>)> = CONFIGS.with(|configs_cell| {
        let mut configs = configs_cell.borrow_mut();
        let config = configs
            .entry(lang)
            .or_insert_with(|| build_config(lang));
        let config = config.as_ref()?;

        HIGHLIGHTER.with(|hl_cell| {
            let mut highlighter = hl_cell.borrow_mut();
            let events = highlighter
                .highlight(config, contents.as_bytes(), None, |_| None)
                .ok()?;

            let mut out = Vec::new();
            let mut stack: Vec<usize> = Vec::new();
            for event in events {
                match event.ok()? {
                    HighlightEvent::HighlightStart(h) => stack.push(h.0),
                    HighlightEvent::HighlightEnd => {
                        stack.pop();
                    }
                    HighlightEvent::Source { start, end } => {
                        out.push((start, end, stack.last().copied()));
                    }
                }
            }
            Some(out)
        })
    })?;

    Some(build_lines(contents, &spans, colors))
}

/// Turn contiguous highlighted byte-spans into per-line `TextRun`s. Splits spans
/// at newlines; run lengths always sum to their line's byte length and land on
/// char boundaries (tree-sitter emits token-aligned offsets), so `StyledText`
/// never panics.
fn build_lines(contents: &str, spans: &[(usize, usize, Option<usize>)], colors: &TokenColors) -> Vec<HlLine> {
    let font = mono_font();
    let mut lines: Vec<HlLine> = Vec::new();
    let mut cur_text = String::new();
    let mut cur_runs: Vec<TextRun> = Vec::new();

    let push_run = |runs: &mut Vec<TextRun>, len: usize, color: Hsla| {
        if len == 0 {
            return;
        }
        // Merge with the previous run if the colour matches, to reduce shaping.
        if let Some(last) = runs.last_mut() {
            if last.color == color && last.background_color.is_none() {
                last.len += len;
                return;
            }
        }
        runs.push(TextRun {
            len,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    };

    for &(start, end, hidx) in spans {
        let color = hidx
            .and_then(|i| HIGHLIGHT_NAMES.get(i))
            .map(|name| color_for(name, colors))
            .unwrap_or(colors.text);
        let text = &contents[start..end];
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                lines.push(HlLine {
                    text: SharedString::from(std::mem::take(&mut cur_text)),
                    runs: std::mem::take(&mut cur_runs),
                });
            }
            first = false;
            if !part.is_empty() {
                push_run(&mut cur_runs, part.len(), color);
                cur_text.push_str(part);
            }
        }
    }
    // The final line has no trailing newline to flush it.
    lines.push(HlLine {
        text: SharedString::from(cur_text),
        runs: cur_runs,
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> TokenColors {
        let z = gpui::hsla(0., 0., 0., 1.);
        TokenColors {
            text: z, comment: z, string: z, keyword: z, number: z, function: z, type_: z,
            constant: z, property: z, operator: z, punctuation: z, variable: z,
        }
    }

    #[test]
    fn rust_highlights_and_run_lengths_are_sound() {
        let src = "// c\nfn café(x: i32) -> i32 {\n    let s = \"héllo\\n\";\n    x\n}\n";
        let lines = super::highlight(src, "rs", &colors()).expect("rust grammar bundled");
        // Line count matches a plain newline split.
        assert_eq!(lines.len(), src.split('\n').count());
        // Every line's run lengths sum to its text byte length (StyledText-safe).
        for l in &lines {
            let sum: usize = l.runs.iter().map(|r| r.len).sum();
            assert_eq!(sum, l.text.len(), "run mismatch on {:?}", l.text);
        }
    }

    #[test]
    fn unknown_extension_falls_back() {
        assert!(super::highlight("x = 1", "zig", &colors()).is_none());
    }
}

/// Map a tree-sitter capture name to a theme colour.
fn color_for(name: &str, c: &TokenColors) -> Hsla {
    // Match on the first segment so `function.builtin` → `function`, etc.
    let base = name.split('.').next().unwrap_or(name);
    match base {
        "comment" => c.comment,
        "string" => c.string,
        "keyword" => c.keyword,
        "number" => c.number,
        "function" => c.function,
        "constructor" => c.function,
        "type" | "module" => c.type_,
        "constant" => c.constant,
        "property" | "attribute" | "tag" | "label" => c.property,
        "operator" => c.operator,
        "punctuation" => c.punctuation,
        "variable" => c.variable,
        _ => c.text,
    }
}
