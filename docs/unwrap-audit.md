# `unwrap()` audit

_Snapshot: phase 18 of the re-decomposition (see `docs/RE-DECOMPOSITION-PLAN.md` §5)._
_Original audit performed on the 2026-04-19 decomposition branch; this pass
re-runs the methodology on current master with the 9 production unwraps
present at the re-decomposition endpoint._

## Method

For each `.unwrap()` in production code (outside `#[cfg(test)]`), verify it:

- **Legitimate** — the unwrap is protected by a local invariant that makes
  `None` / `Err` unreachable at that program point. Add a `// SAFETY: ...`
  comment explaining the invariant.
- **Fragile** — the invariant holds today but depends on non-local code
  (caller gates, temporal coupling). Refactor so the unwrap becomes
  local-only, OR upgrade to `.expect("...")` with a reason.
- **Actually fallible** — the unwrap can genuinely fail in production.
  Convert to `Result` propagation or graceful fallback.

Test-code unwraps are not audited. `Mutex::lock().unwrap()` inside tests,
temporary directory setup, `parse().unwrap()` on hard-coded inputs — all
fine.

## Results

| # | Site | Category | Action |
|---|------|----------|--------|
| 1 | `terminal_view.rs:541` `scroll_pixel_accumulator.lock().unwrap()` | Legitimate | SAFETY comment — Mutex never poisoned; only the terminal view ever locks it |
| 2 | `terminal_view.rs:550` `scroll_pixel_accumulator.lock().unwrap()` | Legitimate | Same as #1; paired reset |
| 3 | `terminal_view.rs:2191` `nums.last().unwrap()` | Legitimate | SAFETY comment — caller checked `!nums.is_empty()` earlier in the same block |
| 4 | `pending_actions.rs:155` `clone_path.unwrap()` | Legitimate | Already has `// safe: needs_git is true` comment — keep; standardise to SAFETY wording in follow-up |
| 5 | `pending_actions.rs:161` `get_mut(cursor.project_idx).unwrap()` | Fragile | Upgrade to `.expect("cursor produced by a sidebar click; project_idx always in bounds")` |
| 6 | `main.rs:685` `self.session_context_menu.unwrap()` | Fragile | Matches the §7.5 pattern — caller gates on `Some`. Refactor so `render_session_context_menu` handles `None` via early return, like `render_editor_context_menu` was (and should be again — see #9). |
| 7 | `main.rs:2029` `.unwrap()` on `cx.open_window(...)` | Legitimate | Startup diagnostic — upgrade to `.expect("open main window")` for crash-log clarity |
| 8 | `text_input.rs:623` `prepaint.line.take().unwrap()` | Legitimate (GPUI) | GPUI's prepaint/paint invariant — `prepaint.line` is always Some by the time paint runs. SAFETY comment referencing the GPUI pattern |
| 9 | `editor.rs:130` `self.editor.context_menu.clone().unwrap()` | Fragile | Regressed from prior master-side refactor. Re-apply: make `render_editor_context_menu` handle `None` internally via early return, remove the caller gate |

Snapshot count: **9 production unwraps** (was 8 in the 2026-04-19 audit; the
one extra is #9 which regressed on master between the original decomposition
and the re-decomposition).

## Execution status

This audit document reflects the **findings** against current source. The
suggested actions (SAFETY comments for legitimate sites, `.expect()`
upgrades for fragile-but-provable sites, None-handling refactors for the
two fragile-via-caller-gate sites) are **not yet applied** in this phase —
they need a follow-up commit that touches each site.

Recommended phasing for the follow-up:
- First pass: add SAFETY comments / `.expect()` upgrades (sites #1–#5, #7, #8). Purely additive, no behaviour change.
- Second pass: the two refactors (#6, #9). Each is a small targeted edit to one render function, restructuring it so the `None` case short-circuits inside the function body.

## Principles for new unwraps

When writing new code, default to **not** unwrapping. Acceptable alternatives:

```rust
// Option: use let-else for early return
let Some(x) = maybe else { return };

// Option: propagate as Result
let x = maybe.ok_or(AlleleError::State("reason".into()))?;

// Option: expect with a reason when the invariant is truly local
let x = maybe.expect("we just checked is_some() above");
```

Only use bare `.unwrap()` when:
- you're in test code, OR
- the invariant is so obvious to every reader that a comment would be noise
  (very rare — almost always worth a SAFETY line)

GPUI's prepaint/paint pattern is the one common case where bare unwrap on
`prepaint.foo.take()` is idiomatic, because failure would indicate a
framework bug.

## Pre-existing test-code unwraps

Not audited. For reference, test blocks have:

- `git/mod.rs`: 84 (tempdir + git cmd setup)
- `clone/mod.rs`: 12
- `trust/mod.rs`: 10
- `config/mod.rs`: 9
- `rich/attachments.rs`: 7
- `repositories.rs`: 6 (in-memory Mutex locks + round-trip asserts)
- `agents/mod.rs`: 4
- `transcript.rs`: 4
- `settings.rs`: 1

These are fine — a panic during a test is just a test failure message.
