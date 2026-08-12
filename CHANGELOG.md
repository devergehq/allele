# Changelog

All notable changes to Allele are documented here. This file holds **curated
highlights**; the full per-PR detail for each release lives in the
auto-generated notes on the [GitHub Releases page](https://github.com/devergehq/allele/releases).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Allele aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, the `0.MINOR.PATCH` line is used: `MINOR` bumps carry
features and possibly breaking changes, `PATCH` bumps are fixes only.

## [Unreleased]

Changes on `master` awaiting the next tagged release.

### Added
- "Skip startup procedures" on the New Session dialog — creates a session with its own
  workspace clone and branch but runs none of the project's orchestration: no startup
  command, no drawer terminals, no preview, and no shutdown command when it closes. For
  asking a question or doing a short investigation in a project without spinning up its
  dev servers. The choice persists across suspend/resume and can be changed later from
  Edit Session.
- Sidebar "active only" view (⌘⇧E, or the funnel button in the sidebar header):
  hides sessions with no live agent — suspended and ended — along with any project
  left with nothing to show, so a tree of dozens of parked sessions collapses to
  just the work in flight. Nothing is deleted; toggling back restores the full tree.
- "Delete all" on the sidebar ARCHIVES header — clears every archived session in a
  project in one confirmed step, instead of one ✕-and-confirm per row. Hover-revealed,
  and gated by the same inline prompt as a single delete.
- The transcript now surfaces session events it previously ignored, as compact
  one-line notices: a pull request opened, an artifact published, a file edited
  outside Claude, a plan accepted, a queued message, and hook errors.

### Fixed
- Archive delete confirmations no longer survive a project reorder or removal. An armed
  prompt carries a project index, so it could reappear against a different project's
  archives once the indices shifted underneath it.
- Naming an existing branch in "New session with details" no longer silently starts
  the session on a stale copy of it. Because a workspace is an APFS clone of the
  source repo, every branch you have ever checked out there already exists locally,
  so the old local-first lookup always won and the remote was never consulted —
  which meant long-lived rebased or PR-stacked branches quietly began from an
  out-of-date base. Branch resolution is now remote-first: it always fetches, then
  moves the workspace's copy of the branch onto the remote tip. The `origin/` prefix
  is now optional (`origin/feature/x` and `feature/x` are the same request), and the
  remote is taken from the project's configured remote rather than assuming `origin`.
  If the branch can't be confirmed up to date — unreachable remote, or uncommitted
  changes in the source repo blocking the checkout — the session is refused with an
  explanation instead of quietly starting from the wrong base.
- The transcript no longer fills with `⚠ unsupported event` JSON dumps. Claude Code
  writes a lot of session bookkeeping — titles, modes, agent names, file-history
  backups, context plumbing — interleaved with the conversation, and every line the
  parser didn't model was rendered raw. On a real corpus that was roughly 40% of the
  transcript. Those types are now modelled and skipped, and any *future* event type
  Claude Code introduces is recorded without being rendered, so the reading view
  degrades quietly instead of filling with JSON. Nothing is lost: the session ledger
  still retains every original line verbatim.

### Fixed
- New sessions no longer fail with `Failed to create PTY: Too many open files
  (os error 24)` once a project has many sessions open. macOS gives apps launched
  from Finder/Dock a soft file-descriptor limit of 256, which a few dozen sessions'
  worth of PTYs exhausts; Allele now raises its own limit at startup before
  anything spawns.

## [0.2.0] - 2026-07-25

The large batch of work merged after the 0.1.0 baseline.

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
- Workspace dirty-status poller could write a `git status` result onto the wrong
  session when the session list changed while the check was in flight.
- Session header reported `0 changed` on a dirty workspace until the changes panel
  was opened, and its next-action label looked like a button but did nothing.
- Sidebar discard/delete confirmations overflowed the sidebar, putting their
  buttons out of reach.
- Settings window opened too small, and the Infrastructure section squashed the
  section list and search box; settings search now matches keywords, not just
  section labels.

### Changed
- Repository ownership moved to **Deverge Consulting Pty Ltd** (`devergehq/allele`);
  `LICENSE`, `NOTICE`, and `CLA.md` updated accordingly.

### Removed
- `ROADMAP.md` — the historical phase log is retired. The roadmap now lives in Linear
  (Deverge / DEV); a published, always-current roadmap page is planned.

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

[Unreleased]: https://github.com/devergehq/allele/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/devergehq/allele/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/devergehq/allele/releases/tag/v0.1.0
