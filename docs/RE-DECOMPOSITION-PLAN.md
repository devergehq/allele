# Re-decomposition plan

> **What this is:** the playbook for re-applying the `ARCHITECTURE.md` blueprint against
> current master, after the original 22-commit decomposition branch (cut from `2c9d835`)
> diverged too far to rebase or merge cleanly.
>
> **What this is not:** the architecture itself (see [`/ARCHITECTURE.md`](../ARCHITECTURE.md))
> or the tech-stack rationale (see [`./architecture.md`](./architecture.md)). This doc is
> purely an execution plan.

---

## 1. Why we're re-decomposing

Original branch: `origin/allele/session/4186982c-5a94-4c84-85a0-f2923accdb15/task-decompose-main-focused`
(22 commits, shrunk `src/main.rs` from 5451 → 2057 lines).

Since branch cut, master added 16 commits: Rich Sidecar (`src/rich/*`), stream parser
(`src/stream/*`), keymap system (`src/keymap.rs`, `src/terminal/keymap.rs`), new-session
modal (`src/new_session_modal.rs`), transcript tailer (`src/transcript.rs`), and +1115 lines
added to `main.rs` (now 6566 lines). Nine master commits touched `main.rs` directly.

A squash-and-rebase would land as one ~5000-line merge conflict that essentially redoes
the decomposition inside conflict markers, *and* leaves the new Rich / keymap code
sitting in the old un-decomposed positions. Option 3 (fresh re-decomposition, blueprint
as reference) was chosen.

---

## 2. Two architecture docs — distinct scopes

| Doc | Scope | Audience |
|-----|-------|----------|
| [`/ARCHITECTURE.md`](../ARCHITECTURE.md) | **Code-level:** module layout, 6 patterns, invariants, extension recipes, what-not-to-do | Contributors + AI agents editing source |
| [`docs/architecture.md`](./architecture.md) | **System-level:** GPUI vs Tauri decision, PTY flow, process model, tech-stack rationale | Anyone evaluating or onboarding onto the stack |

Both are canonical. Don't merge them; they answer different questions.

---

## 3. Pattern status on master

Per the 6 patterns in `ARCHITECTURE.md` §3:

| # | Pattern | Status on master | Target location |
|---|---------|------------------|-----------------|
| 3.1 | **Command (`PendingAction`)** | 🟡 Partial: flat 41-variant enum exists in `main.rs:L90-L204`. Not family-split. | `src/actions.rs` |
| 3.2 | **Adapter (`platform::*`)** | 🔴 Missing: no `src/platform/`. Platform-specific code delegated ad-hoc to `clone::`, `browser::`, `hooks::`. | `src/platform/{mod,apple,unsupported}.rs` |
| 3.3 | **Repository (`SettingsRepository`, `StateRepository`)** | 🔴 Missing: `Settings::load/save` called directly; `PersistedState` is in `src/state/mod.rs` but not behind a trait. | `src/repositories.rs` |
| 3.4 | **Dirty-flag persistence coordinator** | 🔴 Missing: scattered `save_state()` / `save_settings()` calls through dispatch. No coalescing. | `AppState.state_dirty`, `.settings_dirty`, `.checkpoint_persistence()` |
| 3.5 | **`AppState` sub-struct composition** | 🔴 Missing: `AppState` at `main.rs:L213-L300` is flat god-struct. | `src/app_state.rs` |
| 3.6 | **Typed errors (`AlleleError`) + `tracing`** | 🔴 Missing: 69 `eprintln!`/`println!` sites in `main.rs` alone. No `src/errors.rs`. `anyhow::Result` in `git::`, `clone::`, etc. | `src/errors.rs`; migrate call boundaries |

Legend: 🟢 applied · 🟡 partial · 🔴 missing

---

## 4. Module reconciliation

Modules that already exist on master and whose responsibilities overlap blueprint targets:

| Master module | Lines | Blueprint expectation | Reconciliation action |
|---------------|-------|----------------------|----------------------|
| `src/state/mod.rs` | 223 | Blueprint puts `PersistedState` under `StateRepository` | Keep state types here; add `StateRepository` trait in `src/repositories.rs` whose JSON-file impl delegates to existing `PersistedState::{load,save}` |
| `src/session/mod.rs` | 254 | Blueprint has `Session` + status machine here | ✅ already aligned |
| `src/agents/mod.rs` | 362 | Blueprint §2 lists `agents/` | ✅ already aligned |
| `src/project/mod.rs` | — | Blueprint §2 lists `project/` | ✅ already aligned |
| `src/trust/mod.rs` | 264 | Blueprint §2 lists `trust/` | ✅ already aligned |
| `src/hooks/mod.rs` | 401 | Blueprint §2 lists `hooks/` | ✅ already aligned — hook event *handlers* (apply/auto-name) still in `main.rs:L2493-L2770` and need extraction to `src/hook_events.rs` |
| `src/sidebar/mod.rs` | — | Blueprint puts render in `src/sidebar/render.rs` | Add `src/sidebar/render.rs` submodule; keep existing `mod.rs` as orchestrator |
| `src/terminal/*`, `src/git/mod.rs`, `src/clone/mod.rs`, `src/config/mod.rs`, `src/settings.rs`, `src/settings_window.rs`, `src/scratch_pad/*`, `src/text_input.rs`, `src/browser/mod.rs` | various | Blueprint §2 lists all | ✅ already aligned — no structural change needed |

Modules **new since divergence** (not in blueprint, need coverage in plan):

| Module | Lines | Treatment |
|--------|-------|-----------|
| `src/rich/{mod,rich_view,compose_bar,markdown,document,attachments}.rs` | ~3630 | Already modularized. Verify no AppState field that belongs in a sub-struct. Consider `RichState` sub-struct if ≥3 related fields on `AppState`. |
| `src/stream/{mod,parser,types}.rs` | 605 | Already modularized. No AppState coupling expected. |
| `src/keymap.rs` | 293 | Top-level app keymap loader. Keep as-is. |
| `src/terminal/keymap.rs` | 382 | Terminal-scoped keymap. Distinct from `src/keymap.rs`. Keep both; document distinction. |
| `src/new_session_modal.rs` | 668 | Modal entity. `main.rs:L1810-L1844`, `L1196-L1232` own construction. Leave modal module; extract construction to `session_ops.rs`. |
| `src/transcript.rs` | 343 | Tailer. `main.rs:L266, L687-L704, L753` own lifecycle. Extract lifecycle to `session_ops.rs` or `RichState`. |

---

## 5. Phase sequence (execution order)

Each phase should be one commit, on a fresh session branch off current master. Acceptance
criteria for every phase:

- `cargo check` clean
- `cargo clippy --all-targets -- -D warnings` clean
- `cargo run --bin allele` launches without panic; sidebar renders; a session can be started
- No new `eprintln!` / `println!` introduced (phases 9+ reduce the existing count)

Phases are numbered for ordering; parentheses show which original commit (from
[`./session-2026-04-19.md`](./session-2026-04-19.md)) they correspond to.

### Structural extractions (mechanical moves)

| # | Extraction | Source regions in current `main.rs` | Depends on | Notes |
|---|------------|--------------------------------------|------------|-------|
| 1 | `PendingAction` + `SessionCursor` → `src/actions.rs` | L90-L211 | — | 41 variants (original had fewer — new variants added post-divergence) |
| 2 | `AppState` struct + inherent impl → `src/app_state.rs` | struct L213-L300; impl L306-L3240 (41 methods) | #1 | Largest phase. Move wholesale; no behaviour change. |
| 3 | Session lifecycle → `src/session_ops.rs` | `add_session_to_project` L1488-L1767; `_with_details` L1861-L2164; `close_session_keep_clone` L2165-L2200; `resume_session` L2821-L2995; `remove_session` L2996-L3180; `navigate_session` L2772-L2820 | #2 | ~1700 lines combined |
| 4 | Sidebar render → `src/sidebar/render.rs` (new submodule under existing `src/sidebar/`) | L4785-L5633 (~848 lines) inside `impl Render::render` | #2 | Extract to free-standing `build_sidebar_items` fn taking `&AppState` |
| 5 | Drawer render + helpers → `src/drawer/mod.rs` (new) | `spawn_drawer_tab` L2201-L2259; `ensure_drawer_tabs` L2260-L2307; `focus_active_drawer_tab` L2460-L2492; drawer layout block inside `render` L5822-L6150 | #2 | `src/drawer/` does not yet exist on master |
| 6 | Pending-action dispatcher → `src/pending_actions.rs` | Dispatch match block currently inside `impl Render::render` at L3950-L4712 | #1, #2, #3 | **Refactor step first:** lift the inline match out of `render` into `AppState::dispatch_pending_action(&mut self, cx)`, then move the method to the new module |
| 7 | Editor tab render → `src/editor.rs` | `render_editor_view` L577-L686; `render_editor_context_menu` L1010-L1064; `collect_tree_rows` L1248-L1341; `load_preview` L1362-L1380 | #2 | |
| 8 | Hook event handlers → `src/hook_events.rs` | `apply_hook_event` L2493-L2652; `trigger_auto_naming` L2653-L2770 | #2 | Not to be confused with existing `src/hooks/mod.rs` (the receiver/watcher) |

### Pattern retrofits (introduce new abstractions)

| # | Retrofit | Scope | Depends on |
|---|----------|-------|------------|
| 9 | Introduce `AlleleError` + `tracing` | New `src/errors.rs` with `thiserror`-backed enum; init `tracing` in `main()`; migrate 69 `eprintln!`/`println!` call sites in `main.rs` (+ any discovered in extracted modules from phases 1-8) | #1-8 |
| 10 | Split `PendingAction` into 8 family enums | Refactor `src/actions.rs`: `SessionAction`, `ArchiveAction`, `DrawerAction`, `SidebarAction`, `ProjectAction`, `SettingsAction`, `BrowserAction`, `OverlayAction` + `From<>` impls | #1 |
| 11 | Split `AppState` into sub-structs | Refactor `src/app_state.rs`: `SidebarState`, `RightPanelState`, `DrawerState`, `EditorState`, `ConfirmationState`. **New post-master:** evaluate whether `RichState` (rich_view handle, transcript_tailer) or `TranscriptState` warrants its own sub-struct | #2 |
| 12 | Dirty-flag persistence coordinator | Add `state_dirty` / `settings_dirty` to `AppState`; `mark_state_dirty()` / `mark_settings_dirty()` helpers; `checkpoint_persistence()` called at end of `render`. Replace all inline `save_state()` / `save_settings()` (see `main.rs` L2812, L3070, L4070, L4151, L4505, L4556, L4575, L4589, L4625, L4644, L5087, L5128, + any in extracted session_ops) | #2 |
| 13 | Repository traits for Settings + PersistedState | New `src/repositories.rs` with `SettingsRepository` + `StateRepository` traits, `JsonFileSettingsRepo` / `JsonFileStateRepo` (delegate to existing `Settings::{load,save}` / `PersistedState::{load,save}` in `src/state/mod.rs`), `InMemory*Repository` under `#[cfg(test)]`. Bundle into `Repositories` struct on `AppState.repos` | #12 |
| 14 | Platform adapter traits | New `src/platform/{mod,apple,unsupported}.rs` with `CloneBackend`, `BrowserIntegration`, `SystemShell` traits. `Platform::detect().install_global()` called once in `main()` before any window opens. Migrate `show_about_panel` (currently the only `#[cfg(target_os = "macos")]` in `main.rs` at L3330-L3394) to use the adapter | — |
| 15 | Split dispatcher into per-family handlers | Refactor `src/pending_actions.rs` from single match into `dispatch_pending_action` (8-arm top-level) delegating to `handle_session_action`, `handle_archive_action`, etc. | #6, #10 |

### Verification + polish

| # | Task | Depends on |
|---|------|------------|
| 16 | Linux build verification via Docker (`rust:1.83` + `cargo check --target x86_64-unknown-linux-gnu`) | #14 |
| 17 | Adopt `AlleleError` at remaining `git::*` / `clone::*` / `config::*` / `agents::*` / `hooks::*` boundaries (the blueprint §3.6 "Adoption status" target) | #9 |
| 18 | `unwrap()` audit on production code — repeat the methodology from `docs/unwrap-audit.md` against current master. Document results in updated `unwrap-audit.md`. | #1-17 |
| 19 | Final `ARCHITECTURE.md` status update — flip the blueprint header from "target" to "applied" (or update to reflect whatever ended up partial). Update each "Adoption status" subsection. | #17, #18 |

---

## 6. New-subsystem integration notes

The Rich Sidecar / transcript / keymap / modal subsystems were added post-divergence.
They are already modularized but cross `main.rs` at these integration points:

| Integration | `main.rs` ref | Action during re-decomp |
|-------------|---------------|-------------------------|
| `rich::RichView` entity creation | L263, L705-L763 | Move lifecycle into `session_ops.rs` (phase 3) or a new `RichState` sub-struct (phase 11) |
| `rich::RichViewEvent` subscription | L734-L758 | Moves with the lifecycle |
| `transcript::TranscriptTailer` | L266, L687-L704, L753 | Same as above — lifecycle follows session |
| `transcript::TranscriptEvent` routing | L695-L696 | Handler stays near session_ops or becomes its own `handle_transcript_event` |
| `keymap::load` | L3302 (called in `install_app_menu`) | Stays in `main()` setup — no move |
| `new_session_modal::NewSessionModal` construction | L1810-L1844 | Move with session_ops (phase 3) |
| `new_session_modal::EditSessionModal` | L1196-L1232 | Same |

**Do not** re-entangle these during extraction. If a sub-struct emerges for rich/transcript
state, it belongs in phase 11, not earlier.

---

## 7. Original playbook reference

The 21-commit walkthrough of the original decomposition is preserved at
[`./session-2026-04-19.md`](./session-2026-04-19.md). Use it as a reference for:

- Commit message style (imperative, phase-prefixed)
- Known gotchas encountered (BSD sed, PendingAction double-parens, tuple destructuring,
  `*skip_refocus` deref, `render_editor_context_menu` None guard)
- Linux cross-build command

The phase sequence in §5 above is an updated order for current master; it doesn't follow
the original 1:1 because master already has some modules (`src/state/mod.rs`,
`src/sidebar/mod.rs`) and has more to extract (Rich / transcript / new_session_modal).

---

## 8. Next-session handoff

When resuming the re-decomposition:

1. Start from tip of master (`origin/master`, currently `538da17`).
2. Create a fresh session branch: `allele/session/<uuid>/task-redecompose-phase-<N>`.
3. Pick the next unfinished phase from §5. Phases are ordered by dependency; don't
   reorder without updating the table.
4. Re-read the relevant section of [`/ARCHITECTURE.md`](../ARCHITECTURE.md) before
   starting — it explains **why** each pattern exists, which disambiguates edge cases
   the mechanical extraction can't.
5. Verify acceptance criteria (cargo check, clippy, app launch) before committing.
6. Update the phase row in this doc to ✅ when landed. When a phase changes the target
   state of a blueprint pattern, update the blueprint's "Adoption status" section in
   [`/ARCHITECTURE.md`](../ARCHITECTURE.md) §3.6.
