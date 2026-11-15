//! DEV-677: a terminal built with `InitialFocus::Leave` must not take the
//! keyboard from the terminal that has it, and one built with
//! `InitialFocus::Take` still must.
//!
//! A dispatched session's terminal is built when its clone lands, which is
//! whenever provisioning happens to finish. If building it takes focus, the
//! person typing in another session loses keystrokes mid-word. Drives real
//! `TerminalView`s (real PTYs, running `sleep`) in one gpui test window.
//!
//! No `unwrap`/`expect` here: the panic ratchet in tests/architecture.rs
//! cannot see that this file is test-only.

// Named imports, not `super::*`: the glob would bring in `gpui::test`,
// which shadows `#[test]` inside `#[gpui::test]`'s expansion and recurses.
use super::{InitialFocus, ShellCommand, TerminalView};
use gpui::{AppContext, Context, Entity, TestAppContext, VisualTestContext, Window};

fn sleeper(
    window: &mut Window,
    cx: &mut Context<TerminalView>,
    focus: InitialFocus,
) -> TerminalView {
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
        focus,
    )
}

fn focused(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|window, app| view.read(app).focus_handle.is_focused(window))
}

#[gpui::test]
fn a_terminal_built_to_leave_focus_does_not_take_it(cx: &mut TestAppContext) {
    // The session the person is typing in.
    let (typing, cx) = cx.add_window_view(|window, cx| sleeper(window, cx, InitialFocus::Take));
    cx.run_until_parked();
    assert!(
        focused(&typing, cx),
        "a terminal built to take focus did not get it"
    );

    // A dispatched session's terminal, built while they type.
    let dispatched =
        cx.update(|window, app| app.new(|cx| sleeper(window, cx, InitialFocus::Leave)));
    cx.run_until_parked();
    assert!(
        focused(&typing, cx),
        "building a terminal with InitialFocus::Leave took the keyboard"
    );
    assert!(!focused(&dispatched, cx));

    // Negative space: a terminal a person asked for still takes it.
    let by_hand = cx.update(|window, app| app.new(|cx| sleeper(window, cx, InitialFocus::Take)));
    cx.run_until_parked();
    assert!(
        focused(&by_hand, cx),
        "building a terminal with InitialFocus::Take no longer takes the keyboard"
    );
    assert!(!focused(&typing, cx));
}

#[test]
fn only_a_dispatched_session_leaves_focus() {
    assert_eq!(InitialFocus::unless_dispatched(true), InitialFocus::Leave);
    assert_eq!(InitialFocus::unless_dispatched(false), InitialFocus::Take);
}
