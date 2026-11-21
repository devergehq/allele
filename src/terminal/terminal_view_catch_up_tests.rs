//! DEV-1353: with the catch-up strip open, Esc folds it and writes nothing to
//! the PTY; the first key typed folds it *and* reaches the PTY.
//!
//! Drives a real `TerminalView` (real PTY, running `sleep`) and reads its PTY
//! byte counter, as `terminal_view_overlay_tests.rs` does.
//!
//! No `unwrap`/`expect` here: the panic ratchet in tests/architecture.rs
//! cannot see that this file is test-only.

use super::{InitialFocus, ShellCommand, TerminalEvent, TerminalView};
use gpui::{Entity, TestAppContext, VisualTestContext};
use std::cell::RefCell;
use std::rc::Rc;

fn written(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> usize {
    view.update(cx, |tv, _| {
        tv.terminal.as_ref().map_or(0, |t| t.bytes_written())
    })
}

fn folds(events: &Rc<RefCell<Vec<TerminalEvent>>>) -> usize {
    events
        .borrow()
        .iter()
        .filter(|e| matches!(e, TerminalEvent::FoldCatchUp))
        .count()
}

#[gpui::test]
fn esc_folds_without_typing_and_the_first_key_types_and_folds(cx: &mut TestAppContext) {
    let (view, cx) = cx.add_window_view(move |window, cx| {
        TerminalView::new(
            window,
            cx,
            Some(ShellCommand::with_args_env(
                "/bin/sleep",
                vec!["30".into()],
                Vec::new(),
            )),
            None,
            13.0,
            Vec::new(),
            InitialFocus::Take,
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
    cx.run_until_parked();

    // Esc with the strip open: folded, nothing written.
    view.update(cx, |tv, _| tv.catch_up_open = true);
    let before = written(&view, cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(written(&view, cx), before, "Esc reached the PTY");
    assert_eq!(folds(&events), 1);
    assert!(!view.update(cx, |tv, _| tv.catch_up_open));

    // The first key with the strip open: written, and folded.
    view.update(cx, |tv, _| tv.catch_up_open = true);
    let before = written(&view, cx);
    cx.simulate_keystrokes("r");
    cx.run_until_parked();
    assert!(written(&view, cx) > before, "the key did not reach the PTY");
    assert_eq!(folds(&events), 2);

    // With the strip folded, Esc goes to the PTY as it always did.
    let before = written(&view, cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        written(&view, cx) > before,
        "Esc was swallowed with the strip folded"
    );
    assert_eq!(folds(&events), 2);
}
