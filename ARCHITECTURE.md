# Allele architecture

_Audience: future contributors and AI agents editing this codebase._

This document captures **why** the code is shaped the way it is. When you
read the source and something seems odd, check here first — most
apparent oddities are deliberate responses to a real constraint.

---

## 1. The problem Allele solves

Allele is a native macOS desktop app for running multiple **Claude Code
sessions** in parallel, each in its own **copy-on-write clone** of a
project directory. A user can spawn five concurrent variants of the
same task without conflicting working trees.

The core primitives:

- **APFS `clonefile(2)`** — near-instant, zero-disk-cost snapshots of a
  directory. One project × N sessions means N clones; thanks to COW, N
  × 0 disk cost until files are modified.
- **PTYs** — each session runs `claude --session-id <uuid>` (or a shell)
  in its own pseudoterminal, rendered inside a GPUI terminal widget.
- **Git archive refs** — when a session closes, its branch is archived
  under `refs/allele/archive/<id>` on canonical so work is never lost
  and can be merged back.
- **Hook events** — Claude Code's hook system writes JSONL files to
  `~/.allele/events/`. Allele polls them to update session status
  (Running / AwaitingInput / ResponseReady / Done).

Everything else in the codebase exists to make those four things
ergonomic.

---

## 2. Module layout

```
src/
├── main.rs                 # entry point, AppState constructor, Render impl,
│                           # panic hook, macOS app menu, About panel
├── app_state.rs            # AppState struct + 5 cohesive sub-structs
├── actions.rs              # PendingAction + 8 family enums (command pattern)
├── errors.rs               # AlleleError + Result<T> alias
├── repositories.rs         # SettingsRepository + StateRepository traits
│
├── platform/               # OS-abstraction layer
│   ├── mod.rs              # 3 traits + Platform bundle + global() accessor
│   ├── apple.rs            # macOS impls (clonefile, AppleScript, open/afplay)
│   └── unsupported.rs      # Linux/Windows fallbacks
│
├── pending_actions.rs      # dispatch_pending_action + 8 family handlers
├── session_ops.rs          # add/resume/close/remove/navigate session + config
├── hook_events.rs          # apply_hook_event + trigger_auto_naming
├── editor.rs               # Editor tab (file tree, preview, context menu)
├── sidebar/render.rs       # build_sidebar_items (project tree + archives)
├── drawer/mod.rs           # render_drawer + drawer tab helpers
│
├── agents/                 # AgentAdapter trait + Claude/Opencode/Generic impls
├── browser/                # macOS-only Chrome AppleScript driver
├── clone/                  # Workspace clone + trash + orphan sweep
├── config/                 # allele.json (per-project config) parser
├── git/                    # git subprocess wrapper (pull, merge, archive)
├── hooks/                  # Hook receiver install + event watcher + dialogs
├── project/                # Project struct (source path, sessions, archives)
├── scratch_pad/            # Compose overlay entity + clipboard image helper
├── session/                # Session + DrawerTab + status machine
├── settings/               # Settings data model + JSON IO
├── settings_window.rs      # GPUI Settings window entity
├── state/                  # PersistedState (state.json) + archived sessions
├── terminal/               # PTY + GPUI terminal grid element
├── text_input/             # Reusable focus-aware text input
└── trust/                  # Claude Code trust file updater
```

---

## 3. Patterns — what they are, why they're here

Six patterns carry most of the design weight. They aren't gratuitous;
each one solves a concrete problem that biting us pre-refactor.

### 3.1 Command pattern — `PendingAction`

**Problem**: GPUI mouse/keyboard listeners borrow the render tree while
they run. A listener cannot mutate `AppState` directly because the
tree is immutable during paint. Inline mutation leads to
"ArenaRef-after-arena-cleared" crashes (documented in
`main.rs` pending-action dispatch comments).

**Solution**: listeners enqueue `PendingAction` values into
`AppState.pending_action`. The next render tick drains the queue
*before* building the tree.

**Type shape** (see `actions.rs`):

```rust
enum PendingAction {
    Session(SessionAction),
    Archive(ArchiveAction),
    Drawer(DrawerAction),
    Sidebar(SidebarAction),
    Project(ProjectAction),
    Settings(SettingsAction),
    Browser(BrowserAction),
    Overlay(OverlayAction),
}
```

Each family enum holds its own variants. `From<FamilyAction>` impls
let call sites write `SessionAction::CloseActive.into()` rather than
`PendingAction::Session(SessionAction::CloseActive)`.

**Dispatch** (see `pending_actions.rs`): `dispatch_pending_action` is
an 8-arm top-level match that delegates to `handle_<family>_action`
methods. Each handler only knows its own sub-enum.

**Extending it**: to add a new action,
1. add a variant to the appropriate family enum in `actions.rs`
2. add a match arm in the matching `handle_*_action` method in
   `pending_actions.rs`
3. call sites use `FamilyAction::NewVariant.into()`

Do not add a new family unless the action genuinely doesn't belong in
any existing one — the family list is stable by design.

### 3.2 Adapter pattern — `platform::*`

**Problem**: Allele is deeply macOS-coupled (APFS clonefile, AppleScript
for Chrome, `open(1)`, `afplay`, Cocoa dialogs). Scattering
`#[cfg(target_os = "macos")]` through business logic made porting
infeasible.

**Solution**: three traits in `platform/mod.rs` carve the OS boundary:

- **`CloneBackend`** — copy-on-write cloning. `clone(src, dst)`,
  `supports_cow()`, `name()`. macOS uses `libc::clonefile`; non-macOS
  fallback does a recursive `std::fs::copy`.
- **`BrowserIntegration`** — Chrome tab control. `is_running`,
  `create_tab`, `activate_tab`, `navigate_tab`, `close_tab`. macOS
  drives Chrome via AppleScript. Non-macOS returns `false` / `None`
  everywhere — callers fall back to `SystemShell::open_url`.
- **`SystemShell`** — miscellaneous OS primitives: `open_url`,
  `reveal_in_files`, `play_sound`, `show_fatal_dialog`. macOS wraps
  `open`, `afplay`, `NSAlert`. Non-macOS uses `xdg-open`/`explorer`
  with logged stubs for sound/dialog.

**Bundle**: `Platform` holds all three behind `Arc<dyn Trait>` so
background tasks get cheap clones.

**Selection**: `Platform::detect()` picks per `#[cfg(target_os)]` at
startup, exactly once. Stored on `AppState` AND in a process-wide
`OnceLock` accessor `platform::global()` so leaf components like
`TerminalView` (deeply nested GPUI entities) can reach it without
explicit injection.

**Why a singleton escape hatch?** Dependency injection all the way
down would require threading `Arc<dyn SystemShell>` through every
GPUI entity's constructor. For leaves that use the shell in one
place (URL-click handler), `platform::global()` is pragmatic. This
is the only approved use of the global — handlers on `AppState` must
use `self.platform`.

**Extending it**: adding a new platform op → add method to the
appropriate trait, implement in both `apple.rs` and
`unsupported.rs`. If the op is genuinely unsupportable on non-macOS,
log via `tracing::warn!` in the stub rather than silently no-op'ing
— missing functionality should be discoverable.

### 3.3 Repository pattern — `repositories::*`

**Problem**: `Settings::load()` / `save()` were inherent methods on
the data type, mixing schema with IO. Unit-testing state transitions
required filesystem setup / teardown. There was no way to spin up
an `AppState` in a test.

**Solution**: two traits in `repositories.rs`:

```rust
trait SettingsRepository: Send + Sync {
    fn load(&self) -> Settings;
    fn save(&self, settings: &Settings) -> Result<(), AlleleError>;
}
trait StateRepository: Send + Sync {
    fn load(&self) -> PersistedState;
    fn save(&self, state: &PersistedState) -> Result<(), AlleleError>;
}
```

**Production**: `JsonFileSettingsRepo` / `JsonFileStateRepo` delegate
to the existing inherent `Settings::{load,save}` methods so the seed
logic + atomic-write semantics stay in one place.

**Tests**: `InMemorySettingsRepository` / `InMemoryStateRepository`
(gated `#[cfg(test)]`) provide deterministic fakes — tests set
initial state, run handlers, assert on the saved state.

**Injection**: `Repositories` bundle lives on `AppState.repos`,
Arc-cloned into background tasks. `AppState::save_state` /
`save_settings` go through `self.repos.*` rather than calling data
types' inherent methods.

**Extending it**: adding a new persisted file → define trait with
`load` / `save` methods, add JSON-file + in-memory impls, include in
the `Repositories` bundle, inject into `AppState`.

### 3.4 Dirty-flag persistence coordinator

**Problem**: pre-refactor there were 22 `save_state()` + 15
`save_settings()` calls scattered across handlers. Easy to forget one
on a new feature; each mutation redundantly re-serialised and
re-wrote the whole file.

**Solution**: `AppState` carries two `bool` flags:

- `state_dirty`
- `settings_dirty`

Handlers call `self.mark_state_dirty()` / `self.mark_settings_dirty()`
after mutating state. The render method's final act is
`self.checkpoint_persistence()` which drains the flags, writing at
most once per file per frame (N mutations per frame coalesce to one
write).

**Where the flush happens**: very bottom of `fn render(...)` in
`main.rs`. Search for `checkpoint_persistence()` — it's the single
exit point.

**Invariant**: handlers that mutate persisted state MUST call
`mark_*_dirty()`. If they forget, the mutation is kept in memory but
never written — the next startup will load stale state. This is the
safe degenerate case (no crash, no data loss pre-restart) but still
a bug.

**Why not auto-dirty every mutation?** Rust doesn't give us a hook
into field assignment, and wrapping every field in an interior-mutable
"tracked" type would pollute the struct surface. Explicit marking is
the simplest thing that works.

### 3.5 Sub-struct composition — `AppState.*`

**Problem**: pre-refactor `AppState` had 30 flat fields across 6 impl
blocks. Field accesses like `self.sidebar_width`, `self.drawer_height`,
`self.confirming_quit` gave no hint which subsystem owned them, and
code touching one subsystem had broad read/write access to all others.

**Solution**: 5 sub-structs bundle cohesive fields (see `app_state.rs`):

- `SidebarState` — `visible`, `width`, `resizing`
- `RightPanelState` — same shape, separate type so they can diverge
- `DrawerState` — `height`, `resizing`, `rename`, `rename_focus`
- `EditorState` — `selected_path`, `expanded_dirs`, `preview`,
  `context_menu`
- `ConfirmationState` — `discard`, `dirty_session`, `quit`

Accesses become `self.sidebar.width`, `self.drawer.rename`, etc.

**Where this unlocks real wins**: a method that only needs drawer
state could take `&mut DrawerState` rather than `&mut AppState`,
reducing what it can inadvertently touch. As of writing this hasn't
been done — methods still take `&mut self` — but the option is on
the table now.

**Not refactored into sub-structs**: `user_settings`, `platform`,
`repos`, `hooks_settings_path`, `main_tab`, `scratch_pad*`,
`browser_status`, `settings_window`, `pull_warning`,
`editing_project_settings`. These are either single-field or
genuinely cross-cutting (platform, repos). Don't pack unrelated
fields into a sub-struct just for symmetry.

### 3.6 Typed errors — `errors::AlleleError`

**Problem**: pre-refactor, every fallible operation used
`anyhow::Result`. Errors crossed boundaries as `format!("{e}")`
strings; no caller could branch on failure mode. `eprintln!` was
used for both diagnostics and errors.

**Solution**: `thiserror`-backed `AlleleError` enum (see `errors.rs`)
with variants for the real failure categories: `Io`, `Json`, `Git`,
`Clone`, `Agent`, `Config`, `State`, `PlatformUnsupported`, `Other`.

**Adoption status** (as of this writing):
- ✅ `clone::create_session_clone` / `create_clone`
- ✅ `git::pull`
- ✅ `git::merge_archive` / `squash_merge_archive` / `rebase_merge_archive`
- ⏳ remaining `git::*` functions still on `anyhow::Result`
- ⏳ `config`, `agents`, `hooks` still on `anyhow::Result`

Migration strategy: convert one boundary function at a time. When
bridging to an `anyhow` internal helper, use
`.map_err(|e| AlleleError::Git(e.to_string()))` at the bridge point.

**Logging**: `tracing::{info, warn, error}` replaces `eprintln!`.
- `info!` for startup / state-transition diagnostics
- `warn!` for recoverable failures ("continuing")
- `error!` for fatal errors / crash handler
- Filter via `ALLELE_LOG` env var (default `info`)

---

## 4. Key invariants

Things that must stay true. Violating these breaks the app in
non-obvious ways.

### 4.1 Platform singleton is set exactly once, before any window opens

`Platform::detect().install_global()` in `main()` MUST complete before
any GPUI entity constructor runs. `platform::global()` panics if
called before `install_global`. This is a real invariant violation —
don't quiet the panic.

### 4.2 Pending actions flush at render time, not at enqueue time

Listeners set `self.pending_action = Some(X.into())`. Nothing
processes it until the next render tick. Between enqueue and dispatch,
the action is queued on `AppState` — don't read `pending_action`
assuming fresh state.

### 4.3 `skip_refocus` suppresses terminal refocus

Most handlers should return focus to the active terminal after
running (keyboard input goes back to Claude). Drawer / settings /
overlay actions manage focus themselves — they set
`*skip_refocus = true` in the per-family handler so the top-level
dispatcher's post-action block is skipped.

### 4.4 `save_state` / `save_settings` are private to AppState

Direct calls happen only inside `checkpoint_persistence()`. External
handlers MUST use `mark_*_dirty()` — bypassing the checkpoint means
(a) losing coalescing, (b) writing mid-mutation so a concurrent read
could see torn state, (c) being a hidden IO cost in a hot path.

### 4.5 GPUI entities are dropped to release resources

`Session.terminal_view = None` is not just a UI cleanup — dropping
the `Entity<TerminalView>` fires `PtyTerminal::Drop` which sends
`Msg::Shutdown` to the PTY's worker thread, killing the child
process. Any refactor that keeps entity handles alive needs to
consider whether PTYs are also leaking.

### 4.6 Session IDs are shared with Claude Code

A session's `id` is a UUID that is passed to claude via
`--session-id`. Claude writes its history JSONL under
`~/.claude/projects/<slug>/<id>.jsonl`. Session identity must survive
rename (e.g. auto-naming updates labels but never IDs).

### 4.7 Clone paths live under `~/.allele/workspaces/<project>/<short-id>/`

`short-id` is the first 8 chars of the session UUID. Collisions are
astronomically rare; when detected, a `-alt` suffix is appended.
Never treat clone paths as stable outside this layout.

---

## 5. Extension recipes

Standard "how do I add X?" walkthroughs.

### 5.1 Add a new PendingAction

1. Pick the appropriate family enum in `actions.rs`
   (`SessionAction`, `DrawerAction`, etc.) — or add a new family with
   its `From<...>` impl if none fits
2. Add the variant with a doc comment explaining when it fires
3. In `pending_actions.rs`, add a match arm in
   `handle_<family>_action` implementing the behaviour
4. If state was mutated: call `self.mark_state_dirty()` or
   `mark_settings_dirty()`
5. If focus should NOT return to the terminal:
   `*skip_refocus = true`
6. Call sites enqueue via `FamilyAction::Variant.into()`:
   `this.pending_action = Some(SessionAction::Foo.into());`

### 5.2 Add a new persistence repository

1. Define the trait in `repositories.rs` with `load` and `save`
2. Implement the JSON-file version delegating to the data type's
   existing load/save (preserves seed/atomic-write logic)
3. Implement an `InMemory*` version under `#[cfg(test)]`
4. Add field to `Repositories` struct; wire in
   `Repositories::production()`
5. Update `AppState.repos` usages; replace any inline IO with
   `self.repos.<new>.save(&data)`

### 5.3 Add a new platform adapter

1. Decide whether it goes in an existing trait (`SystemShell`,
   `BrowserIntegration`, etc.) or warrants a new one
2. Define the method with a `&self` receiver returning a clear
   result type (avoid `()` for anything that can fail silently)
3. Implement in `platform/apple.rs`; add a `tracing::warn!` stub in
   `platform/unsupported.rs`
4. Callers route through `self.platform.<adapter>` on `AppState` OR
   `platform::global().<adapter>` at deep leaves

### 5.4 Add a new sub-struct field to AppState

Only compose a new sub-struct if **three or more** related fields
cluster together. For 1-2 fields, keep them loose on `AppState`.
- Define the sub-struct in `app_state.rs` near the others
- Add field to `AppState`; initialize in the struct literal in
  `main.rs`
- Access patterns: `self.<cluster>.<field>`

### 5.5 Add a new action family

Justify to yourself: does this genuinely not fit in any existing
family? If yes:
1. Define `FooAction` enum in `actions.rs`
2. Add `From<FooAction> for PendingAction` impl
3. Add `PendingAction::Foo(FooAction)` wrapper variant
4. Add `handle_foo_action` to `pending_actions.rs` and a delegator
   arm in `dispatch_pending_action`

---

## 6. The macOS coupling status

As of this writing, the Linux target **compiles cleanly** via
`cargo check`, but many features are stubs (see
`platform/unsupported.rs`). Specifically:

| Feature | macOS | Linux |
|---------|-------|-------|
| APFS clone | libc::clonefile | full recursive copy (slow, correct) |
| Chrome integration | AppleScript | stub (UnsupportedBrowser) |
| Open URL | `open(1)` | `xdg-open` |
| Reveal in files | `open -R` | `xdg-open` on parent dir |
| Play sound | `afplay` | debug log only |
| Fatal dialog | `NSAlert` | `tracing::error!` only |
| Clipboard images | Cocoa pasteboard | macOS-only (gated) |
| About panel | `NSApp` about panel | macOS-only (gated) |
| PTY | rustix-openpty | rustix-openpty (Unix) |
| App menu | AppKit | ? (GPUI-dependent) |

Treat the Linux build as a **compile target, not a release target**,
until the stub behaviours are replaced with real implementations.

---

## 7. What NOT to do

Some recurring "fixes" that would undo recent architectural work.

### 7.1 Don't call `eprintln!`

Use `tracing::{info, warn, error}`. The crate-level `ALLELE_LOG`
env filter depends on consistent tracing usage.

### 7.2 Don't call `save_state()` / `save_settings()` from handlers

Use `mark_state_dirty()` / `mark_settings_dirty()`. The coordinator
at the render tick coalesces writes.

### 7.3 Don't reach for `platform::global()` from `AppState` methods

Use `self.platform` instead. The global is a concession for leaf
GPUI entities that can't realistically accept injected dependencies;
reaching for it from a place that has `self` is anti-pattern.

### 7.4 Don't sprinkle `#[cfg(target_os = "macos")]` in business logic

Put platform variance behind the adapter traits. If a new macOS-only
primitive is needed, add it to the appropriate `SystemShell` /
`BrowserIntegration` / `CloneBackend` trait.

### 7.5 Don't add impl AppState blocks that bypass PendingAction

Mutation from listeners MUST go through `pending_action`. Direct
`this.foo = bar; cx.notify()` inside a listener can land inside a
borrow-of-AppState that the render tree is still holding — leading
to the ArenaRef panic.

### 7.6 Don't use `anyhow::Result<T>` for new public functions

Use `crate::errors::Result<T>` (typed `AlleleError`). Anyhow is
fine for internal helpers where the error category is stable; public
API boundaries should use the typed surface.

### 7.7 Don't introduce a new god-object

If you find yourself adding more than 3 fields to `AppState`
directly (not inside a sub-struct), stop and ask whether a new
sub-struct / entity is warranted. The god-object was removed once;
it doesn't need to come back.

---

## 8. Open follow-ups

Known work not yet landed, in rough priority order:

- Adopt `AlleleError` in the remaining `git::*` functions
  (`fetch_and_rebase_onto_remote_branch`, `auto_commit_if_dirty`,
  `delete_ref`, `list_archive_refs`, `prune_archive_refs`, etc.)
- Extend repositories: `ScratchPadRepository`, `ArchivesRepository`
- `unwrap()` audit in `session_ops.rs` + `pending_actions.rs`
  (started; see `docs/unwrap-audit.md` if present)
- CDP-based `BrowserIntegration` for Linux (currently stub). Patrick
  wants this reviewed as a separate decision — don't unilaterally
  add it.
- Windows port: rustix-openpty is Unix-only; needs a Windows PTY
  backend (ConPTY) gated behind `cfg(windows)`.
- Tests using `InMemory*Repository` fakes — the scaffolding exists
  but no handler-level tests have been written yet.

---

## 9. For AI agents reading this

Context compression will eat this file across conversations. Before
making non-trivial changes:

1. Re-read this file
2. Re-read `CLAUDE.md` and `MEMORY/` if present
3. Check recent `git log --oneline -20` for the last few refactor
   decisions — each commit message explains its rationale
4. Run `cargo check` on both macOS and Linux (via Docker) before
   claiming a change is clean; the Linux path is easy to break
   accidentally

The seven patterns above (command, adapter, repository, dirty-flag,
sub-struct composition, typed errors, platform singleton escape
hatch) are not the only valid patterns — but any new pattern should
be explicit, documented, and reserved for real problems.
