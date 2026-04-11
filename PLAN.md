# Allele — POC Build Plan

**Goal:** Prove the GPUI + alacritty_terminal architecture works for a Claude Code session manager. Each phase produces a runnable checkpoint — if any phase reveals a blocker, we have a clear fallback to Swift/AppKit.

---

## Phase 0: GPUI Proof of Life ← START HERE
**Goal:** Confirm GPUI compiles, renders a window, and we can work with it.
**Deliverable:** A window with a sidebar (static text) and a main content area.

- [ ] `cargo build` succeeds with all pinned dependencies
- [ ] GPUI window opens on macOS with Metal rendering
- [ ] Sidebar panel (240px, dark background) renders on the left
- [ ] Main content area fills remaining space
- [ ] Window title shows "Allele"

**Risk gate:** If GPUI fails to compile or the API has changed from the pinned rev, we stop and evaluate. This phase should take <1 day. If it takes longer, that's a signal.

**Reference:** Study Termy's `src/main.rs` and `crates/terminal_ui/` for GPUI app bootstrapping patterns.

---

## Phase 1: Terminal in a Box
**Goal:** Render a real PTY-backed terminal inside the GPUI window. This is the hardest phase and the one that validates the entire architecture.
**Deliverable:** A GPUI window with a working terminal that can run `bash` or any CLI.

- [ ] Spawn a PTY subprocess (`rustix-openpty` or `portable-pty`)
- [ ] Connect PTY to `alacritty_terminal::Term` for VTE parsing
- [ ] Render terminal grid (rows/columns of cells) in GPUI
- [ ] Handle keyboard input → write to PTY
- [ ] Handle PTY output → update terminal state → re-render
- [ ] Basic ANSI colour rendering (16-colour minimum)
- [ ] Cursor rendering (block cursor, blink optional)
- [ ] Terminal resizes when window resizes

**Risk gate:** If terminal rendering doesn't work or has critical fidelity issues, we evaluate the Swift/AppKit fallback. This is the make-or-break phase.

**Reference:** Study how Termy and Zed's `crates/terminal/` implement the PTY↔alacritty_terminal↔GPUI pipeline. Key pattern: 4ms batching interval to coalesce PTY events before triggering re-render.

---

## Phase 2: Run Claude Code
**Goal:** Launch `claude` CLI specifically (not just bash) and confirm PAI output renders correctly.
**Deliverable:** Claude Code running inside the embedded terminal with full output fidelity.

- [ ] Spawn `claude` as the PTY subprocess (detect claude binary path)
- [ ] Claude Code's ANSI output renders correctly (colours, box drawing, cursor control)
- [ ] Interactive prompts work (tool approval, Y/n confirmations)
- [ ] Extended thinking / verbose output visible (not hidden or truncated)
- [ ] PAI Algorithm output renders (phase headers, emoji, formatted blocks)
- [ ] Subagent activity visible in terminal output
- [ ] Ctrl+C sends SIGINT correctly
- [ ] Terminal scrollback works (scroll up to see history)

**Validation:** Run the same PAI workflow in both native terminal and Allele. Compare output line-by-line. Any discrepancy is a bug.

---

## Phase 3: Multiple Sessions + Sidebar
**Goal:** Run multiple Claude Code sessions and switch between them via the sidebar.
**Deliverable:** Sidebar lists sessions with status. Clicking switches the terminal view.

- [ ] Session data model: `Project { name, path, sessions: Vec<Session> }`
- [ ] Session struct: `{ id, status, pty, terminal_state, started_at }`
- [ ] Sidebar renders list of sessions with status icons (● running, ○ idle, ✓ done)
- [ ] Click session in sidebar → swap terminal view to that session's PTY
- [ ] Non-visible sessions continue running in background (PTY still reads)
- [ ] Process exit detection → status updates to "done"
- [ ] "New Session" button → spawns new claude instance
- [ ] State persists to `~/.cc-multiplex/state.json` on change
- [ ] State loads on app startup (reconnect to still-running PTYs if possible)
- [ ] Add/remove projects via sidebar (path picker or manual input)

---

## Phase 4: APFS Clone Management
**Goal:** Create copy-on-write workspace clones for session isolation.
**Deliverable:** Each session can optionally run in an APFS clone of the project.

- [ ] `clonefile()` FFI wrapper — clone directory tree atomically
- [ ] Workspace model: `Workspace { name, clone_path, source_project }`
- [ ] "New Workspace" action → creates APFS clone at `~/.cc-multiplex/workspaces/{project}/{name}/`
- [ ] Session launched in workspace runs `claude` with cwd set to clone path
- [ ] "Delete Workspace" action → `rm -rf` clone (with confirmation)
- [ ] Clone path shown in sidebar/status bar
- [ ] Handle same-volume check (clonefile requires same APFS volume)
- [ ] Handle EEXIST error (destination already exists)

---

## Phase 5: Polish + Session Status
**Goal:** Quality of life — status detection, notifications, keyboard shortcuts.
**Deliverable:** A daily-driver-ready POC.

- [ ] PTY activity monitoring → idle detection (no output for >30s)
- [ ] Claude Code hooks integration (if available) for richer status
- [ ] Status bar at bottom: current session name, status, elapsed time
- [ ] Keyboard shortcuts: Cmd+1-9 for session switching, Cmd+N for new session
- [ ] macOS system notification on session completion (optional)
- [ ] Session elapsed time display
- [ ] Graceful shutdown — warn about running sessions on quit

---

## Architecture Reference

```
src/
├── main.rs              # GPUI app bootstrap, window setup
├── terminal/
│   └── mod.rs           # PTY + alacritty_terminal + GPUI rendering
├── sidebar/
│   └── mod.rs           # Project tree, session list, status icons
├── clone/
│   └── mod.rs           # APFS clonefile() management
├── session/
│   └── mod.rs           # Session lifecycle, process management
└── state/
    └── mod.rs           # JSON persistence, app state
```

## Key References

- **Termy source:** github.com/lassejlv/termy — the closest reference project
- **Zed terminal:** github.com/zed-industries/zed/tree/main/crates/terminal — production GPUI terminal
- **GPUI book:** matinaniss.github.io/gpui-book — community documentation
- **awesome-gpui:** github.com/zed-industries/awesome-gpui — ecosystem projects
- **alacritty_terminal:** docs.rs/alacritty_terminal — terminal emulation API
