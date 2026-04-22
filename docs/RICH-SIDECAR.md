# Rich Sidecar — Read-Only UI Companion to the PTY

> A structured, scrollable, searchable view of what happened in the
> active Claude Code session. Sits alongside the PTY terminal view; does
> NOT spawn, drive, or communicate with the `claude` CLI. Claude Code
> runs interactively as normal — Allele watches the files it writes.

## Architecture

```
┌───────────────────────────────────────────────────────────┐
│                    Allele (Rust/GPUI)                     │
│  ┌────────────────────┐    ┌───────────────────────────┐  │
│  │   PTY Terminal     │    │    Rich Sidecar Panel     │  │
│  │                    │    │  ┌─────────────────────┐  │  │
│  │   running          │    │  │  Transcript feed    │  │  │
│  │   `claude`         │    │  │  (scrollable)       │  │  │
│  │   interactively    │    │  │                     │  │  │
│  │                    │    │  │  - user prompts     │  │  │
│  │   (keyboard input  │    │  │  - thinking blocks  │  │  │
│  │    OR ComposeBar   │    │  │  - tool calls       │  │  │
│  │    via bracketed   │    │  │  - subagents        │  │  │
│  │    paste)          │    │  └─────────────────────┘  │  │
│  │                    │    │  ┌─────────────────────┐  │  │
│  │                    │    │  │     ComposeBar      │  │  │
│  │                    │    │  │  paste cards +      │  │  │
│  │                    │    │  │  attachments +      │  │  │
│  │                    │    │  │  Cmd+Enter submit   │  │  │
│  │                    │    │  └─────────┬───────────┘  │  │
│  └──────────▲─────────┘    └────────────┼──────────────┘  │
│             │                            │                 │
│             │  terminal.write(           │                 │
│             │    bracketed_paste(text))  │                 │
│             └────────────────────────────┘                 │
│                                                            │
│  ┌──────────────────────────────────────────────────────┐  │
│  │         src/transcript (TranscriptTailer)            │  │
│  │    reads ~/.claude/projects/<cwd>/<session>.jsonl    │  │
│  │         + <session>/subagents/agent-*.jsonl          │  │
│  └──────────────────────────────▲───────────────────────┘  │
└─────────────────────────────────┼──────────────────────────┘
                                   │ append-only writes
                                   │
                      ~/.claude/projects/... (Claude Code)
```

## Data flow

1. User types in PTY **or** composes in ComposeBar.
2. If ComposeBar: `RichViewEvent::Submit { text, attachments }` bubbles
   up to the app layer. App assembles the final byte payload (e.g. `@`
   prefix per attachment, user text appended) and writes it to the PTY
   master fd using bracketed paste (`\x1b[200~...\x1b[201~`) followed by
   `\r` after ~80 ms. This is the same path Scratch Pad uses today
   (`src/main.rs::scratch_pad_send`).
3. `claude` reads the bytes as if they were typed, runs its turn, and
   writes records to `~/.claude/projects/<dashed-cwd>/<session>.jsonl`
   and `<session>/subagents/agent-*.jsonl` as they occur.
4. `TranscriptTailer::poll()` (called on a ~100 ms timer) reads newly
   appended lines, parses via `StreamParser::feed_line`, returns
   `TranscriptEvent`s.
5. The sidecar's `RichView::apply_event` updates the document model;
   the render walks `document.blocks()` producing GPUI elements.

## What the sidecar does NOT do

- Never runs `Command::new("claude")`.
- Never passes `-p`, `--input-format stream-json`, or
  `--output-format stream-json`.
- Never opens a pipe to `claude`'s stdin.
- Never reads from `claude`'s stdout.
- Never calls any Anthropic API directly.

Claude Code operates as an opaque, normally-interactive subprocess
launched by the user (or by Allele's session bootstrap, same as today).
Allele only reads files Claude Code writes.

## Module layout

| Path | Role |
|------|------|
| `src/rich/compose_bar.rs` | Multi-line input widget (paste cards, attachments, Cmd+Enter) |
| `src/rich/attachments.rs` | Local filesystem attachment pipeline |
| `src/rich/markdown.rs` | pulldown-cmark → GPUI element tree |
| `src/rich/document.rs` | In-memory document model (`Block`, `BlockKind`) |
| `src/rich/rich_view.rs` | GPUI view — feeds events to the document, renders blocks |
| `src/stream/types.rs` | `RichEvent` enum + serde wire types |
| `src/stream/parser.rs` | NDJSON line → `Vec<RichEvent>` (`feed_line`) |
| `src/transcript.rs` | File tailer + JSONL envelope unwrapping + subagent discovery |

## On-disk conventions (Claude Code, verified empirically)

- Project directory: `~/.claude/projects/<dashed-cwd>/` where
  `<dashed-cwd>` is the absolute cwd with every non-alphanumeric
  character replaced by `-`. Example:
  `/Users/foo/.allele/workspaces/allele/e95d96e2` →
  `-Users-foo--allele-workspaces-allele-e95d96e2`.
- Main transcript: `<session-uuid>.jsonl` — appended one record per
  event (user prompt, assistant turn, tool result, etc.).
- Subagents: `<session-uuid>/subagents/agent-<id>.jsonl` — one file per
  subagent invocation, same record schema, `isSidechain: true` on every
  line.

## Gotchas & follow-ups

- **Busy-turn guard.** Submitting a prompt while Claude is still
  responding interleaves bytes in the PTY buffer. `RichView::set_busy`
  locks the ComposeBar; the app layer should drive this from the
  transcript-tail state (e.g. treat the window between the last user
  turn and the next `result` as busy).
- **The 80 ms delay before `\r`** in `scratch_pad_send` is a workable
  hack tuned to Claude Code's paste heuristic. If CC changes that
  heuristic, this tuning moves with it.
- **Cross-agent portability.** The ComposeBar→PTY path works for any
  agent running in a PTY (OpenCode, Codex, shell). The transcript
  tailer is Claude-Code-specific; a sibling module would be needed to
  parse another agent's log format.
- **@-mention attachment convention** is Claude Code specific
  (`@path` on its own line triggers a file read). Other agents may use
  different conventions — route via the coding-agent registry.
