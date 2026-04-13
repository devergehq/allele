# Roadmap

Build phases for the Allele POC. Each phase produces a runnable checkpoint.

---

## Phase 0: GPUI Proof of Life — DONE

- [x] `cargo build` succeeds with all pinned dependencies
- [x] GPUI window opens on macOS with Metal rendering
- [x] Sidebar panel (240px, dark background) renders on the left
- [x] Main content area fills remaining space
- [x] Window title shows "Allele"

## Phase 1: Terminal in a Box — DONE

- [x] Spawn a PTY subprocess (via `alacritty_terminal::tty`)
- [x] Connect PTY to `alacritty_terminal::Term` for VTE parsing
- [x] Render terminal grid (cell-based Element, see `docs/grid-renderer.md`)
- [x] Handle keyboard input → write to PTY
- [x] Handle PTY output → update terminal state → re-render
- [x] ANSI colour rendering (256-colour + truecolor, Catppuccin Mocha palette)
- [x] Cursor rendering (Block, Beam, Underline, HollowBlock, Hidden + blink)
- [x] Terminal resizes when window resizes

## Phase 2: Run Claude Code — DONE

- [x] Spawn `claude` as the PTY subprocess (auto-detect binary path)
- [x] Claude Code's ANSI output renders correctly (colours, box drawing, cursor control)
- [x] Interactive prompts work (tool approval, Y/n confirmations)
- [x] Extended thinking / verbose output visible
- [x] Ctrl+C sends SIGINT correctly
- [x] Terminal scrollback works (10,000 line history)

## Phase 3: Multiple Sessions + Sidebar — DONE (polish remaining)

- [x] Session data model (`Project` + `Session` structs)
- [x] Sidebar renders list of sessions with status icons (6 states)
- [x] Click session in sidebar → swap terminal view
- [x] Non-visible sessions continue running in background
- [x] Process exit detection → status updates to "done"
- [x] "New Session" button spawns new Claude instance
- [x] State persists to `~/.allele/state.json` on change (atomic writes)
- [x] State loads on app startup (cold resume via `claude --resume`)
- [x] Add/remove projects via sidebar
- [x] Suspended sessions rehydrated from disk with ⏸ icon
- [x] Session elapsed time display (live for running, frozen for done/suspended)
- [ ] **Drag-and-drop project/session reorder**

## Phase 4: APFS Clone Management — DONE (minor gap)

- [x] `clonefile(2)` FFI via `libc` — atomic COW clone
- [x] Session launched in clone directory with `claude --session-id`
- [x] "Delete Workspace" with safety checks (refuses paths outside `~/.allele/workspaces/`)
- [x] Clone path shown in status bar
- [x] EEXIST handling (alt-suffix fallback)
- [x] Trash system (clones moved to `~/.allele/trash/` on close)
- [x] Orphan sweep (clones not referenced by any session → trashed)
- [x] 14-day TTL auto-purge of trashed clones on startup
- [x] Discard confirmation flow in sidebar
- [ ] **User-friendly error message for cross-volume `EXDEV` failure**

## Phase 5: Polish + Session Status — MOSTLY DONE

- [x] PTY activity monitoring → idle detection
- [x] Claude Code hooks integration (7 hook types, auto-installed receiver script)
- [x] Status bar at bottom showing current session info
- [x] Keyboard shortcuts (Cmd+1-9, Cmd+N, Cmd+W, Cmd+[/])
- [x] macOS system notification on session completion
- [x] Session elapsed time display
- [x] Sound alerts via `afplay` (configurable per attention type)
- [x] Attention routing (AwaitingInput ⚠, ResponseReady ★)
- [ ] **Graceful shutdown — warn about running sessions on quit**

## Phase 6: Git Integration + Archive System — DONE

Added post-original-plan. Full git plumbing for clone/session merge-back.

- [x] Git availability gate on startup
- [x] Session branch creation in clones (`allele/session/<id>`)
- [x] Auto-commit dirty clones before archiving
- [x] Archive sessions into canonical refs on discard (`refs/allele/archive/<id>`)
- [x] Archive browser UI in sidebar (merge/delete actions)
- [x] Periodic archive ref pruning (matching trash TTL)
- [x] Orphan sweep archives before trashing

## Phase 7: Terminal QoL — DONE

Added post-original-plan. Terminal interaction improvements.

- [x] Text selection (mouse drag) with clipboard copy
- [x] Find/search within terminal (Cmd+F, Cmd+G / Cmd+Shift+G)
- [x] URL detection and hover + click-to-open
- [x] Font size adjustment (Cmd+/-, Cmd+0 reset)
- [x] Scrollbar with fade animation
- [x] Trackpad scroll momentum accumulation (sub-cell pixel deltas)
- [x] Policy-based keymap system (readline-friendly Option+key, Meta mode)
- [x] Per-session drawer terminal panel (Cmd+J toggle)

## Phase 8: Workspace Trust + Session Intelligence — DONE

Added post-original-plan.

- [x] Auto-trust APFS clones in `~/.claude.json` at creation time
- [x] Auto-name sessions from first prompt via LLM summarisation
- [x] Project relocation on missing `source_path`
- [x] Dirty-state confirmation before session creation
- [x] macOS `.app` bundle for clipboard history app compatibility
- [x] Stderr capture from `.app` bundle context

---

## Remaining Work

Ordered by priority. These are the known gaps.

### Must-have for daily-driver use

1. **Graceful shutdown warning** — prompt when quitting with running sessions
2. **Cross-volume `EXDEV` error handling** — detect and show a clear message when the source repo isn't on the same APFS volume as `~/.allele/`

### Nice-to-have

3. **Drag-and-drop reorder** — projects and sessions in the sidebar
4. **Right-click context menus** — project/session actions
5. **Grid renderer caching** — row-level paint cache, damage tracking (see Termy's approach in `docs/grid-renderer.md`)
6. **Box-drawing character sprites** — pixel-snapped geometric rendering for U+2500-U+257F
7. **Split terminal views** — side-by-side sessions
8. **Git status indicator per workspace**
9. **Session templates** — pre-configured Claude args
