# Allele Rich Mode — Implementation Plan

> **SUPERSEDED (2026-04-21).** This plan targeted a stdin-driven
> `claude -p --input-format stream-json` child process. That approach is
> no longer the architecture: the Rich UI is now a **read-only sidecar**
> that tails Claude Code's own JSONL transcript while `claude` continues
> running interactively in the PTY. The ComposeBar writes user prompts
> into the PTY via the Scratch Pad's bracketed-paste path (identical to
> keyboard input), not into any child process stdin. See
> `docs/RICH-SIDECAR.md` for the current architecture. Document kept
> for historical context on what was built and why it changed.

> Structured rendering of Claude Code output via CLI stream-json,
> replacing the raw PTY terminal grid with semantic GPUI components.

## Architecture Overview

```
┌──────────────────────────────────────────────────────┐
│                   Allele (Rust/GPUI)                  │
│                                                       │
│  ┌─────────────┐    ┌──────────────────────────────┐ │
│  │  PTY Mode   │    │        Rich Mode              │ │
│  │  (existing) │    │                               │ │
│  │             │    │  ┌─────────────────────────┐  │ │
│  │ alacritty   │    │  │   Stream Parser          │  │ │
│  │ _terminal   │    │  │   (NDJSON → Rust types)  │  │ │
│  │             │    │  └────────┬────────────────┘  │ │
│  │ raw PTY     │    │           │                    │ │
│  │ grid        │    │  ┌────────▼────────────────┐  │ │
│  │             │    │  │  Event Dispatcher         │  │ │
│  │             │    │  │  (text, tool, permission) │  │ │
│  │             │    │  └────────┬────────────────┘  │ │
│  │             │    │           │                    │ │
│  │             │    │  ┌────────▼────────────────┐  │ │
│  │             │    │  │  GPUI Rich Components    │  │ │
│  │             │    │  │  - Text blocks           │  │ │
│  │             │    │  │  - Tool call cards       │  │ │
│  │             │    │  │  - Permission modal      │  │ │
│  │             │    │  │  - Diff viewer           │  │ │
│  │             │    │  └─────────────────────────┘  │ │
│  └─────────────┘    └──────────────────────────────┘ │
│         │                        │                    │
│         └──── same session ID ───┘                    │
│                (--resume for switching)                │
└──────────────────────────────────────────────────────┘
         │                         │
    PTY process              CLI process
    claude --session-id      claude -p --output-format stream-json
                                    --session-id <same-id>
```

**Key insight from investigation:** The Node.js sidecar (from the council debate)
is unnecessary. The Claude CLI's `--output-format stream-json` provides the same
structured events directly, with session ID support and resume capability.
This eliminates the Node.js dependency entirely.

## The Decision: CLI Stream-JSON over SDK Sidecar

| Factor | SDK Sidecar | CLI Stream-JSON |
|--------|------------|-----------------|
| Runtime dependency | Node.js required | None (claude CLI only) |
| Process chain | Rust → Node → SDK → CLI | Rust → CLI |
| Session IDs | Not supported by query() | Supported (--session-id) |
| Resume | Not supported | Supported (--resume) |
| PTY/rich sharing | Impossible | Same session, switchable |
| Permission handling | canUseTool callback | PreToolUse hook + file IPC |
| Stream format | TypeScript objects | Claude API NDJSON (stable) |
| New CC features | Must update SDK | Appear automatically |

---

## Implementation Phases

### Phase S: Validation Spike [2 hours, DO FIRST]

**Purpose:** Verify the three critical assumptions before committing.

**Test 1 — Stream completeness:**
```bash
claude -p "Read src/main.rs, then add a comment to line 1" \
  --output-format stream-json \
  --session-id spike-test-1 \
  --allowedTools "Read,Edit" \
  2>/dev/null | head -50
```
Verify: Edit tool call appears with `file_path`, `old_string`, `new_string` in the
accumulated `input_json_delta` chunks.

**Test 2 — Hook permission control:**
Create a PreToolUse hook that returns `{"decision":"deny","reason":"test"}`.
Run a prompt that triggers Bash. Verify Claude respects the denial.

**Test 3 — Session resume in stream mode:**
```bash
claude -p "What did we just do?" \
  --output-format stream-json \
  --resume spike-test-1
```
Verify: Valid NDJSON output referencing prior conversation context.

**Test 4 — Multi-turn via piped stdin:**
Verify whether `-p` with stream-json supports follow-up prompts or requires
separate invocations. This determines the conversation model.

**Go/No-Go:** If any of tests 1-3 fail, reassess architecture.
If test 4 fails, design around single-prompt invocations with --resume.

---

### Phase 0: Rust Types & Stream Parser [1-2 days]

**What:** Build the typed NDJSON parser in Rust. Zero UI work — pure data.

**New file: `src/stream/mod.rs`**

```rust
// Core event envelope (every NDJSON line)
#[derive(Deserialize)]
pub struct StreamLine {
    #[serde(rename = "type")]
    pub line_type: String,  // "stream_event"
    pub event: StreamEvent,
}

// All event types from Claude API streaming spec
#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum StreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageMeta },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: u32, content_block: ContentBlock },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: Delta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: MessageDeltaBody, usage: Option<Usage> },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
}
```

**New file: `src/stream/parser.rs`**

```rust
// Reads lines from AsyncBufRead, deserialises, accumulates tool inputs
pub struct StreamParser {
    tool_inputs: HashMap<u32, (String, String)>,  // index → (tool_name, accumulated_json)
}

impl StreamParser {
    /// Parse one NDJSON line into a high-level RichEvent
    pub fn feed_line(&mut self, line: &str) -> Option<RichEvent> {
        let parsed: StreamLine = serde_json::from_str(line).ok()?;
        match parsed.event {
            StreamEvent::ContentBlockDelta { index, delta: Delta::Text { text } } => {
                Some(RichEvent::TextDelta { text })
            }
            StreamEvent::ContentBlockStart { index, content_block: ContentBlock::ToolUse { id, name } } => {
                self.tool_inputs.insert(index, (name.clone(), String::new()));
                Some(RichEvent::ToolStart { id, name })
            }
            StreamEvent::ContentBlockDelta { index, delta: Delta::InputJson { partial_json } } => {
                if let Some((_, acc)) = self.tool_inputs.get_mut(&index) {
                    acc.push_str(&partial_json);
                }
                None  // accumulating, don't emit yet
            }
            StreamEvent::ContentBlockStop { index } => {
                if let Some((name, json)) = self.tool_inputs.remove(&index) {
                    let input: serde_json::Value = serde_json::from_str(&json).ok()?;
                    Some(RichEvent::ToolComplete { name, input })
                } else {
                    None
                }
            }
            // ... other events
        }
    }
}

/// High-level events consumed by the GPUI layer
pub enum RichEvent {
    TextDelta { text: String },
    ToolStart { id: String, name: String },
    ToolComplete { name: String, input: serde_json::Value },
    ToolResult { id: String, output: String, is_error: bool },
    EditDiff { file_path: String, old_string: String, new_string: String },
    SessionStatus { status: SessionStatus },
    UsageUpdate { input_tokens: u64, output_tokens: u64 },
    Error { message: String },
}
```

**Key design:** Two-layer parsing. `StreamEvent` is 1:1 with the wire format (stable).
`RichEvent` is Allele's internal representation (can evolve independently).
The `StreamParser` transforms between them — this IS the protocol boundary the
council recommended.

**Integration point:** `RichEvent::ToolComplete` where `name == "Edit"` extracts
`file_path`, `old_string`, `new_string` from the JSON value and emits
`RichEvent::EditDiff`. This is where the raw API format becomes semantic.

---

### Phase 1: Rich Session Process [2-3 days]

**What:** A new session type that spawns the CLI in stream-json mode instead of PTY.

**New file: `src/rich_session.rs`**

```rust
pub struct RichSession {
    child: tokio::process::Child,
    events_rx: flume::Receiver<RichEvent>,
    session_id: String,
    status: SessionStatus,
}

impl RichSession {
    pub fn spawn(
        session_id: &str,
        prompt: &str,
        clone_path: &Path,
        hooks_settings: &Path,
        allowed_tools: &[&str],
    ) -> Result<Self> {
        let mut cmd = tokio::process::Command::new("claude");
        cmd.arg("-p").arg(prompt)
           .arg("--output-format").arg("stream-json")
           .arg("--session-id").arg(session_id)
           .arg("--allowedTools").arg(allowed_tools.join(","))
           .arg("--settings").arg(hooks_settings)
           .current_dir(clone_path)
           .stdout(Stdio::piped())
           .stderr(Stdio::piped())
           .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().unwrap();

        // Spawn background task to parse stdout into RichEvents
        let (tx, rx) = flume::bounded(256);
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut parser = StreamParser::new();
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(event) = parser.feed_line(&line) {
                    if tx.send_async(event).await.is_err() { break; }
                }
            }
        });

        Ok(Self { child, events_rx: rx, session_id: session_id.to_string(), status: SessionStatus::Running })
    }

    pub fn resume(session_id: &str, prompt: &str, /* ... */) -> Result<Self> {
        // Same as spawn but with --resume instead of --session-id
    }

    pub fn drain_events(&mut self) -> Vec<RichEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events_rx.try_recv() {
            events.push(event);
        }
        events
    }
}
```

**Integration with existing Session struct:**
```rust
pub enum SessionBackend {
    Pty(PtyTerminal),
    Rich(RichSession),
}
```

The `Session` struct gains a `backend: SessionBackend` field. All existing PTY
code continues to work unchanged. New rich-mode code is additive.

---

### Phase 2: Permission Hook Flow [2-3 days]

**What:** Bidirectional permission communication via file-based IPC.

**New hook script: `~/.allele/bin/permission-hook.sh`**
```bash
#!/bin/bash
# Receives: tool_name, tool_input via env vars from Claude Code
# Must output: {"decision":"allow"} or {"decision":"deny","reason":"..."}

SESSION_ID="$CLAUDE_SESSION_ID"
REQUEST_ID="$(uuidgen)"
PERMS_DIR="$HOME/.allele/perms/$SESSION_ID"
mkdir -p "$PERMS_DIR"

# Write request
cat > "$PERMS_DIR/$REQUEST_ID.request" << EOF
{"request_id":"$REQUEST_ID","tool_name":"$TOOL_NAME","tool_input":$TOOL_INPUT}
EOF

# Wait for response (poll every 100ms, timeout 120s)
TIMEOUT=1200
COUNT=0
while [ ! -f "$PERMS_DIR/$REQUEST_ID.response" ] && [ $COUNT -lt $TIMEOUT ]; do
    sleep 0.1
    COUNT=$((COUNT + 1))
done

if [ -f "$PERMS_DIR/$REQUEST_ID.response" ]; then
    cat "$PERMS_DIR/$REQUEST_ID.response"
else
    echo '{"decision":"deny","reason":"Permission request timed out"}'
fi

# Cleanup
rm -f "$PERMS_DIR/$REQUEST_ID.request" "$PERMS_DIR/$REQUEST_ID.response"
```

**Rust side: Permission watcher**
```rust
// In main event loop, check for new .request files
pub struct PermissionWatcher {
    session_id: String,
    perms_dir: PathBuf,
}

impl PermissionWatcher {
    pub fn poll(&self) -> Option<PermissionRequest> {
        // Read any .request files in the perms dir
        // Parse JSON, return structured PermissionRequest
    }

    pub fn respond(&self, request_id: &str, decision: PermissionDecision) {
        // Write .response file atomically (write to .tmp, rename)
    }
}
```

**hooks.json addition:**
```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Bash",
      "hooks": [{
        "type": "command",
        "command": "~/.allele/bin/permission-hook.sh"
      }]
    }]
  }
}
```

**Note:** Only tools that NEED approval go through hooks. Safe tools
(Read, Edit, Write, Grep, Glob) are pre-approved via `--allowedTools`.
This means the hook fires only for Bash and other dangerous tools.

**Spike dependency:** Phase S must confirm that PreToolUse hook return values
actually control tool execution. If they don't, this design needs rework
(fallback: MCP permission-prompt-tool, or process stdin).

---

### Phase 3: GPUI Rich Components [3-5 days]

**What:** The visible payoff. UI components that render RichEvents.

#### Component 1: RichView (container)

```rust
pub struct RichView {
    events: Vec<RenderedBlock>,
    scroll_offset: f32,
    session: Arc<Mutex<RichSession>>,
    permission_watcher: PermissionWatcher,
    active_permission: Option<PermissionRequest>,
}

enum RenderedBlock {
    Text(String),                           // Accumulated text
    ToolCall { name: String, input: String, collapsed: bool },
    Diff { file_path: String, old: String, new: String },
    ToolResult { output: String, is_error: bool },
    PermissionPrompt(PermissionRequest),    // Active, waiting for response
}
```

Polling loop (same pattern as TerminalView — 16ms cx.spawn):
1. `session.drain_events()` — get new RichEvents
2. Map events to RenderedBlocks, append to list
3. `permission_watcher.poll()` — check for permission requests
4. If permission pending, show modal overlay
5. `cx.notify()` to trigger repaint

#### Component 2: Permission Modal

```
┌─────────────────────────────────────────┐
│  ⚠ Permission Required                  │
│                                          │
│  Tool: Bash                              │
│  Command: rm -rf /tmp/old-builds         │
│                                          │
│  ┌──────────┐    ┌──────────┐           │
│  │  Allow   │    │   Deny   │           │
│  │ (Enter)  │    │  (Esc)   │           │
│  └──────────┘    └──────────┘           │
└─────────────────────────────────────────┘
```

- Rendered as GPUI overlay (absolute positioned, dimmed backdrop)
- Keyboard: Enter → allow, Esc → deny
- Calls `permission_watcher.respond()` on action
- Dismisses modal, resumes stream consumption

#### Component 3: Diff View

```
┌─────────────────────────────────────────┐
│  src/main.rs                             │
│  ─────────────────────────────────────── │
│  - fn old_function_name() {              │ (red bg)
│  + fn new_function_name() {              │ (green bg)
│      let x = 42;                         │ (grey, context)
│  }                                       │
└─────────────────────────────────────────┘
```

- Extracts old_string/new_string from Edit tool input
- Renders unified diff format using GPUI text elements
- Monospace font, red/green backgrounds for changes
- File path header with line context if available
- Character-level diff highlighting within changed lines (optional, v0.2)

#### Component 4: Streaming Text

- Appends text_delta tokens to current text block
- Basic markdown-like rendering: `**bold**`, `` `code` ``, headers
- Full markdown rendering deferred to v0.2 (start with styled monospace)
- Auto-scroll to bottom as new content arrives

---

### Phase 4: Integration & Mode Switching [2-3 days]

**What:** Wire everything together. Mode toggle. Fallback.

**Session creation flow:**
1. User creates new session (existing flow)
2. New option in session config: "Mode: Terminal / Rich" (default: Terminal)
3. Terminal mode: existing PTY path (unchanged)
4. Rich mode: spawn RichSession, show RichView instead of TerminalView

**Mode switching (same session):**
```
PTY → Rich:
  1. Kill PTY process (SIGTERM)
  2. Create RichSession with --resume <session-id>
  3. Replace TerminalView with RichView
  4. Rich mode gets conversation context from resume

Rich → PTY:
  1. Kill RichSession process
  2. Spawn PTY with `claude --resume <session-id>`
  3. Replace RichView with TerminalView
  4. Terminal shows full conversation history from resume
```

**Fallback on crash:**
- RichSession process exits unexpectedly
- Show notification: "Rich mode crashed. [Switch to Terminal] [Retry]"
- Switch to Terminal: PTY spawn with --resume
- Retry: new RichSession with --resume

**Settings:**
```json
{
  "default_mode": "terminal",  // or "rich"
  "rich_mode_allowed_tools": ["Read", "Edit", "Write", "Grep", "Glob"],
  "rich_mode_hook_tools": ["Bash"]
}
```

---

## Dependency Graph

```
Phase S (spike)
    │
    ▼
Phase 0 (types + parser)
    │
    ├──────────────┐
    ▼              ▼
Phase 1         Phase 2
(rich session)  (permission hooks)
    │              │
    └──────┬───────┘
           ▼
       Phase 3
    (GPUI components)
           │
           ▼
       Phase 4
    (integration)
```

Phases 1 and 2 can run in parallel after Phase 0.
Phase 3 needs both 1 and 2.
Phase 4 is integration/wiring.

## Timeline Estimate

| Phase | Duration | Dependency |
|-------|----------|------------|
| S: Validation Spike | 2 hours | None |
| 0: Types & Parser | 1-2 days | Spike passes |
| 1: Rich Session | 2-3 days | Phase 0 |
| 2: Permission Hooks | 2-3 days | Phase 0 (parallel with 1) |
| 3: GPUI Components | 3-5 days | Phases 1 + 2 |
| 4: Integration | 2-3 days | Phase 3 |
| **Total** | **~2-3 weeks** | |

## What This Does NOT Include (v0.2+)

- Full markdown rendering (v0.1 uses styled monospace)
- Subagent visualisation (show parallel agents)
- Token cost dashboard
- File tree of touched files
- Tool result collapsing/expanding
- Character-level diff highlighting
- Syntax highlighting in diff view (beyond red/green)
- Rich mode for drawer terminals
- Theme customisation for rich components

## Hard Constraint: Observability Parity

**Rich mode must never trade observability for aesthetics.**

Allele's core value is supervision — real-time visibility into what agents are
doing so humans can course-correct during execution. This is the workflow that
produces reliable results, as opposed to "YOLO mode" (blind delegation + hope).

The spike gate criterion is NOT "does stream-json work" but:
**"Does stream-json expose everything a supervising human needs to
course-correct during execution?"**

Specifically, rich mode must show:
- What every subagent is doing RIGHT NOW (not just final results)
- Intermediate tool calls (file reads, grep searches, bash commands)
- When agents are going down a wrong path (before they waste minutes)
- Option to drill into any agent's work on demand

If stream-json cannot provide subagent internals, the architecture must change.
Possible fallbacks:
1. **Hybrid overlay** — PTY remains the source of truth, rich components overlay
   on top (permission modals, diff views) without replacing the terminal
2. **Per-subagent streams** — If subagents have their own session IDs, spawn
   separate stream-json readers for each
3. **Don't ship** — If observability is compromised, rich mode stays in the lab

Progressive disclosure is acceptable (collapsed by default, expandable on demand).
Data loss is not.

## Open Questions

1. **Multi-turn in stream-json:** Does `-p` support follow-up prompts via stdin,
   or does each turn require a new process with `--resume`? If the latter,
   there's a cold-start cost per turn (~1-2s).

2. **Hook environment variables:** What exact env vars does Claude Code pass to
   PreToolUse hooks? Need `TOOL_NAME`, `TOOL_INPUT`, and `SESSION_ID` at minimum.
   Must verify during spike.

3. **Thinking blocks:** Does `--output-format stream-json` include thinking/reasoning
   blocks? If so, should rich mode display them? (Probably collapsed by default.)

4. **Subagent events:** Do subagent spawns appear in the stream? If not, rich mode
   won't know about parallel work — acceptable for v0.1 but needs investigation.

5. **Extended thinking tokens:** Are thinking tokens counted separately in the
   usage events? Relevant for cost display in v0.2.
