# Rich Mode Implementation Progress

## Session (branch: `allele/session/09521f62-…`)

| Item | Commit | Status |
|------|--------|--------|
| 1. Markdown rendering | `e96fdb9` | ✅ Done |
| 2. File attachments (picker + drag-drop + paste + cards + preamble + cleanup) | `297463d` | ✅ Done |
| 3. Long-paste collapsing (⟪paste-N-K⟫ tokens, expand on submit) | `ad300c9` | ✅ Done |
| 4. Click-to-expand (Thinking / ToolCall / Diff blocks) | `30ca2dc` | ✅ Done |
| 5. Permission modal | — | ⏳ Next |
| 6. stream-json stdin — persistent session, cold-start eliminated | `d56b50f` | ✅ Done |

---

## What was built

### Item 1 — Markdown rendering (`e96fdb9`)

- `src/rich/markdown.rs` (new): pure `render(content, streaming, font_size) -> Div`
- pulldown-cmark 0.13 → GPUI element tree
- H1–H6 with chevron-scaled sizes, bold/italic/strikethrough, inline + fenced code (JetBrains Mono), ordered/unordered lists, links (blue + underline), thematic breaks
- Streaming-safe: mid-token emphasis reflows when close tag arrives
- 3 unit tests

### Item 2 — File attachments (`297463d`)

- `src/rich/attachments.rs` (new): `Attachment` struct, `copy_file`, `save_image`, `cleanup_session`, `sweep_orphans`; `sanitise_ext` path guard; READABLE_EXTS allow-list; 5 unit tests
- `src/rich/compose_bar.rs`: `session_id` + `Vec<Attachment>` state; 📎 picker (`cx.prompt_for_paths`); drag-drop (`on_drop::<ExternalPaths>`); clipboard image paste (`ClipboardEntry::Image`); attachment cards with ✕ remove + ⚠ binary warning; `ComposeBarEvent::Submit` carries `Vec<Attachment>`
- `src/rich/rich_view.rs`: `assemble_prompt` prepends "Attached files:" preamble when non-empty; attachment-only submits allowed
- `src/main.rs`: `remove_session` calls `cleanup_session`; startup background thread sweeps orphans
- 8 unit tests (5 attachment + 3 prompt assembly)

### Item 3 — Long-paste collapsing (`ad300c9`)

- `PastedChunk { id: u32, full_text: String, line_count: usize }` on `ComposeBar`
- `paste()` counts newlines; threshold is `LONG_PASTE_THRESHOLD = 20`
- Paste ≤ threshold: verbatim (regression-free)
- Paste > threshold: stores chunk, inserts sentinel `⟪paste-{id}-{n}⟫`; visible label `[Pasted N lines ▸]`
- `submit()` expands all tokens via `expand_paste_tokens(content, chunks) -> String`
- 6 unit tests (threshold + expansion round-trip)

### Item 4 — Click-to-expand (`30ca2dc`)

- `render_block` takes `cx: &mut Context<RichView>` for child `cx.listener` handlers
- Thinking, ToolCall, Diff headers all get `.id()` + mouse-down toggle calling `toggle_collapsed`
- Chevron `▸` collapsed / `▾` expanded on all three block types
- `PointingHand` cursor on interactive headers
- ToolCall expanded view renders pretty-printed `input_full` JSON
- Diff default: expanded; Thinking default: collapsed (per existing behaviour)
- Non-interactive blocks unchanged

### Item 6 — Persistent stream-json session (`d56b50f`)

**Spike findings:**
- `-p --input-format stream-json` accepts NDJSON on stdin: `{"type":"user","message":{"role":"user","content":"..."}}`
- Multi-turn on one process verified: second turn's `cache_creation` dropped from 15 391 → 24 tokens (massive reuse), eliminating the ~1–2 s per-turn cold start
- Session ID must be a valid UUID (plain strings rejected)

**Implementation:**
- `src/rich/rich_session.rs` fully rewritten: spawns once, keeps stdin pipe open; `send_prompt()` writes NDJSON; `drain_events()` clears `in_progress` on `SessionResult`; `kill()` drops stdin first for clean exit; `is_in_progress()` drives UI busy signal
- `src/rich/rich_view.rs`: `handle_submit` rewritten to reuse persistent session via `send_prompt`; busy guard blocks input while turn is in-flight

---

## Item 5 — Permission Modal (remaining)

### Spike first (~30 min)

Verify that PreToolUse hook return `{"decision":"deny"}` actually blocks tool execution before building any UI. Use a hook that writes a request file then reads a response file.

The hook format from the CC docs is:
```json
{"decision": "allow"}
{"decision": "deny", "reason": "..."}
```

**Note:** From today's spike attempt, the process itself didn't block — the hook in `/tmp/perm-spike-hook.sh` was never called because `--settings` override didn't apply the hooks from that file alongside the global ones cleanly. Needs a clean re-test with proper `settings.json` merge or a `~/.allele/settings.json` written directly.

### If spike passes — build plan

1. **`~/.allele/bin/permission-hook.sh`** — write `~/.allele/perms/<session_id>/<request_id>.request`, poll for `<request_id>.response`, emit decision JSON, cleanup both files
2. **`PermissionWatcher` (Rust)** — polls `~/.allele/perms/<session_id>/` each 16ms tick for `.request` files; `respond()` writes `.response` atomically (write to `.tmp`, rename)
3. **GPUI modal** — absolute-positioned overlay, dimmed backdrop, tool_name + input display, Allow (Enter) / Deny (Esc) buttons
4. **Wire into `RichView` render loop** — `permission_watcher.poll()` after `drain_events()`; `active_permission: Option<PermissionRequest>` on `RichView`; modal shown when `Some`

### Hook wiring

Only tools NOT in `--allowedTools` go through the hook. Current `--allowedTools` in `RichView::spawn_session`:
- Read, Edit, Write, Grep, Glob

Hook fires for: Bash (and any other non-allow-listed tool).

```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Bash",
      "hooks": [{"type": "command", "command": "~/.allele/bin/permission-hook.sh"}]
    }]
  }
}
```

This goes into the settings file passed via `--settings` to the `claude -p` process.

---

## Learning from this session

- **Single persistent stdin process eliminates cold start.** The per-turn `--resume` model was paying 1–2 s startup cost every turn. stdin NDJSON reuses the same process indefinitely; cache_creation drops to near-zero on turn 2+.
- **Stage A+B+C combined was correct.** Attachments (picker + drag-drop + paste) share the same `copy_file` pipeline — splitting stages would have produced a temporary API. One coherent diff, one commit.
- **Test FS isolation from day one.** Two attachment tests raced on the shared `~/.allele/attachments/` root under `cargo test`'s default parallelism. Fixed by consolidating into a single serial test with an active-id list. Rule: any test writing to a shared root needs isolation (tempdir or serial marker) before the first run.
- **`cargo test` truncates output in piped invocations.** Run the test binary directly for reliable output: `./target/debug/allele-test <filter> -- --nocapture`.

---

## Test count

105 tests passing (`cargo test --release`) as of `d56b50f`.

---

## Next session

Start Item 5: re-run the PreToolUse spike cleanly (write `~/.allele/settings.json` directly or isolate to a dedicated test project), confirm deny works, then build `PermissionWatcher` + modal + hook script. Once Item 5 ships, perform UAT across all 6 items.
