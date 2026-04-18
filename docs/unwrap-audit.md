# `unwrap()` audit

_Snapshot as of commit preceding the follow-up pass._

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
fine. The audit target is the 8 production unwraps found at snapshot
time.

## Results

| # | Site | Category | Action |
|---|------|----------|--------|
| 1 | `terminal_view.rs:1758` `Mutex::lock().unwrap()` | Legitimate | SAFETY comment |
| 2 | `terminal_view.rs:1769` `Mutex::lock().unwrap()` | Legitimate | SAFETY comment |
| 3 | `terminal_view.rs:1843` `nums.last().unwrap()` | Legitimate | SAFETY comment |
| 4 | `pending_actions.rs:132` `clone_path.unwrap()` | Legitimate | Upgraded to `.expect` + SAFETY comment |
| 5 | `pending_actions.rs:138` `get_mut(...).unwrap()` | Fragile | Upgraded to `.expect` + SAFETY comment |
| 6 | `main.rs:1354` `open_window(...).unwrap()` | Legitimate | Upgraded to `.expect` for diagnostics |
| 7 | `editor.rs:115` `context_menu.clone().unwrap()` | Fragile | **Refactored** — function now handles `None` internally via early return, caller no longer gates |
| 8 | `text_input.rs:641` `prepaint.line.take().unwrap()` | Legitimate (GPUI) | SAFETY comment |

Outcome:
- 1 site refactored to remove the unwrap entirely (#7)
- 4 sites upgraded to `.expect` with explanatory messages (#4, #5, #6, plus #7's sibling call)
- 4 sites kept as `.unwrap()` but with `// SAFETY: ...` documentation
- 0 sites converted to `Result` (none were actually fallible)

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

Not audited. For reference, test blocks had:

- `git/mod.rs`: 84 (tempdir + git cmd setup)
- `clone/mod.rs`: 12
- `trust/mod.rs`: 10
- `config/mod.rs`: 9
- `agents/mod.rs`: 4
- `repositories.rs`: 5 (in-memory Mutex locks + round-trip asserts)
- `settings.rs`: 1

These are fine — a panic during a test is just a test failure message.
