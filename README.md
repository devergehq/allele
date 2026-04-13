# Allele

> A native macOS session manager for Claude Code, with APFS copy-on-write workspace isolation.

## About the name

**Allele** (pronounced "uh-LEEL") is a biology term for a variant of a gene at a shared locus. That's literally the model here: each Claude Code session is a variant of the same project at a shared locus (the trunk repo), running in its own APFS copy-on-write fork. You run N variants in parallel, let them diverge, then select the winner and cull the rest. The metaphor isn't decoration — *locus*, *variant*, *select*, and *cull* are the actual operations the tool performs on your workspaces.

**Status:** Pre-alpha. Built primarily for the maintainer and a small group of friends who run a lot of parallel Claude Code sessions. If you're looking for a polished, supported product, this isn't it (yet).

---

## What it is

Allele is a GPU-accelerated native macOS app that manages multiple Claude Code sessions side-by-side. It does three things that aren't well-served by existing tools:

1. **Embedded real terminals.** Each session runs the real `claude` CLI in a real PTY, rendered via GPUI + alacritty_terminal. No output interception, no JSON wrapping, no IPC boundary — what you see is exactly what Claude Code printed. Full 256-colour + truecolor support, cursor shapes, text selection, in-terminal search, and a cell-accurate grid renderer.
2. **APFS copy-on-write workspaces.** Every session runs in an instant copy-on-write clone of your repo, created via macOS's `clonefile(2)` syscall. No waiting for `cp -r` on a 50k-file monorepo. No disk space used until files are modified. Clones are auto-trusted in Claude Code's `~/.claude.json` so there's no trust prompt on first entry.
3. **Session lifecycle management.** A sidebar tracks running, idle, awaiting-input, and finished sessions. Sessions persist across app restarts (cold resume via `claude --resume`). Discarded sessions are archived into canonical git refs so work is never lost. An archive browser lets you merge or delete archived sessions.

## What it isn't

- **Not cross-platform.** macOS only. The APFS copy-on-write feature is core to the value proposition and is Apple-specific.
- **Not a Claude Code wrapper.** It does not intercept, reinterpret, or reformat Claude Code's output. It embeds the unmodified CLI.
- **Not a general-purpose terminal emulator.** If you want iTerm2, Ghostty, or WezTerm, use those. Allele is narrowly focused on managing Claude Code sessions.
- **Not commercial software.** Apache 2.0 licensed, free forever. See [Licensing](#licensing) for nuance on contributor terms.

## Who it's for

People who:

- Run 5+ parallel Claude Code sessions regularly.
- Work on macOS.
- Want each session in its own isolated workspace without waiting for slow clones.
- Have lost track of which terminal tab has which session at least once.

If that's not you, you're probably better served by tmux, iTerm2 split panes, or just your existing terminal.

## Features

### Terminal
- Cell-accurate grid renderer with JetBrains Mono font
- 256-colour + truecolor ANSI rendering (Catppuccin Mocha palette)
- 5 cursor shapes (block, beam, underline, hollow block, hidden) with blink
- Text selection (mouse drag) and clipboard copy (Cmd+C)
- In-terminal search (Cmd+F, Cmd+G / Cmd+Shift+G)
- URL detection with hover underline and click-to-open
- 10,000-line scrollback with trackpad momentum accumulation
- Scrollbar with fade animation
- Font size adjustment (Cmd+/-, Cmd+0 reset)
- Policy-based keymap with readline-friendly Option+key (Meta mode)
- Per-session drawer terminal panel (Cmd+J) for running shell commands alongside Claude

### Sessions
- 6 status states: Running ●, Idle ○, Done ✓, Suspended ⏸, AwaitingInput ⚠, ResponseReady ★
- Keyboard shortcuts: Cmd+1-9 (switch), Cmd+N (new), Cmd+W (close), Cmd+[/] (prev/next)
- Cold resume across app restarts via `claude --resume`
- Auto-naming from first prompt via LLM summarisation
- Sound alerts on attention events (configurable via settings.json)
- macOS notifications on session completion (opt-in)

### Workspaces
- Instant APFS `clonefile(2)` COW clones — zero disk cost until modified
- Auto-trust in `~/.claude.json` at clone creation
- Trash system with 14-day TTL auto-purge (no instant deletion)
- Orphan sweep for leaked clones on startup
- Dirty-state confirmation before cloning

### Git Integration
- Session branches in clones (`allele/session/<id>`)
- Auto-commit dirty state before archiving
- Archive refs in canonical (`refs/allele/archive/<id>`)
- Archive browser with merge/delete actions
- Periodic archive ref pruning matching trash TTL

### Projects
- Add/remove projects via sidebar or folder picker
- Project relocation when source path moves
- Per-project session list with expand/collapse

## Architecture (short version)

- **Language:** Rust (edition 2021).
- **UI:** [GPUI](https://github.com/zed-industries/zed) — Zed's GPU-accelerated Rust UI framework. Renders directly via Metal. Pinned to a specific commit hash in `Cargo.toml`.
- **Terminal emulation:** [alacritty_terminal](https://crates.io/crates/alacritty_terminal) — the same VTE parser and terminal state machine that powers Alacritty.
- **PTY:** `alacritty_terminal::tty` (uses `rustix-openpty` internally) for subprocess PTY management.
- **Workspace cloning:** Direct FFI to `clonefile(2)` via `libc`.
- **Git operations:** Subprocess calls to `git` (no libgit2 dependency).
- **Persistence:** JSON state file at `~/.allele/state.json` (serde, atomic writes).
- **Settings:** `~/.config/allele/settings.json` for user preferences.
- **Async runtime:** tokio.

See `docs/architecture.md` for the full technical deep-dive and `ROADMAP.md` for the phased build plan.

## Building from source

Requirements:

- macOS 14 or later (Metal is required by GPUI).
- Rust toolchain (stable) — install via [rustup.rs](https://rustup.rs).
- Xcode Command Line Tools: `xcode-select --install`.
- Claude Code CLI installed and on your `PATH`.
- `git` available (checked at startup).

Build and run:

```sh
git clone https://github.com/devergehq/allele.git
cd allele
cargo build --release
./target/release/allele
```

First build is slow (~5-10 minutes) because GPUI and alacritty_terminal are large crates. Incremental builds are fast.

## Distribution

Currently source-only. Pre-built binaries are not provided, and the project is not signed or notarised. If you build it yourself, macOS Gatekeeper will treat it as an unsigned local binary (which is fine for local use). If you receive a build from someone else, you may need to clear the quarantine attribute:

```sh
xattr -d com.apple.quarantine ./allele
```

A macOS `.app` bundle target exists for clipboard history app compatibility — see the build output.

## Project status

All core phases complete. Working:

- GPUI window with sidebar + main terminal area
- Real PTY-backed terminal rendering via cell grid Element
- Claude Code CLI running embedded with full output fidelity
- Multi-session management with sidebar switching (Cmd+1-9)
- Session persistence and cold resume across app restarts
- APFS clone-backed workspace isolation with auto-trust
- Claude Code hook integration for attention routing (6 status states)
- Git plumbing for session archiving and merge-back
- Archive browser UI with merge/delete actions
- Policy-based terminal keymap with readline support
- Per-session drawer terminal panel
- Auto-naming sessions from first prompt

See `ROADMAP.md` for the full phase breakdown and remaining work.

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide. Short version:

1. Open an issue first for anything non-trivial, to check the change aligns with the project's direction.
2. Your first PR will trigger a prompt from the CLA Assistant bot. Sign the [CLA](CLA.md) (one click) and the PR is unblocked.
3. Build, test, submit.

Response times are side-project pace (days to weeks). If you need faster, please fork.

## Licensing

Allele is licensed under the **[Apache License 2.0](LICENSE)**. You are free to use, modify, distribute, and (if you wish) commercialise it, subject to the terms of that licence.

Contributors grant an additional broad licence via the [CLA](CLA.md) that preserves the maintainer's right to dual-licence the project in the future. Contributors retain copyright to their contributions and can continue to use them in other projects under any licence they choose.

The maintainer currently has no plans to dual-licence or monetise Allele. The CLA exists solely to keep that option open if circumstances change.

## Acknowledgements

- **Zed team** for open-sourcing GPUI and building a Rust UI framework worth betting on.
- **Alacritty team** for the terminal emulation library that does all the hard VTE parsing work.
- **Termy** ([github.com/lassejlv/termy](https://github.com/lassejlv/termy)) as a reference GPUI + alacritty_terminal integration.
- **Anthropic** for building Claude Code, which is the entire reason this tool exists.
