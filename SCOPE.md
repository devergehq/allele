# Claude Hive — Claude Code Session Manager

**Status:** Technical Scope Document
**Date:** 2026-04-09
**Author:** Patrick Dorival + PAI

---

## 1. Problem Statement

Running 10-15 Claude Code sessions across scattered terminal windows/tabs with no way to:
- Tell which sessions are running vs finished
- Separate Claude Code terminals from regular work terminals
- Switch between sessions without hunting through tabs
- Manage workspace isolation (clones) for parallel work

## 2. Solution Summary

A native macOS desktop app that is **a terminal multiplexer with a GUI sidebar and APFS clone management**. It does NOT wrap, intercept, or reinterpret Claude Code's output. It embeds the real Claude Code CLI in real PTY-backed terminal views.

Seven irreducible components:

| # | Component | Purpose |
|---|-----------|---------|
| 1 | Process Spawner | Launch `claude` CLI in a PTY subprocess |
| 2 | Terminal Renderer | Display PTY output faithfully (ANSI, cursor, interactive prompts) |
| 3 | Session Registry | Track {project, workspace, session, status} as persistent state |
| 4 | View Switcher | Swap which terminal is visible when clicking sidebar items |
| 5 | Status Detector | Distinguish running/idle/finished sessions |
| 6 | APFS Cloner | Create copy-on-write clones of repos for workspace isolation |
| 7 | Window Frame | Host sidebar + terminal area |

## 3. Technology Decision: GPUI + alacritty_terminal

**Decision via 4-member council debate (Architect, Designer, Engineer, Researcher) — 3 rounds + weighted analysis. Full transcript saved to research history.**

### The Decision: GPUI (Score: 3.85/5.0)

**Architecture:** Pure Rust using Zed's GPU-accelerated UI framework + alacritty's terminal emulation library. Renders directly via Metal on macOS. No webview, no JavaScript, no IPC boundary.

| Position | Score | Confidence | Outcome |
|----------|-------|------------|---------|
| **GPUI + alacritty_terminal** | **3.85/5.0** | **High (85%)** | **Selected** |
| Swift/AppKit + alacritty_terminal | 3.65/5.0 | Medium (60%) | Credible fallback |
| Tauri 2 + xterm.js | 1.90/5.0 | High (95%) | Eliminated unanimously |

### Why GPUI Wins

**Terminal fidelity (the decisive factor):**
The byte stream flows from PTY fd → alacritty_terminal parser → GPUI Metal renderer in a single process, single language, zero serialisation boundaries. This is the same architecture that makes Zed's terminal work. It directly addresses the core pain: tools that lose or hide terminal output.

**Why Tauri was eliminated (unanimous):**
Every byte of terminal output traverses Rust → JSON serialise → IPC bridge → JS deserialise → xterm.js DOM render. This is a structural defect for terminal-centric apps. Tauri's own maintainers documented WKWebView performance issues on macOS and marked them "not planned." The IPC boundary is the exact seam where output gets dropped.

**Why Swift/AppKit was considered but rejected (3-1):**
A third option emerged during debate: alacritty_terminal in Rust + Swift/AppKit shell. Native macOS widgets (NSOutlineView, NSMenu) for free. The Designer advocated for this. Rejected because: (1) Swift-Rust FFI boundary is a permanent maintenance tax, (2) fragments the language stack — Rust becomes a thin backend, contradicting the Rust learning goal, (3) the researcher who raised it ultimately withdrew it herself.

### Weighted Scoring (speed-to-market excluded)

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
| Pre-1.0, breaking API changes | Pin to specific commit hash in Cargo.toml. Update on YOUR schedule. Rust compiler catches all breakage at build time. |
| Sparse documentation | Zed's codebase (200K+ lines) IS the documentation. Reading production Rust builds deeper expertise than reading framework docs. |
| No tree view / context menu / drag-drop widgets | Ship terminal-first with minimal sidebar. Build UI chrome incrementally. GPUI Component library (v0.5.0, 60+ components) exists. |
| Zed could fail (~$10M runway) | GPUI is Apache-licensed Rust. Forkable. Does not die with the company. |
| First breakage costs a week, not a day | Bounded cost that decreases over time. Compare: Tauri's IPC ceiling is permanent and unfixable. |

### Fallback Strategy

If GPUI proves unworkable within the first two weeks of prototyping, pivot to Swift/AppKit + alacritty_terminal. The Rust core (PTY management, APFS cloning, session state) ports directly — only the UI layer changes.

### Key Crates

- `gpui` (v0.2.2) — UI framework (pin to specific commit)
- `alacritty_terminal (custom GPUI rendering)` — drop-in terminal widget (uses alacritty_terminal internally)
- `alacritty_terminal` (v0.25.1) — VTE parsing and terminal state management
- `portable-pty` — PTY subprocess management
- `serde` / `serde_json` — session state serialisation
- `uuid` — identifier generation
- `clonetree` — APFS COW directory cloning
- `notify` — file watcher for hook events
- `tokio` — async runtime

### Build Toolchain

```
cargo only — no npm, no Xcode, no second build system
macOS 14+ SDK (Metal required for GPUI)
```

## 4. APFS Copy-on-Write Clone Management

### Creating Clones

```rust
// Using clonetree crate
use clonetree::{clone_tree, CloneOptions};

let options = CloneOptions::default();
clone_tree("/path/to/original/repo", "/path/to/workspaces/repo-clone-1", &options)?;

// Or direct FFI to clonefile(2)
extern "C" {
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> c_int;
}
```

**Key facts:**
- No root/entitlements required — works as regular user
- Single atomic syscall clones entire directory tree
- Near-instant regardless of file count (50K+ files in milliseconds)
- Zero additional disk space until files are modified
- Destination must NOT already exist (`EEXIST` error)
- Must be on same APFS volume (cross-volume fails with `EXDEV`)
- Symlinks preserved, extended attributes copied, ACLs copied

### Clone Lifecycle

```
1. User adds project (repo path)
2. User clicks "New Workspace"
3. App calls clonefile() → instant COW clone at ~/.claude-hive/workspaces/{project}/{workspace-name}/
4. App launches `claude` inside the clone directory
5. User works in the session
6. When done: user can merge changes back (git diff + apply) or discard (rm -rf clone)
```

### Cleanup Strategy

Clones that have been merged or abandoned should be deletable from the sidebar. Since COW clones are regular directories, deletion is just `rm -rf`. Disk space is reclaimed only for blocks unique to that clone.

## 5. Session Status Detection

Three approaches, in order of reliability:

### Approach 1: Claude Code Hooks (Best)

Claude Code supports hooks that fire on events. Configure a `postToolUse` or session-end hook that writes status to a known location:

```jsonc
// ~/.claude/settings.json
{
  "hooks": {
    "postToolUse": [
      {
        "command": "echo '{\"session_id\": \"$SESSION_ID\", \"status\": \"tool_used\", \"timestamp\": \"$(date -Iseconds)\"}' >> /tmp/claude-hive-events.jsonl"
      }
    ]
  }
}
```

The app watches `/tmp/claude-hive-events.jsonl` (or a Unix socket) for status updates.

### Approach 2: PTY Activity Monitoring

Monitor the PTY file descriptor for I/O activity:
- **Running:** PTY output in last N seconds
- **Idle:** No PTY output for >30 seconds, process still alive
- **Finished:** Process exited (SIGCHLD or waitpid)

This is reliable for running/finished but "idle" vs "waiting for user input" is ambiguous.

### Approach 3: Process State

Check if the `claude` subprocess is still alive:
- `waitpid(pid, WNOHANG)` — non-blocking check for process exit
- Exit code 0 = completed normally
- Exit code non-zero = error/interrupted

**Recommended:** Combine all three. Process state for definitive running/finished. PTY activity for idle detection. Hooks for richer status if available.

## 6. Session State Persistence

Simple JSON file at `~/.claude-hive/state.json`:

```json
{
  "projects": [
    {
      "id": "uuid",
      "name": "my-project",
      "path": "/Users/patrick/Sites/my-project",
      "workspaces": [
        {
          "id": "uuid",
          "name": "feature-auth",
          "clone_path": "/Users/patrick/.claude-hive/workspaces/my-project/feature-auth",
          "created_at": "2026-04-09T12:00:00+10:00",
          "sessions": [
            {
              "id": "uuid",
              "status": "running",
              "started_at": "2026-04-09T12:30:00+10:00",
              "pid": 12345,
              "claude_args": ["--model", "opus"]
            }
          ]
        }
      ]
    }
  ]
}
```

## 7. UI Layout Specification

```
┌─────────────────────────────────────────────────────────────┐
│  Claude Hive                                    [−] [□] [×] │
├──────────────┬──────────────────────────────────────────────┤
│              │                                              │
│  PROJECTS    │  Terminal View                               │
│              │                                              │
│  ▶ my-proj   │  $ claude                                    │
│    ├ ws-1  ● │  ╭─────────────────────────────────────╮     │
│    ├ ws-2  ○ │  │ I'll help you with that. Let me     │     │
│    └ ws-3  ✓ │  │ read the file first...              │     │
│              │  ╰─────────────────────────────────────╯     │
│  ▶ api-svc   │                                              │
│    └ ws-1  ● │  > Reading src/main.rs                       │
│              │                                              │
│  ▶ frontend  │                                              │
│    ├ ws-1  ✓ │                                              │
│    └ ws-2  ● │                                              │
│              │                                              │
│  ──────────  │                                              │
│  [+ Project] │                                              │
│              │                                              │
│  ● Running 3 │                                              │
│  ○ Idle    1 │                                              │
│  ✓ Done    2 │                                              │
│              │                                              │
├──────────────┴──────────────────────────────────────────────┤
│  ws-1 @ my-proj | ● Running | 12m elapsed                  │
└─────────────────────────────────────────────────────────────┘
```

**Sidebar (left, ~200-250px):**
- Collapsible project tree
- Status icon per workspace/session (●/○/✓)
- "Add Project" button at bottom
- Summary counts at bottom

**Main area (right, remaining space):**
- Full terminal view of the selected session
- Status bar at bottom showing current session info

**Interactions:**
- Click workspace → switch terminal view
- Right-click workspace → New Session, Delete Workspace, Open in Finder
- Right-click project → New Workspace, Remove Project
- Double-click workspace name → rename

## 8. Build Toolchain & Dependencies

```
All Rust:
  - gpui = "0.2" (pinned to commit hash)
  - alacritty_terminal (custom GPUI rendering)
  - gpui-component (for sidebar widgets as needed)
  - alacritty_terminal
  - portable-pty
  - serde + serde_json
  - uuid
  - clonetree
  - notify
  - tokio

Build:
  - cargo only — single build system, single language
  - macOS 14+ SDK (Metal required for GPUI)
```

## 9. Architecture Diagram

```
┌──────────────────────────────────────────────────────┐
│                    Claude Hive App                     │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │              UI Layer                            │ │
│  │  ┌──────────┐  ┌────────────────────────────┐   │ │
│  │  │ Sidebar  │  │ Terminal View(s)            │   │ │
│  │  │ (GPUI    │  │ (alacritty_terminal (custom GPUI rendering) +            │   │ │
│  │  │  list)   │  │  alacritty_terminal)        │   │ │
│  │  └────┬─────┘  └────────────┬───────────────┘   │ │
│  └───────┼─────────────────────┼───────────────────┘ │
│          │                     │                      │
│  ┌───────┼─────────────────────┼───────────────────┐ │
│  │       │    Core Layer       │                    │ │
│  │  ┌────▼─────┐  ┌───────────▼──────────────┐     │ │
│  │  │ Session  │  │ PTY Manager              │     │ │
│  │  │ Registry │  │ (portable-pty)           │     │ │
│  │  │ (state   │  │ - spawn claude CLI       │     │ │
│  │  │  .json)  │  │ - read/write PTY I/O     │     │ │
│  │  └────┬─────┘  │ - detect exit            │     │ │
│  │       │        └──────────────────────────┘     │ │
│  │  ┌────▼─────┐  ┌─────────────────────────┐     │ │
│  │  │ Status   │  │ Clone Manager           │     │ │
│  │  │ Detector │  │ (clonetree/clonefile)   │     │ │
│  │  │ - hooks  │  │ - create COW clones     │     │ │
│  │  │ - pty    │  │ - delete clones         │     │ │
│  │  │ - pid    │  │ - list workspaces       │     │ │
│  │  └──────────┘  └─────────────────────────┘     │ │
│  └─────────────────────────────────────────────────┘ │
│                                                       │
│  ┌─────────────────────────────────────────────────┐ │
│  │              System Layer                        │ │
│  │  macOS: APFS clonefile() | PTY | Notifications  │ │
│  └─────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────┘
```

## 10. MVP Feature Boundary

### MVP (v0.1) — Ship This First

- [ ] Add/remove projects (repo paths)
- [ ] Create/delete workspaces (APFS COW clones)
- [ ] Launch Claude Code session in a workspace
- [ ] Embedded terminal showing Claude Code CLI unmodified
- [ ] Switch between sessions via sidebar click
- [ ] Status indicators (running ●, idle ○, done ✓)
- [ ] Session persists across app restart (reconnect to running PTY or show last output)
- [ ] Status bar showing current session info

### v0.2 — Quality of Life

- [ ] macOS system notification on session completion
- [ ] Session elapsed time display
- [ ] Keyboard shortcuts for session switching (Cmd+1-9, Cmd+↑/↓)
- [ ] Drag-and-drop to reorder projects
- [ ] Search/filter sessions
- [ ] Session rename

### v0.3+ — Future (only if needed)

- [ ] Split terminal views (side-by-side sessions)
- [ ] Git status indicator per workspace
- [ ] File change summary per workspace
- [ ] Session templates (pre-configured claude args)
- [ ] Cross-platform (Linux, Windows)
- [ ] Import/export project list

## 11. Complexity Estimate

### GPUI + alacritty_terminal

| Component | Effort | Notes |
|-----------|--------|-------|
| GPUI learning curve | 3-5 days | Reading Zed source, alacritty_terminal (custom GPUI rendering) examples, Termy/ZTerm as references |
| Project scaffolding | 1-2 days | GPUI app setup, Metal dependencies, pin commit hash |
| Terminal rendering (alacritty_terminal (custom GPUI rendering)) | 2-3 days | Drop-in crate, but integration and PTY wiring needed |
| PTY management | 2-3 days | portable-pty, async I/O with tokio |
| APFS clone management | 1-2 days | clonetree crate + create/delete UI |
| Session state persistence | 1 day | JSON file read/write with serde |
| Status detection | 2-3 days | PTY monitoring + process state + hooks |
| Sidebar UI (minimal — terminal-first) | 3-5 days | Simple list view, status icons, click-to-switch. No tree view or drag-drop in v1. |
| View switching | 2-3 days | GPUI view management, terminal multiplexing |
| Polish + testing | 5-7 days | Edge cases, error handling, GPUI quirks |
| **Total MVP** | **~5-8 weeks** | Part-time, single developer |

**Note:** This is not a speed-to-market estimate. It's a realistic assessment. The GPUI learning curve is front-loaded — once you're reading Zed source fluently, iteration speed increases significantly.

## 12. Verified References

All URLs verified (HTTP 200) on 2026-04-09:

**Tauri terminal ecosystem:**
- [tauri-plugin-pty](https://github.com/Tnze/tauri-plugin-pty) — Tauri 2 PTY plugin
- [terraphim-liquid-glass-terminal](https://github.com/terraphim/terraphim-liquid-glass-terminal) — Production Tauri terminal

**GPUI ecosystem:**
- [alacritty_terminal (custom GPUI rendering)](https://github.com/zortax/alacritty_terminal (custom GPUI rendering)) — Drop-in terminal widget for GPUI
- [awesome-gpui](https://github.com/zed-industries/awesome-gpui) — Community GPUI projects

**Terminal emulation:**
- [iced_term](https://github.com/Harzu/iced_term) — Terminal widget for Iced (reference)
- [alacritty_terminal](https://crates.io/crates/alacritty_terminal) — Standard Rust terminal emulation library

**APFS cloning:**
- [clonetree](https://github.com/cortesi/clonetree) — Rust APFS directory cloning

**PTY management:**
- [portable-pty](https://lib.rs/crates/portable-pty) — Cross-platform PTY crate
