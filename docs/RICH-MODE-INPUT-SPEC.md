# Rich Mode Input UX Spec

> **Still current** — the ComposeBar UX (multi-line input, long-paste
> collapsing into cards, file/image attachments, Cmd+Enter submit) is
> preserved in the Rich Sidecar. What changed is the send path: the
> ComposeBar now emits a `Submit` event upward and the parent routes it
> into the PTY via bracketed paste (the Scratch Pad's path), instead of
> calling `RichSession::send_prompt` on a stdin-piped child process.
> See `docs/RICH-SIDECAR.md`.

## Compose Bar

A discrete input area at the bottom of the RichView. Structurally different from
PTY mode (where stdin IS the interface) — here it's a compose-and-submit model.

```
┌────────────────────────────────────────────┐
│         Activity feed (scrollable)         │
├────────────────────────────────────────────┤
│ ┌──────────────────────────────┐ ┌──────┐ │
│ │ Multi-line text input        │ │ Send │ │
│ │                              │ │ (⌘⏎) │ │
│ │ [Pasted 87 lines ▸]         │ ├──────┤ │
│ │                              │ │  📎  │ │
│ │ rest of my prompt here...   │ │      │ │
│ │                              │ │ img1 │ │
│ │ [img2.png ▸]                │ │ img2 │ │
│ └──────────────────────────────┘ └──────┘ │
└────────────────────────────────────────────┘
```

## Multi-line Text Input

- Extends existing `text_input.rs` to support multiple lines
- Full macOS text conventions:
  - Cmd+A (select all), Cmd+C/V/X (copy/paste/cut)
  - Opt+Left/Right (word jump), Opt+Shift+Left/Right (word select)
  - Cmd+Left/Right (line start/end)
  - Shift+arrows (selection)
  - Fn+Delete (forward delete)
- Grows vertically as content is added (up to a max height, then scrolls)
- Submit: Cmd+Enter (not bare Enter — Enter inserts a newline)
- Speech-to-text compatible — content is visible and editable before submission

## Long-Paste Collapsing

When pasting content exceeding a threshold (~20 lines or ~50 lines of code):

1. Detect paste event (Cmd+V with multi-line content)
2. Replace the pasted content in the visible input with a **PasteCard** token
3. Store the full content separately in a `Vec<Attachment>` on the compose bar
4. PasteCard renders inline as: `[87 lines — click to review ▸]`
5. Click → opens scratch pad with the full content, editable
6. Edits in scratch pad save back to the attachment
7. On submit: full content (expanded paste cards) assembled into the prompt string

**Why:** Long pastes make it impossible to see your actual prompt. Speech-to-text
errors hide inside collapsed paste and can't be caught until after submission.
This design keeps the compose area usable while making the full content inspectable.

## File & Image Attachments

### Attachment Flow

1. User attaches a file via:
   - Drag-and-drop onto the compose bar
   - Paste from clipboard (Cmd+V with image data)
   - Attachment button (📎) → file picker
2. File is copied to temporary storage: `~/.allele/attachments/<session_id>/<uuid>.<ext>`
3. A thumbnail/preview card appears in the compose bar:
   - Images: small thumbnail preview
   - Other files: icon + filename + size
4. Cards are removable (click X to detach)

### Prompt Assembly

On submit, attachments are prepended to the prompt:

```
[The user has attached the following files for this prompt. Read each file
before processing the prompt below.]

Attached files:
- Image: ~/.allele/attachments/<session>/<uuid>.png
- File: ~/.allele/attachments/<session>/<uuid>.rs

---

<user's actual prompt text here, with paste cards expanded>
```

Claude Code reads the files via its Read tool (images are multimodal-readable).
The attachment instruction is invisible to the user — it's injected by Allele.

### Lifecycle

- Attachments are tracked per-session in a `Vec<AttachedFile>` on the session
- On session end/archive: cleanup all files in `~/.allele/attachments/<session_id>/`
- On app quit: sweep `~/.allele/attachments/` for orphaned session dirs

### Supported Types

- Images: PNG, JPG, GIF, WebP, SVG (Claude Code reads these natively)
- Code/text files: any text file
- PDFs: Claude Code can read these too
- Binary files: warn user that Claude can't read binary content

## Submission Flow

1. User composes prompt (text + optional paste cards + optional attachments)
2. Cmd+Enter triggers submit
3. Allele assembles the full prompt:
   a. Attachment preamble (if any files attached)
   b. Paste cards expanded to full content
   c. User's visible text
4. Allele calls either:
   - `RichSession::spawn(prompt, ...)` for first message
   - `RichSession::resume(prompt, ...)` for follow-ups (new process with --resume)
5. Compose bar clears, activity feed starts streaming new events

## Implementation Notes

- The compose bar is a GPUI View, child of RichView
- Text input builds on existing `text_input.rs` (extend to multi-line)
- Paste detection: intercept Cmd+V, check content length, decide inline vs card
- File copy uses `std::fs::copy` to the temp directory
- Prompt assembly is a pure function: `fn assemble_prompt(text, paste_cards, attachments) -> String`
