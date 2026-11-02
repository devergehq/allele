//! Mirror tests: a real alacritty `Term` is the host; frames go through JSON,
//! as they will on the wire; a `MirroredGrid` is the client. "Matches" always
//! means the two `GridView`s agree cell for cell, which is everything the grid
//! element reads, so equal views draw equal pixels.
//!
//! No `unwrap`/`expect` here: the panic ratchet in tests/architecture.rs
//! cannot see that this file is test-only.

use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use super::model::{Cell, Row, ScreenFrame, WireRow};
use super::{capture, capture_history, DiffProducer, MirroredGrid, Poll, MIN_FRAME_INTERVAL};
use crate::terminal::grid_source::GridView;

// ── Harness ────────────────────────────────────────────────────────

struct Host {
    term: Term<VoidListener>,
    parser: Processor<StdSyncHandler>,
}

impl Host {
    fn new(columns: usize, lines: usize, history: usize) -> Self {
        let config = Config {
            scrolling_history: history,
            ..Default::default()
        };
        Self {
            term: Term::new(config, &TermSize::new(columns, lines), VoidListener),
            parser: Processor::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn host_row(&self, line: i32) -> Row {
        let columns = self.term.grid().columns();
        let row = &self.term.grid()[Line(line)];
        (0..columns)
            .map(|c| Cell::from_term(&row[Column(c)]))
            .collect()
    }
}

/// A client: applies frames after a JSON round trip, like the wire will.
struct Client {
    grid: MirroredGrid,
    bytes: usize,
    frames: usize,
    /// Diffs that expressed a scroll, and diffs that fell back to a reset.
    scrolls: usize,
    resets: usize,
}

impl Client {
    fn new() -> Self {
        Self {
            grid: MirroredGrid::new(),
            bytes: 0,
            frames: 0,
            scrolls: 0,
            resets: 0,
        }
    }

    fn receive(&mut self, frame: &ScreenFrame) {
        let json = serde_json::to_vec(frame).unwrap_or_else(|e| panic!("encode: {e}"));
        let back: ScreenFrame =
            serde_json::from_slice(&json).unwrap_or_else(|e| panic!("decode: {e}"));
        assert_eq!(&back, frame, "frame must survive JSON unchanged");
        self.bytes += json.len();
        self.frames += 1;
        if let ScreenFrame::Diff(d) = frame {
            self.scrolls += usize::from(d.scrolled > 0);
            self.resets += usize::from(d.history_reset);
        }
        self.grid
            .apply(&back)
            .unwrap_or_else(|e| panic!("apply: {e}"));
    }
}

/// Deterministic xorshift, so a failing seed can be replayed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Everything the grid element reads must agree, at the current scroll.
fn assert_views_match(host: &dyn GridView, mirror: &dyn GridView, context: &str) {
    assert_eq!(host.columns(), mirror.columns(), "{context}: columns");
    assert_eq!(
        host.screen_lines(),
        mirror.screen_lines(),
        "{context}: lines"
    );
    assert_eq!(host.total_lines(), mirror.total_lines(), "{context}: total");
    assert_eq!(
        host.display_offset(),
        mirror.display_offset(),
        "{context}: offset"
    );
    assert_eq!(host.cursor(), mirror.cursor(), "{context}: cursor");
    let offset = host.display_offset() as i32;
    for screen_line in 0..host.screen_lines() as i32 {
        let line = screen_line - offset;
        for col in 0..host.columns() {
            assert_eq!(
                host.cell(line, col),
                mirror.cell(line, col),
                "{context}: cell line {line} col {col}"
            );
        }
    }
}

/// The screen, cell for cell including extras the renderer skips.
fn assert_screen_exact(host: &Host, mirror: &MirroredGrid, context: &str) {
    for line in 0..host.term.grid().screen_lines() as i32 {
        assert_eq!(
            Some(&host.host_row(line)),
            mirror.row(line),
            "{context}: screen line {line}"
        );
    }
}

/// Every scrollback line the mirror has loaded equals the host's.
fn assert_loaded_history_exact(host: &Host, mirror: &MirroredGrid, context: &str) {
    for i in 0..mirror.loaded_history() {
        let line = -(i as i32) - 1;
        assert_eq!(
            Some(&host.host_row(line)),
            mirror.row(line),
            "{context}: history line {line}"
        );
    }
}

/// Random terminal output: text, colours, attributes, cursor motion, erases,
/// newlines (scrolling), wide and combining characters, clears, alt screen.
fn random_output(rng: &mut Rng, columns: usize, lines: usize) -> Vec<u8> {
    let mut out = String::new();
    for _ in 0..1 + rng.below(12) {
        match rng.below(14) {
            0..=3 => {
                for _ in 0..rng.below(40) {
                    out.push((b'!' + rng.below(90) as u8) as char);
                }
            }
            4 => out.push_str(&format!("\x1b[{}m", 30 + rng.below(8))),
            5 => out.push_str(&format!("\x1b[48;5;{}m", rng.below(256))),
            6 => out.push_str(&format!(
                "\x1b[38;2;{};{};{}m",
                rng.below(256),
                rng.below(256),
                rng.below(256)
            )),
            7 => out.push_str(
                ["\x1b[1m", "\x1b[3m", "\x1b[4m", "\x1b[7m", "\x1b[0m"][rng.below(5) as usize],
            ),
            8 => out.push_str(&format!(
                "\x1b[{};{}H",
                1 + rng.below(lines as u64),
                1 + rng.below(columns as u64)
            )),
            9 => out.push_str(["\x1b[K", "\x1b[1K", "\x1b[2K", "\x1b[J"][rng.below(4) as usize]),
            10 => {
                for _ in 0..1 + rng.below(2 * lines as u64) {
                    out.push_str("\r\n");
                }
            }
            11 => out.push_str(["日本語", "e\u{301}", "🦀", "\t"][rng.below(4) as usize]),
            12 => out.push_str(
                ["\x1b[?25l", "\x1b[?25h", "\x1b[5 q", "\x1b[3 q", "\x1b[2 q"]
                    [rng.below(5) as usize],
            ),
            _ => match rng.below(10) {
                0 => out.push_str("\x1b[?1049h"),
                1 => out.push_str("\x1b[?1049l"),
                2 => out.push_str("\x1b[2J"),
                _ => out.push_str("\r\n"),
            },
        }
    }
    out.into_bytes()
}

fn frame_from(poll: Poll) -> Option<ScreenFrame> {
    match poll {
        Poll::Frame(f) => Some(f),
        Poll::Unchanged | Poll::Wait(_) => None,
    }
}

// ── Model ──────────────────────────────────────────────────────────

#[test]
fn wire_rows_round_trip_styles_wide_and_combining_characters() {
    let mut host = Host::new(30, 3, 0);
    host.feed("\x1b[1;31mred\x1b[0m 日本 e\u{301} \x1b[4:3m\x1b[58;5;99mcurl\x1b[0m".as_bytes());
    host.feed(b"\r\n\x1b[48;2;1;2;3m  bg  \x1b[0m   ");
    for line in 0..3 {
        let row = host.host_row(line);
        let wire = WireRow::encode(&row);
        let json = serde_json::to_string(&wire).unwrap_or_else(|e| panic!("{e}"));
        let back: WireRow = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back.decode(30).ok(), Some(row), "line {line} via {json}");
    }
    // Trailing blanks are not sent at all.
    assert_eq!(WireRow::encode(&host.host_row(2)), WireRow::default());
}

#[test]
fn a_row_wider_than_the_grid_is_rejected() {
    let mut host = Host::new(10, 1, 0);
    host.feed(b"0123456789");
    let wire = WireRow::encode(&host.host_row(0));
    assert!(wire.decode(10).is_ok());
    assert!(wire.decode(9).is_err());
}

#[test]
fn snapshot_round_trip_reproduces_the_host_screen() {
    let mut host = Host::new(80, 24, 1000);
    host.feed(b"\x1b[32mgreen\x1b[0m \x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m\r\n");
    host.feed("\x1b[38;2;250;100;0mtruecolour\x1b[0m 日本語 🦀\r\n$ ".as_bytes());
    host.feed(b"\x1b[5 q"); // blinking beam cursor

    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    client.receive(&producer.snapshot(capture(&host.term), Instant::now()));

    assert_views_match(&host.term, &client.grid, "snapshot");
    assert_screen_exact(&host, &client.grid, "snapshot");
    assert_eq!(client.grid.cursor().line, 2);
    assert_eq!(client.grid.cursor().column, 2);
}

// ── Diffs ──────────────────────────────────────────────────────────

#[test]
fn diffs_reproduce_the_host_after_random_output() {
    let (mut scrolls, mut resets, mut unexplained) = (0, 0, 0);
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (columns, lines) = (20 + rng.below(40) as usize, 5 + rng.below(15) as usize);
        let mut host = Host::new(columns, lines, 200);
        let mut producer = DiffProducer::default().with_history_limit(200);
        let mut client = Client::new();
        let mut now = Instant::now();
        client.receive(&producer.snapshot(capture(&host.term), now));

        for step in 0..150 {
            let before = client.grid.history_size();
            host.feed(&random_output(&mut rng, columns, lines));
            now += MIN_FRAME_INTERVAL;
            if let Some(frame) = frame_from(producer.poll(now, || capture(&host.term))) {
                // A reset is expected when scrollback shrank (2J clears it in
                // Allele's alacritty fork; the alt screen has none), grew by
                // more than a screenful, or is full, where blank history can
                // align at several counts. Below the limit the count is exact,
                // so any other reset is a detector bug.
                if let ScreenFrame::Diff(d) = &frame {
                    let after = d.history_size as usize;
                    let explained = after < before || after - before > lines || after == 200;
                    if d.history_reset && !explained {
                        unexplained += 1;
                    }
                }
                client.receive(&frame);
            }
            let context = format!("seed {seed} step {step}");
            assert_views_match(&host.term, &client.grid, &context);
            assert_screen_exact(&host, &client.grid, &context);
            assert_loaded_history_exact(&host, &client.grid, &context);
        }
        scrolls += client.scrolls;
        resets += client.resets;
    }
    println!(
        "random output: {scrolls} scroll diffs, {resets} history resets \
         ({unexplained} not explained by a clear, a jump, or full scrollback)"
    );
    assert!(
        scrolls > 500,
        "too few scrolls ({scrolls}) for the history checks to mean much"
    );
    assert_eq!(
        unexplained, 0,
        "below the limit every scroll count must be exact"
    );
}

#[test]
fn a_scroll_sends_only_the_new_rows() {
    let mut host = Host::new(40, 10, 100);
    for i in 0..10 {
        host.feed(format!("line {i}\r\n").as_bytes());
    }
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let mut now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));

    host.feed(b"line 10\r\nline 11\r\nline 12\r\n");
    now += MIN_FRAME_INTERVAL;
    let Some(ScreenFrame::Diff(diff)) = frame_from(producer.poll(now, || capture(&host.term)))
    else {
        panic!("expected a diff");
    };
    assert_eq!(diff.scrolled, 3);
    assert!(!diff.history_reset);
    // Rows 6..8 hold lines 10..12; row 9 (the empty prompt line) is unchanged
    // after the shift because it was blank before too.
    let changed: Vec<u16> = diff.rows.iter().map(|(l, _)| *l).collect();
    assert_eq!(changed, vec![6, 7, 8]);

    client.receive(&ScreenFrame::Diff(diff));
    assert_screen_exact(&host, &client.grid, "after scroll");
    // The three lines that scrolled off are now loaded scrollback.
    assert_eq!(client.grid.loaded_history(), 3);
    assert_loaded_history_exact(&host, &client.grid, "after scroll");
}

#[test]
fn an_unchanged_screen_sends_nothing() {
    let mut host = Host::new(40, 10, 100);
    host.feed(b"hello");
    let mut producer = DiffProducer::default();
    let now = Instant::now();
    producer.snapshot(capture(&host.term), now);
    let poll = producer.poll(now + MIN_FRAME_INTERVAL, || capture(&host.term));
    assert!(matches!(poll, Poll::Unchanged), "{poll:?}");
}

#[test]
fn a_cursor_move_alone_is_a_frame() {
    let mut host = Host::new(40, 10, 100);
    host.feed(b"hello");
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));
    host.feed(b"\x1b[5;7H");
    let Some(frame) = frame_from(producer.poll(now + MIN_FRAME_INTERVAL, || capture(&host.term)))
    else {
        panic!("expected a frame");
    };
    client.receive(&frame);
    assert_eq!(
        (client.grid.cursor().line, client.grid.cursor().column),
        (4, 6)
    );
}

#[test]
fn a_resize_sends_a_full_snapshot() {
    let mut host = Host::new(40, 10, 100);
    host.feed(b"before\r\n");
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));

    host.term.resize(TermSize::new(60, 15));
    let frame = frame_from(producer.poll(now + MIN_FRAME_INTERVAL, || capture(&host.term)));
    assert!(matches!(frame, Some(ScreenFrame::Snapshot(_))), "{frame:?}");
    if let Some(frame) = frame {
        client.receive(&frame);
    }
    assert_views_match(&host.term, &client.grid, "after resize");
}

#[test]
fn a_diff_out_of_sequence_is_rejected_and_changes_nothing() {
    let mut host = Host::new(40, 10, 100);
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));
    host.feed(b"one");
    let _skipped = producer.poll(now + MIN_FRAME_INTERVAL, || capture(&host.term));
    host.feed(b" two");
    let Some(frame) =
        frame_from(producer.poll(now + 2 * MIN_FRAME_INTERVAL, || capture(&host.term)))
    else {
        panic!("expected a frame");
    };
    assert!(client.grid.apply(&frame).is_err());
    assert_eq!(client.grid.cell(0, 0).c, ' ', "mirror must be untouched");
}

// ── Rate cap and coalescing ────────────────────────────────────────

#[test]
fn frames_are_capped_at_one_per_interval_without_taking_the_lock() {
    let mut host = Host::new(40, 10, 100);
    let mut producer = DiffProducer::default();
    let t0 = Instant::now();
    producer.snapshot(capture(&host.term), t0);
    host.feed(b"x");

    let mut captured = false;
    let poll = producer.poll(t0 + Duration::from_millis(10), || {
        captured = true;
        capture(&host.term)
    });
    assert!(
        matches!(poll, Poll::Wait(d) if d == Duration::from_millis(23)),
        "{poll:?}"
    );
    assert!(!captured, "no capture (so no lock) while the cap holds");

    let poll = producer.poll(t0 + MIN_FRAME_INTERVAL, || capture(&host.term));
    assert!(matches!(poll, Poll::Frame(_)), "{poll:?}");
}

#[test]
fn a_stalled_consumer_gets_one_frame_for_the_present_not_a_replay() {
    let mut host = Host::new(80, 24, 10_000);
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let mut now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));

    // 30 seconds of output the consumer never polls for: a stalled link.
    for i in 0..900 {
        host.feed(format!("\x1b[3{}mbuild step {i}\x1b[0m\r\n", i % 8).as_bytes());
        now += MIN_FRAME_INTERVAL;
    }
    host.feed(b"$ ");

    let Some(frame) = frame_from(producer.poll(now, || capture(&host.term))) else {
        panic!("expected one catch-up frame");
    };
    client.receive(&frame);
    assert_eq!(client.frames, 2, "snapshot plus exactly one catch-up frame");
    assert_views_match(&host.term, &client.grid, "after stall");
    assert_screen_exact(&host, &client.grid, "after stall");

    // Caught up: nothing further is owed.
    let poll = producer.poll(now + MIN_FRAME_INTERVAL, || capture(&host.term));
    assert!(matches!(poll, Poll::Unchanged), "{poll:?}");
}

// ── Scrollback ─────────────────────────────────────────────────────

#[test]
fn scrolling_up_on_the_client_loads_host_scrollback_in_pages() {
    let mut host = Host::new(40, 10, 1000);
    for i in 0..300 {
        host.feed(format!("\x1b[1mline\x1b[0m {i}\r\n").as_bytes());
    }
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let mut now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));
    assert_eq!(client.grid.history_size(), host.term.grid().history_size());
    assert_eq!(client.grid.loaded_history(), 0);

    for _ in 0..25 {
        client.grid.scroll_display(10);
        host.term.scroll_display(Scroll::Delta(10));
        // Output keeps arriving while the client reads back.
        host.feed(b"more\r\n");
        now += MIN_FRAME_INTERVAL;
        while let Some((start, count)) = client.grid.wanted_page(50) {
            let page = capture_history(&host.term, start, count);
            for frame in producer.scrollback(capture(&host.term), page, start, now) {
                client.receive(&frame);
            }
        }
        if let Some(frame) = frame_from(producer.poll(now, || capture(&host.term))) {
            client.receive(&frame);
        }
        assert_views_match(&host.term, &client.grid, "scrolled back");
        assert_loaded_history_exact(&host, &client.grid, "scrolled back");
    }
    assert!(client.grid.display_offset() > 0);
}

// ── Measurements ───────────────────────────────────────────────────

fn busy_200x60_host() -> Host {
    let mut host = Host::new(200, 60, 10_000);
    let mut rng = Rng(0xC0FFEE);
    for i in 0..12_000 {
        let line: String = (0..150)
            .map(|_| (b'a' + rng.below(26) as u8) as char)
            .collect();
        host.feed(
            format!(
                "\x1b[3{}m{i:05}\x1b[0m \x1b[1m{}\x1b[0m {}\r\n",
                i % 8,
                &line[..20],
                &line[20..]
            )
            .as_bytes(),
        );
    }
    host
}

/// DEV-1165 acceptance: holding the `Term` lock to capture a 200×60 screen
/// takes under 2 ms. Asserted in release builds only (`cargo test --release`);
/// debug builds print the number but are not representative.
#[test]
fn capture_lock_hold_time_at_200x60() {
    use alacritty_terminal::sync::FairMutex;
    let term = FairMutex::new(busy_200x60_host().term);

    let mut samples: Vec<Duration> = (0..300)
        .map(|_| {
            let start = Instant::now();
            let guard = term.lock();
            let cap = capture(&guard);
            drop(guard);
            let held = start.elapsed();
            assert_eq!(cap.screen.len(), 60);
            held
        })
        .collect();
    samples.sort();
    let median = samples[samples.len() / 2];
    let p99 = samples[samples.len() * 99 / 100];
    let max = samples[samples.len() - 1];
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!("capture lock hold 200x60 ({build}): median {median:?}, p99 {p99:?}, max {max:?}");
    if !cfg!(debug_assertions) {
        assert!(
            p99 < Duration::from_millis(2),
            "p99 {p99:?} over the 2 ms budget"
        );
    }
}

/// Bytes on the wire for steady scrolling output at 200×60, as JSON frames.
/// A synthetic stand-in for `cargo build`; the real measurement belongs to
/// the networked stage.
#[test]
fn scrolling_output_bandwidth_estimate() {
    let mut host = busy_200x60_host();
    let mut producer = DiffProducer::default();
    let mut client = Client::new();
    let mut now = Instant::now();
    client.receive(&producer.snapshot(capture(&host.term), now));
    let snapshot_bytes = client.bytes;

    // 10 simulated seconds at ~600 lines/s (20 lines per 33 ms frame).
    let mut line = 0;
    for _ in 0..300 {
        for _ in 0..20 {
            host.feed(
                format!("   \x1b[32;1mCompiling\x1b[0m crate-{line} v0.{line}.0 (/src/crate-{line})\r\n")
                    .as_bytes(),
            );
            line += 1;
        }
        host.feed(b"\x1b[36;1m    Building\x1b[0m [=====>    ] 123/456\r");
        now += MIN_FRAME_INTERVAL;
        if let Some(frame) = frame_from(producer.poll(now, || capture(&host.term))) {
            client.receive(&frame);
        }
    }
    assert_views_match(&host.term, &client.grid, "after build");
    // Scrollback is full and this producer was not told the limit, so this
    // is the alignment path: every pushed line must still be exact.
    assert!(client.grid.loaded_history() >= 6000);
    assert_loaded_history_exact(&host, &client.grid, "after build");
    let diff_bytes = client.bytes - snapshot_bytes;
    let seconds = 300.0 * MIN_FRAME_INTERVAL.as_secs_f64();
    assert_eq!(
        client.resets, 0,
        "steady scrolling must not reset scrollback"
    );
    println!(
        "bandwidth: snapshot {snapshot_bytes} B; scrolling {} frames ({} scrolls), {:.1} KB/s",
        client.frames - 1,
        client.scrolls,
        diff_bytes as f64 / seconds / 1024.0
    );
}
