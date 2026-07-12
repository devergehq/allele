# Changelog

All notable changes to Allele are documented here. This file holds **curated
highlights**; the full per-PR detail for each release lives in the
auto-generated notes on the [GitHub Releases page](https://github.com/devergehq/allele/releases).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Allele aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, the `0.MINOR.PATCH` line is used: `MINOR` bumps carry
features and possibly breaking changes, `PATCH` bumps are fixes only.

## [Unreleased]

Changes on `master` awaiting the next tagged release (**0.2.0**). This is the large
batch of work merged after the 0.1.0 baseline.

### Added
- Native "Lab Instrument" UI refresh: transparent unified titlebar, SF Pro chrome,
  semantic design-token theming, sidebar vibrancy, SVG icon language, and purposeful
  motion (breathing status dots, animated toggles, modal/drawer entrances).
- Drag-and-drop reordering for projects and sessions in the sidebar.
- Right-click context menus for project headers, and a per-session git dirty indicator.
- Settings: branch/remote text inputs and a per-session merge-strategy override.
- Agent UI capture bridge — `Allele --capture-ui` writes a PNG + machine-readable
  UI context without macOS Screen Recording permission.
- Word/line keyboard navigation in session text-input modals.
- Restore button to reactivate a discarded/archived session.
- Agent event-integration adapter layer (adds OpenCode alongside Claude Code).
- Governance & release infrastructure: automated versioned macOS release pipeline,
  this changelog, and `RELEASING.md`.

### Fixed
- Login-shell `PATH` now adopted under launchd's bare environment.
- Metal window capture routed through Core Graphics.
- Interactive session-naming modal no longer fires twice.
- A user-chosen branch is no longer auto-renamed.
- Session tracking now follows Claude's session-id rotation on `/clear`.
- Context-menu dismissal and vibrancy-hole resize-handle fixes.

### Changed
- Repository ownership moved to **Deverge Consulting Pty Ltd** (`devergehq/allele`);
  `LICENSE`, `NOTICE`, and `CLA.md` updated accordingly.

## [0.1.0] - 2026-07-02

Initial pre-alpha baseline — the state of the project at PR #34, before the large
merge batch. Core proof-of-concept complete and runnable.

### Added
- GPUI (Metal) window with sidebar + main terminal area.
- Real PTY-backed terminal rendering via `alacritty_terminal` with a cell-accurate
  grid renderer (256-colour + truecolor, cursor shapes, selection, in-terminal search,
  URL detection, 10k-line scrollback).
- Embedded, unmodified `claude` CLI running in a real PTY with full output fidelity.
- Multi-session management with sidebar switching (Cmd+1-9) and 6 status states.
- Session persistence and cold resume across restarts via `claude --resume`.
- APFS `clonefile(2)` copy-on-write workspaces with auto-trust in `~/.claude.json`,
  trash system with 14-day TTL, and orphan sweep.
- Claude Code hook integration for attention routing.
- Git plumbing for session branches, auto-commit, archive refs, and an archive
  browser with merge/delete actions.
- Per-session drawer terminal panel and auto-naming of sessions from the first prompt.

[Unreleased]: https://github.com/devergehq/allele/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/devergehq/allele/releases/tag/v0.1.0
