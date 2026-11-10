//! Change decorations on file paths in terminal output (DEV-1262).
//!
//! Allele already finds every path-shaped token in the visible viewport on
//! each render. This module joins those tokens against what *this session*
//! changed in its clone, so a path Claude printed — `Update(src/foo.rs)` —
//! can carry a dim `+12 −3` chip, open a one-hunk diff peek on ⌥-hover, and
//! offer "Show diff" in its context menu.
//!
//! Everything here is pure: it reads characters and the change index and
//! returns geometry and text for the renderer to paint as an overlay. None
//! of it can reach the PTY, by construction — the view's input path is the
//! only writer, and nothing in this module is given access to it.
//!
//! The join is deliberately strict. The path underliner is a shape heuristic
//! and marks plenty of non-paths (`~US$99/yr`, `approve/reject`), so a chip
//! appears only on a token that resolves, lexically and without touching
//! the filesystem, to a file in the change index. A path Claude truncated
//! (`src/…/foo.rs`) is never guessed at.

use super::terminal_view::parse_line_col_suffix;
use crate::git::{ChangeKind, ChangedFile, LineStats};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;
use gpui::SharedString;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Component, Path, PathBuf};

/// Most lines a peek card shows — one hunk window, far below the Changes
/// panel's 2000-line cap.
pub const PEEK_MAX_LINES: usize = 16;

/// One changed path, with its panel rows folded together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    /// Path relative to the repo root, as the Changes panel lists it.
    pub rel_path: String,
    /// Sum over this path's panel rows (staged and unstaged sides), so the
    /// chip reads the same numbers the panel shows.
    pub added: u32,
    pub removed: u32,
    /// Any side was reported binary by git.
    pub binary: bool,
    /// Each panel row for this path: (kind, staged side). Unstaged first.
    pub sides: Vec<(ChangeKind, bool)>,
    /// `chip_label()`, built once so a render clones a pointer, not a string.
    label: SharedString,
}

impl ChangedPath {
    /// The chip text: `+12 −3`, or `bin` for a binary change with no counts.
    pub fn chip_label(&self) -> SharedString {
        self.label.clone()
    }

    fn make_label(&self) -> SharedString {
        if self.binary && self.added == 0 && self.removed == 0 {
            "bin".into()
        } else {
            format!("+{} \u{2212}{}", self.added, self.removed).into()
        }
    }

    /// Side the Changes panel should select for "Show diff": the unstaged
    /// row when there is one (it is what the agent most recently wrote).
    pub fn preferred_side(&self) -> bool {
        !self.sides.iter().any(|(_, staged)| !staged)
    }
}

/// What this session changed, keyed for lookup from terminal text.
///
/// Three keys, cheapest first, because the scan asks about every path on
/// screen on every render: a plain relative token printed from the clone
/// root is looked up as-is, one hash and no allocation; for any other form
/// the file name alone rejects nearly every unchanged path, and only the
/// survivors pay for lexical resolution.
#[derive(Debug, Clone, Default)]
pub struct SessionChangeIndex {
    /// The clone directory the data was loaded from.
    pub repo_dir: PathBuf,
    entries: Vec<ChangedPath>,
    // Fx, not SipHash: these are probed for every path on screen on every
    // render, keys are local file paths, and nothing here is adversarial.
    by_abs: FxHashMap<PathBuf, usize>,
    by_rel: FxHashMap<String, usize>,
    names: FxHashSet<String>,
    /// Narrowest chip label, in cells. A row with no room for even this
    /// can skip the lookup entirely.
    min_label_width: usize,
}

impl SessionChangeIndex {
    pub fn build(repo_dir: &Path, files: &[ChangedFile], stats: &LineStats) -> Self {
        let repo_dir = normalize(repo_dir);
        let mut index = Self {
            repo_dir: repo_dir.clone(),
            ..Self::default()
        };
        // Unstaged rows first, so `sides` lists the unstaged side first.
        let ordered = files
            .iter()
            .filter(|f| !f.staged)
            .chain(files.iter().filter(|f| f.staged));
        for file in ordered {
            let slot = match index.by_rel.get(&file.path) {
                Some(&slot) => slot,
                None => {
                    let slot = index.entries.len();
                    index.entries.push(ChangedPath {
                        rel_path: file.path.clone(),
                        added: 0,
                        removed: 0,
                        binary: false,
                        sides: Vec::new(),
                        label: SharedString::default(),
                    });
                    index.by_rel.insert(file.path.clone(), slot);
                    index
                        .by_abs
                        .insert(normalize(&repo_dir.join(&file.path)), slot);
                    index.names.insert(file_name(&file.path).to_string());
                    slot
                }
            };
            let entry = &mut index.entries[slot];
            if let Some(stat) = stats.get(&(file.path.clone(), file.staged)) {
                entry.added += stat.added;
                entry.removed += stat.removed;
                entry.binary |= stat.binary;
            }
            entry.sides.push((file.kind, file.staged));
        }
        for entry in &mut index.entries {
            entry.label = entry.make_label();
        }
        index.min_label_width = index
            .entries
            .iter()
            .map(|e| e.label.chars().count())
            .min()
            .unwrap_or(0);
        index
    }

    pub fn get(&self, abs: &Path) -> Option<&ChangedPath> {
        self.by_abs.get(abs).map(|&slot| &self.entries[slot])
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The decoration join: `None` for a truncated token, one that doesn't
    /// resolve, or an unchanged file. `wd_is_repo` says the token's working
    /// directory is the clone root, which lets plain relative tokens skip
    /// resolution.
    fn find(
        &self,
        token: &str,
        working_dir: Option<&Path>,
        wd_is_repo: bool,
        home: Option<&Path>,
    ) -> Option<&ChangedPath> {
        if is_truncated(token) {
            return None;
        }
        let (path_part, _) = parse_line_col_suffix(token);
        let plain = !path_part.starts_with(['/', '~', '.']) && !path_part.contains("/.");
        if wd_is_repo && plain {
            return self.by_rel.get(path_part).map(|&slot| &self.entries[slot]);
        }
        if !self.names.contains(file_name(path_part)) {
            return None;
        }
        let abs = resolve_lexical(path_part, working_dir, home)?;
        self.get(&abs)
    }

    fn wd_is_repo(&self, working_dir: Option<&Path>) -> bool {
        working_dir.is_some_and(|wd| normalize(wd) == self.repo_dir)
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A chip to paint: `label` starting at grid cell (`row`, `col`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub row: usize,
    pub col: usize,
    pub label: SharedString,
}

/// Everything the viewport scan produces for one render.
#[derive(Debug, Default)]
pub struct ScanOutput {
    pub url_spans: Vec<(usize, usize, usize)>,
    pub path_spans: Vec<(usize, usize, usize)>,
    pub chips: Vec<Chip>,
}

/// A path-shaped token on a row, as char indices into the row's chars.
/// `raw_*` bounds the whitespace-delimited word; `start..=end` is the path
/// inside it once brackets, punctuation and a `Tool(` prefix are trimmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowToken {
    pub raw_start: usize,
    pub raw_end: usize,
    pub start: usize,
    pub end: usize,
}

/// Split a row into trimmed tokens. Shared by the viewport scan and the
/// hover/right-click lookup so the two never disagree about a token's bounds.
pub fn row_tokens(chars: &[char]) -> Vec<RowToken> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let raw_start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let raw_end = i - 1;
        if let Some((start, end)) = trim_token(chars, raw_start, raw_end) {
            out.push(RowToken {
                raw_start,
                raw_end,
                start,
                end,
            });
        }
    }
    out
}

fn trim_token(chars: &[char], mut start: usize, mut end: usize) -> Option<(usize, usize)> {
    while start < end && matches!(chars[start], '(' | '[' | '{' | '<' | '"' | '\'' | '`') {
        start += 1;
    }
    while end > start
        && matches!(
            chars[end],
            ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | ';' | '.' | '!' | '?' | ':'
        )
    {
        end -= 1;
    }
    // Claude Code prints tool calls as `Update(src/foo.rs)`: drop a leading
    // identifier and its `(`, so the path inside is what resolves.
    if let Some(open) = (start..end).find(|&k| chars[k] == '(') {
        let prefix = &chars[start..open];
        if !prefix.is_empty()
            && prefix
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            start = open + 1;
        }
    }
    (start <= end).then_some((start, end))
}

/// Cheap syntactic check — no filesystem stat. True if the token *looks
/// like* it could be a path. This drives the passive underline.
pub fn token_looks_like_path(token: &str) -> bool {
    if token.len() < 2 {
        return false;
    }
    let (path_part, _) = parse_line_col_suffix(token);
    if path_part.is_empty() {
        return false;
    }
    if path_part.starts_with('/') || path_part.starts_with("~/") || path_part == "~" {
        return true;
    }
    if path_part.starts_with("./") || path_part.starts_with("../") {
        return true;
    }
    // Bare relative: require at least one '/' or a file extension to
    // avoid matching random words. e.g. `src/main.rs` or `foo.rs`.
    if path_part.contains('/') {
        return true;
    }
    if let Some(dot) = path_part.rfind('.') {
        let ext = &path_part[dot + 1..];
        // Extensions: 1-8 alphanumeric chars.
        if (1..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
    }
    false
}

/// True when a path was shortened for display (`src/…/foo.rs`,
/// `.../foo.rs`). Such a path is ambiguous, so it is never decorated.
pub fn is_truncated(token: &str) -> bool {
    token.contains('\u{2026}')
        || token.starts_with("...")
        || token.ends_with("...")
        || token.split('/').any(|segment| segment == "...")
}

/// Resolve a path token against `working_dir` purely lexically: expand
/// `~`, join relative paths, fold `.` and `..`. No filesystem access, so it
/// is cheap enough to run for every path on every render.
pub fn resolve_lexical(
    path_part: &str,
    working_dir: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let expanded = if let Some(rest) = path_part.strip_prefix("~/") {
        home?.join(rest)
    } else if path_part == "~" {
        home?.to_path_buf()
    } else {
        PathBuf::from(path_part)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        working_dir?.join(expanded)
    };
    Some(normalize(&joined))
}

/// True when two directories are the same once `.`/`..` are folded.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    normalize(a) == normalize(b)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Look up a token in the change index — the join the chip, the peek and
/// the context menu all share. `None` for a truncated token, one that
/// doesn't resolve, or an unchanged file.
pub fn lookup<'a>(
    token: &str,
    index: &'a SessionChangeIndex,
    working_dir: Option<&Path>,
) -> Option<&'a ChangedPath> {
    let home = dirs::home_dir();
    index.find(
        token,
        working_dir,
        index.wd_is_repo(working_dir),
        home.as_deref(),
    )
}

/// Where a chip of `width` cells goes for a path ending at `col_end`.
///
/// First choice is just after the path, in blank cells: one blank
/// separator, the label, and a blank (or the row's end) after it, so the
/// chip never touches other text. Failing that, the right-margin gutter:
/// the row's last `width` cells, if they and the cell before them are
/// blank. Failing both, no chip — covering terminal text is never an
/// option. `taken` holds column ranges already used by chips on this row.
pub fn place_chip(
    blank: &[bool],
    col_end: usize,
    width: usize,
    taken: &[(usize, usize)],
) -> Option<usize> {
    let num_cols = blank.len();
    let free = |from: usize, to: usize| -> bool {
        to < num_cols
            && (from..=to).all(|c| blank[c])
            && !taken.iter().any(|&(a, b)| from <= b && a <= to)
    };
    let after = col_end + 2;
    let after_end = after + width - 1;
    if free(col_end + 1, after_end) && (after_end + 1 >= num_cols || blank[after_end + 1]) {
        return Some(after);
    }
    let gutter = num_cols.checked_sub(width)?;
    if gutter > col_end + 1 && free(gutter - 1, num_cols - 1) {
        return Some(gutter);
    }
    None
}

/// Scan the visible viewport of `term` for URL spans, path spans and the
/// change chips for paths in `index`. One lock-holding pass over the grid;
/// the caller holds the lock for the duration.
pub fn scan_grid<T>(
    term: &Term<T>,
    working_dir: Option<&Path>,
    index: Option<&SessionChangeIndex>,
) -> ScanOutput {
    let mut out = ScanOutput::default();
    let grid = term.grid();
    let num_cols = grid.columns();
    let num_lines = grid.screen_lines();
    let display_offset = grid.display_offset() as i32;
    let index = index.filter(|i| !i.is_empty());
    let home = index.and_then(|_| dirs::home_dir());
    let wd_is_repo = index.is_some_and(|i| i.wd_is_repo(working_dir));

    let mut chars: Vec<char> = Vec::with_capacity(num_cols);
    let mut cols: Vec<usize> = Vec::with_capacity(num_cols);
    let mut blank: Vec<bool> = Vec::with_capacity(num_cols);
    let mut taken: Vec<(usize, usize)> = Vec::new();

    for row in 0..num_lines {
        chars.clear();
        cols.clear();
        blank.clear();
        taken.clear();
        let line = &grid[Line(row as i32 - display_offset)];
        for col in 0..num_cols {
            let cell = &line[Column(col)];
            let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
            blank.push(!spacer && matches!(cell.c, ' ' | '\0'));
            if spacer {
                continue;
            }
            chars.push(if cell.c == '\0' { ' ' } else { cell.c });
            cols.push(col);
        }
        if chars.iter().all(|c| *c == ' ') {
            continue;
        }

        scan_urls(&chars, &cols, row, &mut out.url_spans);

        for token in row_tokens(&chars) {
            let text: String = chars[token.start..=token.end].iter().collect();
            if !token_looks_like_path(&text) {
                continue;
            }
            let col_start = cols[token.start];
            let col_end = cols[token.end];
            out.path_spans.push((row, col_start, col_end));

            let Some(index) = index else { continue };
            // Clear the raw word's tail too (`Update(foo.rs)` ends at `)`).
            let anchor = cols[token.raw_end].max(col_end);
            // No room for even the narrowest chip: nothing to look up. Exact,
            // not a heuristic — any width that fits, a narrower one fits too.
            if place_chip(&blank, anchor, index.min_label_width, &taken).is_none() {
                continue;
            }
            let Some(changed) = index.find(&text, working_dir, wd_is_repo, home.as_deref()) else {
                continue;
            };
            let label = changed.chip_label();
            let width = label.chars().count();
            if let Some(col) = place_chip(&blank, anchor, width, &taken) {
                taken.push((col.saturating_sub(1), col + width - 1));
                out.chips.push(Chip { row, col, label });
            }
        }
    }
    out
}

fn scan_urls(chars: &[char], cols: &[usize], row: usize, out: &mut Vec<(usize, usize, usize)>) {
    let text: String = chars.iter().collect();
    for (scheme_start, _) in text
        .match_indices("http://")
        .chain(text.match_indices("https://"))
    {
        // Convert byte offset → char offset in `chars`.
        let char_start = text[..scheme_start].chars().count();
        let mut char_end = char_start;
        while char_end < chars.len() {
            let c = chars[char_end];
            if c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'') {
                break;
            }
            char_end += 1;
        }
        while char_end > char_start
            && matches!(chars[char_end - 1], '.' | ',' | ';' | ':' | '!' | '?')
        {
            char_end -= 1;
        }
        if char_end > char_start {
            out.push((row, cols[char_start], cols[char_end - 1]));
        }
    }
}

// ── Peek ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekLineKind {
    Hunk,
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeekLine {
    pub kind: PeekLineKind,
    pub text: String,
}

/// What a peek card shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeekBody {
    Loading,
    /// One hunk window of this session's diff.
    Diff {
        lines: Vec<PeekLine>,
        /// 1-based index of the hunk shown, and how many the file has.
        hunk: usize,
        hunks: usize,
        /// Line in the working-tree file of the hunk's first change.
        first_changed_line: Option<u32>,
    },
    /// An unchanged file: its first lines.
    FileHead(Vec<String>),
    Binary,
    Empty,
    Error(String),
}

impl PeekBody {
    pub fn first_changed_line(&self) -> Option<u32> {
        match self {
            Self::Diff {
                first_changed_line, ..
            } => *first_changed_line,
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Hunk {
    new_start: u32,
    new_len: u32,
    /// Lines after the `@@` header, with the new-file line each sits at.
    lines: Vec<(PeekLineKind, String, u32)>,
    header: String,
}

fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut new_line = 0u32;
    for raw in diff.lines() {
        if raw.starts_with("@@") {
            let (new_start, new_len) = parse_hunk_header(raw).unwrap_or((1, 0));
            new_line = new_start;
            hunks.push(Hunk {
                new_start,
                new_len,
                lines: Vec::new(),
                header: raw.to_string(),
            });
            continue;
        }
        let Some(hunk) = hunks.last_mut() else {
            continue; // file headers before the first hunk
        };
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'+') => (PeekLineKind::Added, &raw[1..]),
            Some(b'-') => (PeekLineKind::Removed, &raw[1..]),
            Some(b' ') => (PeekLineKind::Context, &raw[1..]),
            _ => continue, // `\ No newline at end of file`
        };
        hunk.lines.push((kind, text.to_string(), new_line));
        if kind != PeekLineKind::Removed {
            new_line += 1;
        }
    }
    hunks
}

/// `@@ -a,b +c,d @@` → (c, d).
fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    let plus = header.split_whitespace().find(|p| p.starts_with('+'))?;
    let mut parts = plus[1..].splitn(2, ',');
    let start = parts.next()?.parse().ok()?;
    let len = parts.next().map_or(Some(1), |l| l.parse().ok())?;
    Some((start, len))
}

/// Cut one hunk window out of a unified diff.
///
/// The hunk is the one containing `around` (a `:line` the path carried),
/// else the nearest to it, else the first. A hunk longer than `max_lines`
/// is windowed to start two lines before its first change.
pub fn hunk_window(diff: &str, around: Option<u32>, max_lines: usize) -> PeekBody {
    let hunks = parse_hunks(diff);
    if hunks.is_empty() {
        return PeekBody::Empty;
    }
    let pick = match around {
        Some(line) => hunks
            .iter()
            .enumerate()
            .min_by_key(|(_, h)| {
                let end = h.new_start + h.new_len.saturating_sub(1);
                if (h.new_start..=end).contains(&line) {
                    0
                } else {
                    h.new_start.abs_diff(line).min(end.abs_diff(line))
                }
            })
            .map(|(i, _)| i)
            .unwrap_or(0),
        None => 0,
    };
    let hunk = &hunks[pick];
    let first_change = hunk
        .lines
        .iter()
        .position(|(k, _, _)| *k != PeekLineKind::Context);
    let first_changed_line = first_change.map(|i| hunk.lines[i].2);
    let body_cap = max_lines.saturating_sub(1).max(1);
    let start = if hunk.lines.len() > body_cap {
        first_change.unwrap_or(0).saturating_sub(2)
    } else {
        0
    };
    let mut lines = vec![PeekLine {
        kind: PeekLineKind::Hunk,
        text: hunk.header.clone(),
    }];
    lines.extend(
        hunk.lines
            .iter()
            .skip(start)
            .take(body_cap)
            .map(|(kind, text, _)| PeekLine {
                kind: *kind,
                text: text.clone(),
            }),
    );
    PeekBody::Diff {
        lines,
        hunk: pick + 1,
        hunks: hunks.len(),
        first_changed_line,
    }
}

/// Blocking: build the peek for `abs`. Runs on the background executor.
pub fn load_peek(
    repo_dir: &Path,
    changed: Option<&ChangedPath>,
    abs: &Path,
    around: Option<u32>,
) -> PeekBody {
    let Some(changed) = changed else {
        return file_head(abs, PEEK_MAX_LINES);
    };
    let mut text = String::new();
    for (kind, staged) in &changed.sides {
        match crate::git::diff_file(repo_dir, &changed.rel_path, *kind, *staged) {
            Ok(diff) if diff.binary => return PeekBody::Binary,
            Ok(diff) => text.push_str(&diff.text),
            Err(e) => return PeekBody::Error(e.to_string()),
        }
    }
    hunk_window(&text, around, PEEK_MAX_LINES)
}

fn file_head(path: &Path, max_lines: usize) -> PeekBody {
    use std::io::Read;
    let mut buf = vec![0u8; 16 * 1024];
    let read = std::fs::File::open(path).and_then(|mut f| f.read(&mut buf));
    let n = match read {
        Ok(n) => n,
        Err(e) => return PeekBody::Error(e.to_string()),
    };
    buf.truncate(n);
    if buf.contains(&0) {
        return PeekBody::Binary;
    }
    let lines: Vec<String> = String::from_utf8_lossy(&buf)
        .lines()
        .take(max_lines)
        .map(str::to_string)
        .collect();
    if lines.is_empty() {
        PeekBody::Empty
    } else {
        PeekBody::FileHead(lines)
    }
}

#[cfg(test)]
#[path = "path_decor_tests.rs"]
mod tests;
