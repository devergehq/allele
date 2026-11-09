//! Tests for terminal path decorations (DEV-1262).
//!
//! No `unwrap`/`expect` here: the panic ratchet in tests/architecture.rs
//! cannot see that this file is test-only.

use super::*;
use crate::git::{ChangeKind, ChangedFile, LineStat, LineStats};
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REPO: &str = "/work/clone";

fn file(path: &str, kind: ChangeKind, staged: bool) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        kind,
        staged,
    }
}

fn stat(added: u32, removed: u32) -> LineStat {
    LineStat {
        added,
        removed,
        binary: false,
    }
}

/// `src/app_state.rs` changed on both sides (12/3 unstaged, 2/1 staged),
/// `notes.md` untracked with 4 lines.
fn sample_index() -> SessionChangeIndex {
    let files = vec![
        file("src/app_state.rs", ChangeKind::Modified, true),
        file("src/app_state.rs", ChangeKind::Modified, false),
        file("notes.md", ChangeKind::Untracked, false),
    ];
    let mut stats = LineStats::new();
    stats.insert(("src/app_state.rs".into(), false), stat(12, 3));
    stats.insert(("src/app_state.rs".into(), true), stat(2, 1));
    stats.insert(("notes.md".into(), false), stat(4, 0));
    SessionChangeIndex::build(Path::new(REPO), &files, &stats)
}

fn term_with(columns: usize, lines: usize, text: &str) -> Term<VoidListener> {
    let mut term = Term::new(
        Config::default(),
        &TermSize::new(columns, lines),
        VoidListener,
    );
    let mut parser: Processor<StdSyncHandler> = Processor::new();
    parser.advance(&mut term, text.replace('\n', "\r\n").as_bytes());
    term
}

fn chips_for(text: &str, columns: usize) -> Vec<Chip> {
    let term = term_with(columns, 6, text);
    let index = sample_index();
    scan_grid(&term, Some(Path::new(REPO)), Some(&index)).chips
}

fn token_texts(s: &str) -> Vec<String> {
    let c: Vec<char> = s.chars().collect();
    row_tokens(&c)
        .into_iter()
        .map(|t| c[t.start..=t.end].iter().collect())
        .collect()
}

// ── Tokens ──────────────────────────────────────────────────────────

#[test]
fn tool_call_prefix_is_stripped_to_the_inner_path() {
    assert_eq!(
        token_texts("⏺ Update(src/app_state.rs)"),
        vec!["⏺", "src/app_state.rs"]
    );
    assert_eq!(token_texts("Read(/etc/hosts)"), vec!["/etc/hosts"]);
    // Surrounding punctuation still trims as before.
    assert_eq!(
        token_texts("(see `src/a.rs:12`),"),
        vec!["see", "src/a.rs:12"]
    );
}

#[test]
fn truncated_paths_are_recognised() {
    assert!(is_truncated("src/…/foo.rs"));
    assert!(is_truncated("…/foo.rs"));
    assert!(is_truncated(".../foo.rs"));
    assert!(is_truncated("src/.../foo.rs"));
    assert!(is_truncated("src/very/long/pa..."));
    assert!(!is_truncated("../foo.rs"));
    assert!(!is_truncated("./src/foo.rs"));
}

#[test]
fn lexical_resolution_folds_dots_without_touching_disk() {
    let wd = Path::new(REPO);
    let home = Path::new("/Users/me");
    let resolve = |s| resolve_lexical(s, Some(wd), Some(home));
    assert_eq!(
        resolve("src/a.rs"),
        Some(PathBuf::from("/work/clone/src/a.rs"))
    );
    assert_eq!(
        resolve("./src/../b.rs"),
        Some(PathBuf::from("/work/clone/b.rs"))
    );
    assert_eq!(resolve("/abs/x.rs"), Some(PathBuf::from("/abs/x.rs")));
    assert_eq!(resolve("~/x.rs"), Some(PathBuf::from("/Users/me/x.rs")));
    assert_eq!(resolve_lexical("rel.rs", None, None), None);
}

// ── Index and chip label ────────────────────────────────────────────

#[test]
fn chip_counts_are_the_sum_of_the_paths_panel_rows() {
    let index = sample_index();
    let Some(changed) = index.get(Path::new("/work/clone/src/app_state.rs")) else {
        panic!("changed path missing from index");
    };
    assert_eq!((changed.added, changed.removed), (14, 4));
    assert_eq!(changed.chip_label(), "+14 \u{2212}4");
    assert_eq!(
        changed.sides,
        vec![(ChangeKind::Modified, false), (ChangeKind::Modified, true)],
        "unstaged side first"
    );
    assert!(
        !changed.preferred_side(),
        "Show diff prefers the unstaged row"
    );
}

#[test]
fn binary_change_without_counts_reads_bin() {
    let mut stats = LineStats::new();
    stats.insert(
        ("logo.png".into(), false),
        LineStat {
            binary: true,
            ..LineStat::default()
        },
    );
    let index = SessionChangeIndex::build(
        Path::new(REPO),
        &[file("logo.png", ChangeKind::Modified, false)],
        &stats,
    );
    let label = index
        .get(Path::new("/work/clone/logo.png"))
        .map(ChangedPath::chip_label);
    assert_eq!(label.as_deref().map(|l| &**l), Some("bin"));
}

// ── Scan: which paths get chips ─────────────────────────────────────

#[test]
fn changed_path_gets_a_chip_just_after_it() {
    let chips = chips_for("⏺ Update(src/app_state.rs)\n", 60);
    // `⏺` col 0, space, `Update(` cols 2-8, path cols 9-24, `)` col 25,
    // blank separator col 26, chip from col 27.
    assert_eq!(
        chips,
        vec![Chip {
            row: 0,
            col: 27,
            label: "+14 \u{2212}4".into()
        }]
    );
}

#[test]
fn every_path_form_of_a_changed_file_resolves() {
    for text in [
        "src/app_state.rs\n",
        "./src/app_state.rs:12\n",
        "/work/clone/src/app_state.rs\n",
        "  ⎿  Updated src/app_state.rs with 12 additions\n",
    ] {
        assert_eq!(chips_for(text, 80).len(), 1, "no chip for {text:?}");
    }
}

#[test]
fn unchanged_path_gets_no_chip() {
    assert!(chips_for("Read(src/main.rs) and Cargo.toml\n", 60).is_empty());
}

#[test]
fn truncated_path_gets_no_chip() {
    assert!(chips_for("Update(src/…/app_state.rs)\n", 60).is_empty());
    assert!(chips_for("Update(…/app_state.rs)\n", 60).is_empty());
}

#[test]
fn chip_never_covers_text_and_falls_back_to_the_gutter() {
    // Text right after the path: the chip moves to the row's right edge.
    let chips = chips_for("src/app_state.rs was edited here\n", 50);
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].col, 50 - chips[0].label.chars().count());

    // Text reaching the row's end: no room anywhere, so no chip.
    let full = format!("src/app_state.rs {}\n", "x".repeat(33));
    assert!(chips_for(&full, 50).is_empty());
}

#[test]
fn chips_on_the_same_row_do_not_overlap() {
    // notes.md has text after it, so it takes the gutter; app_state.rs has
    // blank cells after it.
    let chips = chips_for("notes.md src/app_state.rs\n", 60);
    assert_eq!(chips.len(), 2);
    let spans: Vec<(usize, usize)> = chips
        .iter()
        .map(|c| (c.col, c.col + c.label.chars().count() - 1))
        .collect();
    assert!(
        spans[0].1 < spans[1].0 || spans[1].1 < spans[0].0,
        "{spans:?}"
    );
}

#[test]
fn place_chip_rules() {
    let blank = |s: &str| s.chars().map(|c| c == ' ').collect::<Vec<bool>>();
    // Path ends at col 3: separator col 4, chip cols 5-7, trailer col 8.
    assert_eq!(place_chip(&blank("abcd      "), 3, 3, &[]), Some(5));
    // The chip may end exactly at the row's end.
    assert_eq!(place_chip(&blank("abcd    "), 3, 3, &[]), Some(5));
    // Not enough room after: the gutter needs a blank before it too.
    assert_eq!(place_chip(&blank("abcd x    "), 3, 3, &[]), Some(7));
    assert_eq!(place_chip(&blank("abcd xyzw "), 3, 3, &[]), None);
    // Taken ranges block a position.
    assert_eq!(place_chip(&blank("abcd      "), 3, 3, &[(5, 9)]), None);
}

#[test]
fn no_index_means_no_chips_and_identical_spans() {
    let term = term_with(60, 4, "Update(src/app_state.rs) https://x.io\n");
    let without = scan_grid(&term, Some(Path::new(REPO)), None);
    let with = scan_grid(&term, Some(Path::new(REPO)), Some(&sample_index()));
    assert!(without.chips.is_empty());
    assert_eq!(without.path_spans, with.path_spans);
    assert_eq!(without.url_spans, with.url_spans);
    assert_eq!(with.url_spans.len(), 1);
}

// ── Peek ────────────────────────────────────────────────────────────

const TWO_HUNKS: &str = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n\
@@ -40,3 +40,4 @@\n x\n y\n+z\n w\n";

#[test]
fn peek_shows_the_first_hunk_by_default() {
    let body = hunk_window(TWO_HUNKS, None, PEEK_MAX_LINES);
    let PeekBody::Diff {
        lines,
        hunk,
        hunks,
        first_changed_line,
    } = &body
    else {
        panic!("expected a diff, got {body:?}");
    };
    assert_eq!((*hunk, *hunks), (1, 2));
    assert_eq!(*first_changed_line, Some(2));
    assert_eq!(lines[0].kind, PeekLineKind::Hunk);
    assert_eq!(lines.len(), 1 + 4);
}

#[test]
fn peek_picks_the_hunk_nearest_a_line_suffix() {
    let body = hunk_window(TWO_HUNKS, Some(41), PEEK_MAX_LINES);
    assert!(matches!(body, PeekBody::Diff { hunk: 2, .. }), "{body:?}");
    assert_eq!(body.first_changed_line(), Some(42));
}

#[test]
fn peek_is_capped_to_one_window() {
    let mut diff = String::from("@@ -1,0 +1,100 @@\n");
    for i in 0..100 {
        diff.push_str(&format!("+line {i}\n"));
    }
    let PeekBody::Diff { lines, .. } = hunk_window(&diff, None, PEEK_MAX_LINES) else {
        panic!("expected a diff");
    };
    assert_eq!(lines.len(), PEEK_MAX_LINES);
}

#[test]
fn peek_on_a_real_repo_diff_and_on_an_unchanged_file() -> Result<(), String> {
    let dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let repo = dir.path();
    std::fs::write(repo.join("f.txt"), "one\ntwo\nthree\n").map_err(|e| e.to_string())?;
    std::fs::write(repo.join("same.txt"), "alpha\nbeta\n").map_err(|e| e.to_string())?;
    crate::git::git_init(repo).map_err(|e| e.to_string())?;
    std::fs::write(repo.join("f.txt"), "one\nTWO\nthree\n").map_err(|e| e.to_string())?;

    let files = crate::git::status_changes(repo).map_err(|e| e.to_string())?;
    let stats = crate::git::line_stats(repo, &files);
    let index = SessionChangeIndex::build(repo, &files, &stats);
    let abs = repo.join("f.txt");
    let changed = index.get(&abs);
    assert_eq!(
        changed.map(|c| c.chip_label().to_string()).as_deref(),
        Some("+1 \u{2212}1")
    );

    let body = load_peek(repo, changed, &abs, None);
    let PeekBody::Diff { lines, .. } = &body else {
        return Err(format!("expected a diff, got {body:?}"));
    };
    assert!(lines
        .iter()
        .any(|l| l.kind == PeekLineKind::Added && l.text == "TWO"));
    assert_eq!(body.first_changed_line(), Some(2));

    let same = repo.join("same.txt");
    assert!(index.get(&same).is_none());
    assert_eq!(
        load_peek(repo, None, &same, None),
        PeekBody::FileHead(vec!["alpha".into(), "beta".into()])
    );
    Ok(())
}

// ── Bench: frame-time cost of the scan ──────────────────────────────

type Spans = Vec<(usize, usize, usize)>;

/// The scan exactly as it was before DEV-1262 (`scan_visible_spans` at
/// 01e1638), so "before" is the real baseline rather than the new code with
/// decorations switched off. The old view re-locked the terminal for every
/// row through `line_chars_cols`; here the grid is borrowed directly, which
/// can only flatter the baseline.
fn legacy_scan(term: &Term<VoidListener>) -> (Spans, Spans) {
    let mut url_spans = Vec::new();
    let mut path_spans = Vec::new();
    let grid = term.grid();
    let num_cols = grid.columns();
    let display_offset = grid.display_offset() as i32;
    for row in 0..grid.screen_lines() {
        let line = Line(row as i32 - display_offset);
        let mut chars = Vec::with_capacity(num_cols);
        let mut cols = Vec::with_capacity(num_cols);
        for col in 0..num_cols {
            let c = &grid[line][Column(col)];
            if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            chars.push(if c.c == '\0' { ' ' } else { c.c });
            cols.push(col);
        }
        if chars.is_empty() {
            continue;
        }
        scan_urls(&chars, &cols, row, &mut url_spans);
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_whitespace() {
                i += 1;
                continue;
            }
            let tok_start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            let mut tok_end = i - 1;
            let mut ts = tok_start;
            while ts < tok_end && matches!(chars[ts], '(' | '[' | '{' | '<' | '"' | '\'' | '`') {
                ts += 1;
            }
            while tok_end > ts
                && matches!(
                    chars[tok_end],
                    ')' | ']' | '}' | '>' | '"' | '\'' | '`' | ',' | ';' | '.' | '!' | '?'
                )
            {
                tok_end -= 1;
            }
            if ts > tok_end {
                continue;
            }
            let token: String = chars[ts..=tok_end].iter().collect();
            if token_looks_like_path(&token) {
                path_spans.push((row, cols[ts], cols[tok_end]));
            }
        }
    }
    (url_spans, path_spans)
}

/// A full 200×60 screen of paths, every one in the change index — the
/// worst case for the decoration join. `dense`: every word is a path, so
/// most have no room for a chip. Not dense: each row is one Claude tool
/// line with room after it, so every path gets a chip and pays the lookup.
fn full_screen_of_paths(dense: bool) -> (Term<VoidListener>, SessionChangeIndex) {
    let (cols, rows) = (200usize, 60usize);
    let mut text = String::new();
    let mut files = Vec::new();
    let mut stats = LineStats::new();
    for r in 0..rows {
        let mut line = String::new();
        let mut k = 0u32;
        loop {
            let path = format!("src/m{r}/file_{k}.rs");
            let word = if k.is_multiple_of(2) {
                format!("Update({path}) ")
            } else {
                format!("{path}:{k} ")
            };
            if line.len() + word.len() > cols || (!dense && k == 1) {
                break;
            }
            line.push_str(&word);
            files.push(file(&path, ChangeKind::Modified, false));
            stats.insert((path, false), stat(k + 1, k));
            k += 1;
        }
        text.push_str(&line);
        if r + 1 < rows {
            text.push('\n');
        }
    }
    let term = term_with(cols, rows, &text);
    let index = SessionChangeIndex::build(Path::new(REPO), &files, &stats);
    (term, index)
}

/// Best of the samples. CI runs the whole suite in parallel on a shared
/// runner, so a sample is the scan's cost *plus* whatever preempted it; the
/// minimum is the estimate that noise cannot inflate (a median measured
/// +340µs there against +36µs on an idle machine).
fn best_of(samples: &[Duration]) -> Duration {
    samples.iter().copied().min().unwrap_or_default()
}

/// Acceptance bench: on a full screen of paths, the decorated scan must add
/// no measurable frame time over the scan it replaced. "Measurable" is held
/// to 1% of a 60 Hz frame (167µs), best of 201 interleaved runs, in whatever profile the
/// tests run in — debug is the pessimistic one. For the numbers:
/// `cargo test scan_bench -- --nocapture`.
#[test]
fn scan_bench_full_screen_of_paths() {
    for dense in [true, false] {
        bench_one(dense);
    }
}

// `cfg(test)` is redundant here, but it is what tells the print-macro rule in
// tests/architecture.rs that this file-level helper is test code.
#[cfg(test)]
fn bench_one(dense: bool) {
    let (term, index) = full_screen_of_paths(dense);
    let wd = Some(Path::new(REPO));

    let out = scan_grid(&term, wd, Some(&index));
    assert_eq!(legacy_scan(&term).1.len(), out.path_spans.len());
    if dense {
        assert!(
            out.path_spans.len() >= 400,
            "{} spans",
            out.path_spans.len()
        );
    } else {
        assert_eq!(out.chips.len(), 60, "every row's path is decorated");
    }

    // Warm up, then interleave so drift hits every side equally.
    for _ in 0..5 {
        std::hint::black_box(legacy_scan(&term));
        std::hint::black_box(scan_grid(&term, wd, Some(&index)));
    }
    let (mut before, mut bare, mut after) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..201 {
        let t = Instant::now();
        std::hint::black_box(legacy_scan(&term));
        before.push(t.elapsed());
        let t = Instant::now();
        std::hint::black_box(scan_grid(&term, wd, None));
        bare.push(t.elapsed());
        let t = Instant::now();
        std::hint::black_box(scan_grid(&term, wd, Some(&index)));
        after.push(t.elapsed());
    }
    let (before, bare, after) = (best_of(&before), best_of(&bare), best_of(&after));
    let frame = Duration::from_micros(16_667);
    let added = after.saturating_sub(before);
    println!(
        "scan bench 200x60 {}, {} paths, {} chips: before {before:?} | after, no index {bare:?} \
         | after, every path changed {after:?} | added {added:?} = {:.2}% of a 60 Hz frame",
        if dense {
            "dense"
        } else {
            "one tool line per row"
        },
        out.path_spans.len(),
        out.chips.len(),
        added.as_secs_f64() / frame.as_secs_f64() * 100.0
    );
    assert!(
        added <= frame / 100,
        "decorated scan added {added:?} over the {before:?} baseline"
    );
}
