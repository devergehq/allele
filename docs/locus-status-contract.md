# Agent status contract v1 — proposal

**Status:** proposal. **Owner:** `devergehq/locus`. **Consumer:** `devergehq/allele`.
**Contract version:** `agent.status/1`.

> This document lives in the Allele repo as the *proposal*. Once agreed it is mirrored into
> `devergehq/locus` and Locus becomes the authoritative owner; Allele's copy then links to it.
> Allele implements the consumer side only and must not extend the contract unilaterally.

## Why

`src/rich/narrative.rs` recovers the active Locus Algorithm phase by matching English words
(`OBSERVE`…`LEARN`) in model-generated assistant prose, and drives a sticky phase indicator
from it. That makes Locus's *prompt wording* an undocumented API between two repositories.
Slimming `CLAUDE.md`, renaming a phase, or making the Algorithm's visible output optional is
a silent cross-repository break with no type, version, or protocol to catch it.

Three properties fix that, and nothing less does:

1. **Framed** — the signal is a delimited JSON object, not a sentence. Zero false positives.
2. **Versioned** — the consumer gates on a major version and *says on screen* when it can't.
3. **Generic** — phase names are **data**, not a closed enum. Encoding the seven phases into
   the schema would reproduce the original bug in a new syntax.

## Non-goals

- Allele does **not** read `~/.locus/data`, or any Locus-owned path. That is the wrong seam:
  it couples a UI to another tool's on-disk layout and needs filesystem permissions Allele
  should not want. Everything the consumer renders arrives in the frame.
- The contract carries **no credentials, tokens, API keys, or absolute filesystem paths**.
  It crosses a process boundary into a UI that may render it verbatim; treat it as public.
- The contract does not describe the Algorithm. It describes *a producer's current status*.
  Locus is the first producer, not the only conceivable one — hence `agent.status`, not
  `locus.status`.

## Transport

The producer emits the frame as **an additional top-level key in the JSON object it already
writes to hook stdout**, from any Claude Code hook it already registers (see *Emission
points* for the recommended set). Claude Code surfaces hook stdout on the `system` / `hook_response`
stream line, which Allele already parses (`SystemEvent.stdout`, `src/stream/types.rs`). No new
IPC, no daemon, no polling, no filesystem contact, and no change required in Claude Code.

**Framing rule: the frame is a key *in* the hook response object, never a separate line.**
Hook stdout is a single JSON object with no trailing newline, and the hook protocol already
uses it — Locus's own `handle_pre_tool_use` returns a `decision` object that implements the
delegation guardrail, and `handle_session_start` returns injected context. Appending a second
object would produce `{…}{…}`, which is not valid JSON; Claude Code would reject the whole
payload and silently disable the handler's real work. That channel is precious. So:

```json
{"decision": {…},           "agent.status": {…}}
{"hookSpecificOutput": {…}, "agent.status": {…}}
{"agent.status": {…}}
```

The last form is for handlers that emit nothing today. Unknown top-level keys are ignored by
consumers that do not know them — the same convention Allele already applies to unrecognised
`SystemEvent` fields — so adding `agent.status` is invisible to Claude Code and to any other
reader of the hook protocol.

The consumer therefore parses the whole of `SystemEvent.stdout` as one JSON object and looks
for the single literal top-level key `agent.status` — one key whose name contains a dot, *not*
a nested `{"agent": {"status": …}}`. Stdout that is not a JSON object, or carries no such key,
is ignored and is not an error.

Consumers **must** bound the work this creates. A payload larger than **64 KiB** is skipped
without parsing, and a producer emitting frames faster than roughly **one per second** should
expect the consumer to coalesce them — only the newest is rendered. Consumers must also be
**idempotent on identical consecutive frames**: a hook registered more than once delivers the
same frame twice, and re-rendering must be a no-op rather than a flicker.

## Emission points

A producer emits from hooks it already registers. Which ones is a producer's choice — adding
or dropping an emission point is **not a contract change** — but the recommended set is:

| Hook | Emit | Why |
|---|---|---|
| `SessionStart` | **Yes** | Establishes producer-alive and contract version before anything else can be misread. |
| `PreCompact` | **Yes** | The one moment context pressure is *actionable* rather than merely observable: the context is about to be compacted and the user may still intervene, hand off, or let it ride. A frame here carrying `usage.context` is the most decision-shaped signal in the contract. |
| `PostToolUse` | **On change** | See below. |
| `Stop` | **Yes** | Final state. |
| `UserPromptSubmit` | Optional | Only if a turn boundary in the indicator is wanted. Deliberately left out of the recommended set; a producer can add it at any time. |

**`PostToolUse` producers SHOULD emit only on delta** — when `run.stage`, `run.progress` or
`run.status` actually changed since the frame last emitted. It is the hottest path in the
system and often already carries a `hookSpecificOutput` payload, so an unconditional frame is
pure overhead.

This is a **SHOULD, not a MUST**, and the split matters: a naive producer that emits on every
tool call must still be *correct*. Consumer-side idempotence on identical consecutive frames
is the correctness guarantee; producer-side delta emission is its efficiency partner. Neither
substitutes for the other.

## Envelope

```json
{"agent.status": {
  "contract": "agent.status/1",
  "emitted": "2026-08-30T12:04:11Z",
  "producer": {"name": "locus", "version": "1.1.0"},
  "session": {
    "id": "332f9e66",
    "parent": null,
    "backend": "claude-code",
    "model": "claude-opus-5"
  },
  "run": {
    "status": "running",
    "stage": {"id": "observe", "label": "OBSERVE", "ordinal": 1, "total": 7},
    "progress": {"done": 4, "total": 18, "label": "ISC"},
    "detail": "extended"
  },
  "usage": {
    "tokens": {"input": 120345, "output": 8123},
    "context": {"used": 128468, "limit": 1000000},
    "cost_usd": 0.42
  }
}}
```

### Fields

`contract` is the **only required field**. Every other field is optional at every level; a
consumer renders what is present and omits the rest. A frame carrying nothing but `contract`
is valid and means "producer alive, nothing further to report".

| Path | Type | Meaning |
|---|---|---|
| `contract` | string | `"<name>/<major>"`. Required. Gate on it — see *Versioning*. |
| `emitted` | string | RFC 3339 timestamp. Consumers may use it to discard stale frames. |
| `producer.name` | string | Free-form producer id, e.g. `"locus"`. Display only. |
| `producer.version` | string | Producer's own version. Display only; **not** the contract version. |
| `session.id` | string | Opaque session id, for **display only**. Not a path, and never used to route a frame — see *Attribution*. |
| `session.parent` | string \| null | Parent session id when this run is a delegate. |
| `session.backend` | string | Free-form, e.g. `"claude-code"`, `"opencode"`. |
| `session.model` | string | Free-form model identifier. |
| `run.status` | string | One of `idle`, `running`, `blocked`, `done`, `failed`. Unknown values render verbatim. |
| `run.stage.id` | string | Machine-stable slug, e.g. `"observe"`. |
| `run.stage.label` | string | **Human label the consumer displays verbatim.** Free-form. |
| `run.stage.ordinal` | integer | 1-based position, when the producer has ordered stages. |
| `run.stage.total` | integer | Total stage count, when known and fixed. |
| `run.progress.done` / `.total` | integer | Generic counter, deliberately unstructured — see below. |
| `run.progress.label` | string | What is being counted, e.g. `"ISC"`, `"tests"`, `"files"`. |
| `run.detail` | string | One short free-form line, e.g. an effort tier. |
| `usage.tokens.input` / `.output` | integer | **Cumulative** session totals, not deltas. Consumers must not sum successive frames. |
| `usage.context.used` / `.limit` | integer | Context window occupancy. |
| `usage.cost_usd` | number | **Cumulative** session cost in USD, not a delta. |

**`stage.label` is the load-bearing design decision.** It is an opaque string. Locus may
rename `OBSERVE`, add a ninth phase, drop to three, or stop having phases at all — the
consumer keeps working, because it never enumerated them. Consumers **must not** switch on
`stage.id` or `stage.label` values, and **must not** validate them against a known set.

Producers should keep `label` under 24 characters; consumers must truncate rather than let a
long label break layout.

**`run.progress` is deliberately a single flat counter**, for the same reason. `{done, total,
label}` renders as "4/18 ISC" without the consumer knowing what an ISC is, exactly as
`stage.label` renders without it knowing what OBSERVE means. Anything richer — nested
criteria, per-phase breakdowns, typed progress kinds — would be the coupling returning in a
new syntax, and would be the first thing to break when the producer changes its mind about
what it counts. A richer shape can arrive later *additively* under `agent.status/1`, once
something concrete needs it; starting minimal costs nothing, and starting rich cannot be
undone.

## Untrusted values

Every string in a frame is producer-controlled and, in practice, often model-generated.
A consumer renders these in its own UI, so it **must** treat them as untrusted text:

- strip C0/C1 control characters and ANSI escape sequences before display;
- collapse to a single line — a newline in `stage.label` must not become a layout break;
- truncate to a fixed budget (Allele: 24 chars for `stage.label`, 120 for `run.detail`)
  rather than clipping the surrounding view;
- never interpret a value as markup, a URL to fetch, or a path to open.

`run.status` values outside the listed set are rendered verbatim under the same rules rather
than rejected, so a producer can add a state without a major bump.

## Attribution

A frame describes **the stream it arrived on**. The consumer attributes it to the session
whose stdout carried it and must not use `session.id` to route a frame to a different
session — a subagent's hook output would otherwise overwrite its parent's indicator.
`session.id`, `session.parent`, `session.backend` and `session.model` exist so the consumer
can *display* provenance, not resolve it.

`emitted` is the producer's clock and may be absent, skewed, or non-monotonic across a resume.
A consumer may discard a frame whose `emitted` precedes the last accepted one, but must fall
back to arrival order whenever `emitted` is missing or unparseable.

## Versioning

The `contract` string is `<name>/<major>`. Consumers match **both** the name and the major —
`other.thing/1` is not this contract and is ignored as if it were not a frame. Having matched
the name, they gate on the major:

- **Major matches** (`agent.status/1`): accept. Unknown extra fields anywhere in the frame are
  **ignored, not errors** — this is how minor, additive evolution happens without a bump.
- **Major differs** (`agent.status/2`, or anything else): **reject the frame's contents, and
  surface the rejection visibly.** The consumer must render a distinguishable degraded state,
  e.g. a badge reading `Locus v2 — unsupported`. It must *not* render nothing: absence is
  already the "no producer running" state, and collapsing the two recreates the silent break
  this contract exists to prevent.
- **Malformed frame** (bad JSON, missing `contract`, wrong shape): same visible degradation,
  with a different reason string.

Rejection is **additive, not destructive**: a rejected frame must not clear a stage the
consumer already accepted. The last good frame stays on screen and the degraded badge appears
alongside it. Otherwise one stray frame from a newer producer blanks a working display — the
same silent failure in a new costume.

Adding an optional field, adding a `run.status` value, or adding a stage is a **minor** change
and does **not** bump the major. Removing or retyping a field, or changing the framing rule,
is **major** and does.

Both repositories name the supported version in their docs. Allele's supported major is
recorded in `src/rich/locus_status.rs` (`SUPPORTED_MAJOR`) and asserted by a test, so a
consumer-side drift shows up as a failing test rather than a blank pill.

## Fallback

A consumer that has seen no valid frame in a session may fall back to whatever heuristic it
used before — for Allele, prose phase-header parsing. The fallback:

- is **never** the primary path once a frame has been seen in the session;
- must be **attributed**, so a stage shown from prose is visually distinguishable from one
  shown from a frame (Allele tags the annotation `StageSource::Prose`);
- may be removed once a floor version of the producer is required.

## Producer checklist

- [ ] Emit the frame from an existing hook; do not add a daemon or a file.
- [ ] Emit at least at `SessionStart`, `PreCompact` and `Stop`; at `PostToolUse`, only on change.
- [ ] Include `contract`; treat everything else as best-effort.
- [ ] Never include credentials, tokens, or absolute paths.
- [ ] Bump the major only for removals, retypes, or framing changes.
- [ ] Reference this document and the current version in the producer's own docs.
