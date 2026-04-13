# Architecture

## Technology Decision: GPUI + alacritty_terminal

Decided via 4-member council debate (Architect, Designer, Engineer, Researcher) — 3 rounds + weighted analysis. Full transcript saved to research history (2026-04-09).

### Why GPUI

**Architecture:** Pure Rust using Zed's GPU-accelerated UI framework + alacritty's terminal emulation library. Renders directly via Metal on macOS. No webview, no JavaScript, no IPC boundary.

| Position | Score | Confidence | Outcome |
|----------|-------|------------|---------|
| **GPUI + alacritty_terminal** | **3.85/5.0** | **High (85%)** | **Selected** |
| Swift/AppKit + alacritty_terminal | 3.65/5.0 | Medium (60%) | Credible fallback |
| Tauri 2 + xterm.js | 1.90/5.0 | High (95%) | Eliminated unanimously |

**Terminal fidelity (the decisive factor):**
The byte stream flows from PTY fd → alacritty_terminal parser → GPUI Metal renderer in a single process, single language, zero serialisation boundaries. This is the same architecture that makes Zed's terminal work.

**Why Tauri was eliminated (unanimous):**
Every byte of terminal output traverses Rust → JSON serialise → IPC bridge → JS deserialise → xterm.js DOM render. Tauri's own maintainers documented WKWebView performance issues on macOS and marked them "not planned." The IPC boundary is the exact seam where output gets dropped.

**Why Swift/AppKit was rejected (3-1):**
Native macOS widgets for free, but: (1) Swift-Rust FFI is a permanent maintenance tax, (2) fragments the language stack, (3) the researcher who raised it ultimately withdrew it herself.

### Weighted Scoring

| Criterion (Weight) | Tauri 2 | GPUI | Swift/AppKit |
|---|---|---|---|
| Feasibility (25%) | 2/5 | 3/5 | 4/5 |
| Terminal Fidelity (25%) | 1/5 | **5/5** | 4/5 |
| Long-term Maintainability (20%) | 2/5 | **4/5** | 3/5 |
| Rust Skill Development (15%) | 2/5 | **5/5** | 2/5 |
| Platform Integration (15%) | 3/5 | 2/5 | **5/5** |

### GPUI Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Pre-1.0, breaking API changes | Pin to specific commit hash. Rust compiler catches all breakage at build time. |
| Sparse documentation | Zed's codebase (200K+ lines) IS the documentation. |
| No tree view / context menu / drag-drop widgets | Ship terminal-first. GPUI Component library (v0.5.0, 60+ components) exists. |
| Zed could fail | GPUI is Apache-licensed Rust. Forkable. |

---

## System Architecture

```
┌──────────────────────────────────────────────────────┐
│                    Allele App                         │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │              UI Layer                            │ │
│  │  ┌──────────┐  ┌────────────────────────────┐   │ │
│  │  │ Sidebar  │  │ Terminal View(s)            │   │ │
│  │  │ (GPUI)   │  │ (TerminalGridElement +     │   │ │
│  │  │          │  │  alacritty_terminal)        │   │ │
│  │  └────┬─────┘  └────────────┬───────────────┘   │ │
│  └───────┼─────────────────────┼───────────────────┘ │
│          │                     │                      │
│  ┌───────┼─────────────────────┼───────────────────┐ │
│  │       │    Core Layer       │                    │ │
│  │  ┌────▼─────┐  ┌───────────▼──────────────┐     │ │
│  │  │ Session  │  │ PTY Manager              │     │ │
│  │  │ Registry │  │ (alacritty_terminal::tty) │     │ │
│  │  │ (state   │  │ - spawn claude CLI       │     │ │
│  │  │  .json)  │  │ - read/write PTY I/O     │     │ │
│  │  └────┬─────┘  │ - detect exit            │     │ │
│  │       │        └──────────────────────────┘     │ │
│  │  ┌────▼─────┐  ┌─────────────────────────┐     │ │
│  │  │ Status   │  │ Clone Manager           │     │ │
│  │  │ Detector │  │ (libc::clonefile FFI)   │     │ │
│  │  │ - hooks  │  │ - create COW clones     │     │ │
│  │  │ - pty    │  │ - auto-trust in claude  │     │ │
│  │  │ - pid    │  │ - trash + orphan sweep  │     │ │
│  │  └──────────┘  └─────────────────────────┘     │ │
│  │                                                  │ │
│  │  ┌──────────────────────────────────────────┐   │ │
│  │  │ Git Plumbing                              │   │ │
│  │  │ - session branches (allele/session/<id>)  │   │ │
│  │  │ - archive refs (refs/allele/archive/<id>) │   │ │
│  │  │ - merge-back pipeline + auto-commit       │   │ │
│  │  │ - periodic archive ref pruning            │   │ │
│  │  └──────────────────────────────────────────┘   │ │
│  │                                                  │ │
│  │  ┌──────────────────────────────────────────┐   │ │
│  │  │ Hook Infrastructure                       │   │ │
│  │  │ - receiver script (auto-installed)        │   │ │
│  │  │ - JSONL event files per session           │   │ │
│  │  │ - 250ms polling → status transitions      │   │ │
│  │  │ - sound/notification affordances          │   │ │
│  │  └──────────────────────────────────────────┘   │ │
│  └─────────────────────────────────────────────────┘ │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │              System Layer                        │ │
│  │  macOS: APFS clonefile() | PTY | Metal | afplay │ │
│  │  | git (subprocess)                              │ │
│  └─────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────┘
```

### Source Layout

```
src/
├── main.rs              # GPUI app bootstrap, window setup, sidebar rendering
├── terminal/
│   ├── mod.rs           # Module exports
│   ├── pty_terminal.rs  # PTY spawning via alacritty_terminal::tty + event loop
│   ├── terminal_view.rs # GPUI View — input handling, selection, search, resize
│   ├── grid_element.rs  # GPUI Element — cell grid renderer (see docs/grid-renderer.md)
│   └── keymap.rs        # Policy-based key-to-bytes translation (app actions + terminal input)
├── sidebar/
│   └── mod.rs           # Sidebar UI (rendered inline in main.rs currently)
├── clone/
│   └── mod.rs           # APFS clonefile() FFI, trash, orphan sweep
├── git/
│   └── mod.rs           # Git plumbing — session branches, archive refs, merge-back pipeline
├── hooks/
│   └── mod.rs           # Claude Code hook infrastructure, event polling, sounds
├── project/
│   └── mod.rs           # Project model (source path + sessions + archives)
├── session/
│   └── mod.rs           # Session model (UUID, status, timestamps, clone_path, drawer)
├── settings.rs          # User preferences (sidebar width, font, sound, drawer prefs)
├── state/
│   └── mod.rs           # JSON persistence (atomic write), session + archive rehydration
└── trust/
    └── mod.rs           # Auto-trust APFS clones in ~/.claude.json
```

---

## APFS Copy-on-Write Clone Management

### How It Works

Uses the macOS `clonefile(2)` syscall directly via `libc` FFI — near-instant, zero disk cost until files are modified.

**Key facts:**
- No root/entitlements required — works as regular user
- Single atomic syscall clones entire directory tree
- Near-instant regardless of file count (50K+ files in milliseconds)
- Zero additional disk space until files are modified
- Destination must NOT already exist (`EEXIST` error)
- Must be on same APFS volume (cross-volume fails with `EXDEV`)

### Clone Lifecycle

```
1. User adds project (repo path)
2. User clicks "New Session"
3. App calls clonefile() → instant COW clone at ~/.allele/workspaces/{project}/{session-id}/
4. App launches `claude --session-id <uuid>` inside the clone directory
5. User works in the session
6. When done: Close (suspend + keep clone for resume) or Discard (trash clone)
```

### Trash System

Clones are not deleted immediately. On close/discard they're moved to `~/.allele/trash/` with a timestamped name preserving project provenance. A 14-day TTL auto-purge runs on app startup. Orphan sweep catches any clones not referenced by a persisted session.

---

## Hook Infrastructure

Allele injects a `hooks.json` settings file at Claude spawn time via `claude --settings <path>`. This settings file declares hooks for 7 event types:

| Hook | Purpose |
|------|---------|
| Notification | → AwaitingInput (permission prompt, user must act) |
| Stop | → ResponseReady (Claude finished a turn) |
| UserPromptSubmit | Clears attention state (user is active) |
| SessionStart | Session lifecycle tracking |
| SessionEnd | Session lifecycle tracking |
| PreToolUse | Clears AwaitingInput (permission was granted) |
| PostToolUse | Belt-and-suspenders clearing signal |

Events flow: hook fires → shell receiver script → appends JSONL to `~/.allele/events/<session_id>.jsonl` → Allele polls every 250ms → status transition in sidebar.

### Attention Priority

`AwaitingInput` (⚠) outranks `ResponseReady` (★) — a permission prompt blocks everything, so it can never be stomped by a response completion. Only `UserPromptSubmit` clears attention state.

---

## Session Status Model

| Status | Icon | Colour | Meaning |
|--------|------|--------|---------|
| Running | ● | Green | Claude is actively working |
| Idle | ○ | Yellow | No PTY output for >30s, process alive |
| Done | ✓ | Grey | Process exited |
| Suspended | ⏸ | Blue | Rehydrated from disk, no PTY attached |
| AwaitingInput | ⚠ | Peach | Permission prompt — user must act |
| ResponseReady | ★ | Lavender | Claude finished, awaiting next prompt |

---

## Git Integration + Archive System

Allele manages git branches and refs to preserve session work across the clone lifecycle.

### Ref Namespace

| Ref | Location | Purpose |
|-----|----------|---------|
| `refs/heads/allele/session/<session-id>` | Clone's `.git/` | Session work branch, rooted at canonical's HEAD at clone time |
| `refs/allele/archive/<session-id>` | Canonical's `.git/` | Session work fetched back on discard, for later merge or deletion |

### Lifecycle

1. **Session create** → `git init` in clone + create session branch
2. **Session work** → Claude commits normally on the session branch
3. **Session close (suspend)** → PTY killed, clone kept, session branch preserved
4. **Session discard** → auto-commit any dirty state → `git fetch` session branch into canonical as archive ref → trash clone
5. **Archive merge** → user clicks "Merge" in archive browser → `git merge --no-ff` into canonical working tree
6. **Archive delete** → user clicks "Delete" → `git update-ref -d` removes the ref
7. **Archive pruning** → on startup, refs older than `TRASH_TTL_DAYS` (14d) are deleted

### Why Shell Out

Git is universally present on macOS dev workstations. Every operation is a cheap one-shot subprocess call. This avoids crate dependency bloat and follows the same pattern as shelling to `claude` and FFI'ing to `clonefile(2)`.

---

## Workspace Trust

Each APFS clone gets a fresh path, and Claude Code requires per-path trust acceptance. Allele auto-stamps `hasTrustDialogAccepted: true` in `~/.claude.json` at clone creation time so the trust prompt never appears.

The implementation uses read-modify-write via `serde_json::Value` (preserving unknown fields) with atomic replacement via tempfile + `rename(2)`.

---

## Keymap System

Terminal input is governed by a policy-based keymap (`src/terminal/keymap.rs`) that separates:

1. **App actions** — Cmd-key shortcuts handled by Allele (copy, paste, zoom, session switching, drawer toggle). Never reach the PTY.
2. **Terminal input** — bytes sent to the PTY, governed by:
   - A base sequence table (special keys → byte sequences)
   - Modifier policies (Option key as Meta = ESC prefix)
   - Readline override table (Option+Left → `ESC b` backward-word, etc.)

The Option key defaults to Meta mode (matching iTerm2's "Esc+" behaviour).

---

## References

All URLs verified on 2026-04-09:

- [GPUI / Zed](https://github.com/zed-industries/zed) — UI framework
- [alacritty_terminal](https://crates.io/crates/alacritty_terminal) — Terminal emulation
- [Termy](https://github.com/lassejlv/termy) — Reference GPUI + terminal integration
- [awesome-gpui](https://github.com/zed-industries/awesome-gpui) — Community GPUI projects
