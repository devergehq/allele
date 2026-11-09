//! DEV-1262: the path chip, the ⌥-hover peek, Esc, and the path context
//! menu are overlays. None of them may write a byte to the PTY.
//!
//! Drives a real `TerminalView` (real PTY, running `sleep`) through gpui's
//! event dispatch, with Claude-style output fed straight into its grid, and
//! reads the view's own PTY byte counter. The last step types a key with
//! no overlay open, which *must* reach the PTY — so the counter is shown to
//! be live, and the zero before it means something.
//!
//! No `unwrap`/`expect` here: the panic ratchet in tests/architecture.rs
//! cannot see that this file is test-only.

// Named imports, not `super::*`: the glob would bring in `gpui::test`,
// which shadows `#[test]` inside `#[gpui::test]`'s expansion and recurses.
use super::{PeekBody, ShellCommand, TerminalEvent, TerminalView};
use crate::terminal::path_decor::SessionChangeIndex;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use gpui::{
    point, px, Entity, Modifiers, MouseButton, Pixels, Point, TestAppContext, VisualTestContext,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

fn written(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> usize {
    view.update(cx, |tv, _| {
        tv.terminal.as_ref().map_or(0, |t| t.bytes_written())
    })
}

/// Window position of the middle of grid cell (row, col).
fn cell_center(
    view: &Entity<TerminalView>,
    cx: &mut VisualTestContext,
    row: usize,
    col: usize,
) -> Point<Pixels> {
    view.update(cx, |tv, _| {
        let ox = tv
            .element_origin_x
            .load(std::sync::atomic::Ordering::Relaxed) as f32;
        let oy = tv
            .element_origin_y
            .load(std::sync::atomic::Ordering::Relaxed) as f32;
        point(
            px(ox + (col as f32 + 0.5) * tv.cell_width),
            px(oy + (row as f32 + 0.5) * tv.cell_height),
        )
    })
}

fn alt() -> Modifiers {
    Modifiers {
        alt: true,
        ..Modifiers::none()
    }
}

#[gpui::test]
fn path_overlays_write_nothing_to_the_pty(cx: &mut TestAppContext) {
    // `#[gpui::test]` drops a returned `Result`, so an `Err` must panic here
    // or it would pass silently.
    if let Err(e) = run(cx) {
        panic!("{e}");
    }
}

fn run(cx: &mut TestAppContext) -> Result<(), String> {
    // A clone with one changed file and one unchanged file.
    let dir = tempfile::TempDir::new().map_err(|e| e.to_string())?;
    let repo = dir.path().to_path_buf();
    std::fs::create_dir_all(repo.join("src")).map_err(|e| e.to_string())?;
    std::fs::write(repo.join("src/app.rs"), "fn a() {}\nfn b() {}\n").map_err(|e| e.to_string())?;
    std::fs::write(repo.join("same.rs"), "fn same() {}\n").map_err(|e| e.to_string())?;
    crate::git::git_init(&repo).map_err(|e| e.to_string())?;
    std::fs::write(repo.join("src/app.rs"), "fn a() {}\nfn B() {}\nfn c() {}\n")
        .map_err(|e| e.to_string())?;
    let files = crate::git::status_changes(&repo).map_err(|e| e.to_string())?;
    let stats = crate::git::line_stats(&repo, &files);
    let index = Arc::new(SessionChangeIndex::build(&repo, &files, &stats));

    let wd = repo.clone();
    let (view, cx) = cx.add_window_view(move |window, cx| {
        TerminalView::new(
            window,
            cx,
            Some(ShellCommand::with_args_env(
                "/bin/sleep",
                vec!["30".into()],
                Vec::new(),
            )),
            Some(wd),
            13.0,
            Vec::new(),
        )
    });

    let events: Rc<RefCell<Vec<TerminalEvent>>> = Rc::default();
    let sink = events.clone();
    cx.update(|_, app| {
        app.subscribe(&view, move |_, event: &TerminalEvent, _| {
            sink.borrow_mut().push(event.clone())
        })
        .detach();
    });

    // Claude's output, fed straight into the grid (the PTY is not involved).
    view.update(cx, |tv, cx| {
        if let Some(t) = tv.terminal.as_ref() {
            let mut parser: Processor<StdSyncHandler> = Processor::new();
            parser.advance(
                &mut *t.term.lock(),
                b"\x1b[H\x1b[2J\xe2\x8f\xba Update(src/app.rs)\r\n\
                  Read(same.rs)\r\n\
                  Update(src/\xe2\x80\xa6/app.rs)\r\n",
            );
        }
        tv.set_change_index(Some(index.clone()), cx);
    });
    cx.run_until_parked();
    let painted = view.update(cx, |tv, _| {
        tv.element_origin_x
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 0
    });
    assert!(
        painted,
        "the grid never painted, so cell positions are unknown"
    );
    let start = written(&view, cx);

    // Chip: on the changed path only — not the unchanged, not the truncated.
    let chips = view.update(cx, |tv, _| tv.visible_chips.clone());
    assert_eq!(chips.len(), 1, "{chips:?}");
    assert_eq!(chips[0].row, 0);
    assert_eq!(chips[0].label.to_string(), "+2 \u{2212}1");

    // ⌥-hover on the changed path (`src/app.rs` starts at col 9) opens its
    // diff, loaded off-thread.
    let on_changed = cell_center(&view, cx, 0, 12);
    cx.simulate_mouse_move(on_changed, None, alt());
    cx.run_until_parked();
    let body = view.update(cx, |tv, _| tv.peek().map(|p| p.body.clone()));
    match &body {
        Some(PeekBody::Diff { lines, .. }) => assert!(
            lines.iter().any(|l| l.text == "fn B() {}"),
            "peek lacks the change: {lines:?}"
        ),
        other => return Err(format!("expected a diff peek, got {other:?}")),
    }

    // Esc closes it, and is consumed.
    cx.simulate_keystrokes("escape");
    assert!(
        view.update(cx, |tv, _| tv.peek().is_none()),
        "Esc left the peek open"
    );

    // Pressing ⌥ without moving re-opens it from the last pointer position.
    cx.simulate_modifiers_change(alt());
    cx.run_until_parked();
    assert!(
        view.update(cx, |tv, _| tv.peek().is_some()),
        "⌥ press did not peek"
    );
    cx.simulate_keystrokes("escape");

    // ⌥-hover on the truncated path: no peek.
    let on_truncated = cell_center(&view, cx, 2, 12);
    cx.simulate_mouse_move(on_truncated, None, alt());
    cx.run_until_parked();
    assert!(
        view.update(cx, |tv, _| tv.peek().is_none()),
        "truncated path peeked"
    );

    // ⌥-hover on the unchanged file: its first lines.
    let on_same = cell_center(&view, cx, 1, 6);
    cx.simulate_mouse_move(on_same, None, alt());
    cx.run_until_parked();
    let body = view.update(cx, |tv, _| tv.peek().map(|p| p.body.clone()));
    assert_eq!(body, Some(PeekBody::FileHead(vec!["fn same() {}".into()])));
    cx.simulate_keystrokes("escape");

    // Right-click the changed path: the menu knows the change and, once its
    // diff loads, the first changed line.
    cx.simulate_mouse_down(on_changed, MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    let (changed, line) = view.update(cx, |tv, _| {
        tv.context_menu()
            .map(|m| (m.changed.is_some(), m.this_line()))
            .unwrap_or((false, None))
    });
    assert!(changed, "menu does not know the file changed");
    assert_eq!(line, Some(2), "first changed line");

    view.update(cx, |tv, cx| tv.menu_show_diff(cx));
    view.update(cx, |tv, cx| tv.menu_open_at_line(cx));
    cx.run_until_parked();
    let seen = events.borrow().clone();
    assert!(
        seen.iter().any(|e| matches!(
            e,
            TerminalEvent::ShowDiff { rel_path, staged: false } if rel_path == "src/app.rs"
        )),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|e| matches!(
            e,
            TerminalEvent::OpenExternalEditor { path, line_col: Some((2, None)) }
                if path.ends_with("src/app.rs")
        )),
        "{seen:?}"
    );
    // Esc closes the menu, also consumed.
    cx.simulate_keystrokes("escape");
    assert!(view.update(cx, |tv, _| tv.context_menu().is_none()));

    assert_eq!(
        written(&view, cx),
        start,
        "an overlay feature wrote to the PTY"
    );

    // The counter is live: with nothing open, Esc is the TUI's.
    cx.simulate_keystrokes("escape");
    assert!(
        written(&view, cx) > start,
        "the PTY byte counter never moves"
    );
    Ok(())
}
