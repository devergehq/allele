# Allele Multi-Machine — Reach Your Sessions From Any Mac

**Status:** Proposal (2026-11-02). No code changes yet.
**Problem:** Sessions only exist on the Mac that runs them. Step away from the
desk and twenty running sessions are out of reach: you can't see which need
input, answer a prompt, or start new work, without enabling Claude Code remote
control on each session one by one.
**Scope (chosen):** A second Mac running Allele can **see** another Mac's
sessions, **attach** to one (view and type), and **create, interrupt, and
discard** sessions there. The host keeps owning everything. No files, transcripts,
or workspaces move.
**Transport (decided 2026-11-02):** [iroh](https://iroh.computer), built into the
binary. No extra software for the user to install, no Allele server to run.
**Quality bar:** a feature for daily use, not a proof of concept. Pairing and
security are part of the first version, not something added later.

**Relationship to Session Sync** (`Plans/SESSION-SYNC-PROPOSAL.md`). Sync
*moves* a session: push it up, pull it down, resume it elsewhere. Multi-machine
*reaches* a session where it already runs. Both stay. Sync covers "I'm taking
this onto a plane"; multi-machine covers "I'm at lunch and session 14 is asking
for permission".

---

## Part 1 — The model: the host owns, others attach

A session is a live `claude` process in a PTY, in an APFS clone, writing a JSONL
transcript (see the session-sync proposal, Part 1). None of that can be in two
places at once, and none of it needs to be. The machine that runs a session is
its **host**, and stays its only owner. Any other paired Mac is a **client**: a
window onto the host.

```
 Host (desktop)                                Client (laptop)
 ┌──────────────────────────┐                 ┌───────────────────────┐
 │ PTYs ×20  (owns)         │  status ───────►│ Sidebar: "Desktop"    │
 │ hook-event status        │  (always on)    │   ● 3 need input      │
 │ dispatch handler         │◄──── commands ──│ Machine picker        │
 │                          │  screen ───────►│ Attached session tab  │
 │                          │◄──── input ─────│ (only while viewing)  │
 └──────────────────────────┘                 └───────────────────────┘
```

Consequences, all deliberate:

- **Nothing to merge or reconcile.** One writer per session, always.
- **A session closed on the host disappears from every client.** Clients hold
  references, never copies.
- **The host being unreachable is a normal state.** Clients show the host's
  last-known list, greyed out, and say so. Nothing breaks.
- **Every Mac is both.** Your laptop hosts its own sessions and is a client of
  the desktop, and the other way round. There is no "server" Mac.

## Part 2 — What already exists (grounded in the code)

| Need | Already in Allele | Where |
|---|---|---|
| A command API for sessions | `Request` enum: `projects_list`, `sessions_list`, `sessions_status`, `sessions_create`, `sessions_discard`, `sessions_interrupt` | `src/dispatch/protocol.rs:27` |
| A server that answers it | `~/.allele/control.sock`, 0600, local only | `src/dispatch/server.rs` |
| Live per-session status | Hook events → Running / AwaitingInput / ResponseReady / Done | `src/hooks/`, `src/hook_events.rs` |
| "Who needs me" UI | Attention bar | `src/attention_bar.rs` |
| A screen to mirror | alacritty `Term` grid, 10,000 lines of scrollback, behind `Arc<FairMutex<Term>>` | `src/terminal/pty_terminal.rs:222` |
| Writing input to a session | `PtyTerminal::write(&[u8])` | `src/terminal/pty_terminal.rs:315` |
| An async runtime iroh can run on | Shared tokio runtime + GPUI bridge | `src/sync/rt.rs` |
| Local notifications | `show_notification` via `osascript` | `src/hooks/mod.rs:644` |

**Multi-machine adds a network transport to the dispatch API, a status feed, and
a screen mirror.** It doesn't change how sessions work.

## Part 3 — Design

### 3.1 Transport: iroh

iroh gives each endpoint an Ed25519 key pair. The public key is the endpoint's
address (its `EndpointId`). `connect(id)` returns an encrypted QUIC connection,
and iroh handles discovery, hole-punching through home routers, and falling back
to a relay when no direct path exists. Relayed traffic stays end-to-end encrypted.

- **Version:** iroh 1.x. 1.0 shipped 2026-06-15; crates.io showed 1.4.1 on
  2026-10-21. Pin a minor version and record it here.
- **Relays:** start on n0's public relays. Make the relay URL a setting, so a
  self-hosted `iroh-relay` is a config change rather than a code change. Known
  risk: n0 has published end-of-life dates for relays serving older iroh
  versions, so staying on n0's relays means keeping up with their release cadence.
- **A thin seam, no alternatives yet.** Everything above the transport speaks to
  a small `PeerTransport` trait: dial a peer, accept, open a stream. iroh is the
  only implementation. That leaves room for a later adapter (Tailscale, plain
  LAN) without building one now.
- **One ALPN:** `allele/remote/1`. Bump the number on any breaking protocol change.

### 3.2 Identity and pairing

- **Machine identity.** Each Allele data root generates a persistent secret key
  on first use, stored in the macOS Keychain, plus a machine name the user can
  edit (defaults to the computer name). A redirected instance (`ALLELE_HOME`)
  gets its own identity, so a test instance never impersonates the real one.
- **Pairing, once per pair of machines:**
  1. On the host: *Settings → Machines → Pair a new machine*. The host shows a
     short one-time code and a QR, valid for 5 minutes. The code encodes the
     host's `EndpointId` plus a single-use pairing secret.
  2. On the client: *Add machine*, then enter the code.
  3. The client connects and proves it knows the secret. **The host shows an
     approval prompt naming the client machine.** Nothing is trusted until a
     human clicks Approve on the host.
  4. Both sides store each other's `EndpointId` in a trusted-peers list
     (`~/.allele/peers.json`, 0600).
- **After pairing,** a connection is accepted only if the peer's `EndpointId` is
  in the trusted list. iroh authenticates the key during its handshake, so there
  are no passwords, accounts, or tokens.
- **Revocation:** *Settings → Machines → Remove*. Takes effect immediately, live
  connections included.

### 3.3 Security model

**This feature means a remote machine can type into a terminal that can run code.**
The bar:

- **Never expose `control.sock` on the network.** The remote listener is its
  own server. It translates an allow-list of wire operations into the same
  internal calls, the way `dispatch/handler.rs` does for the socket.
- **Permissions per peer:** `view` (status and screen) and `control` (input,
  create, interrupt, discard). **A newly paired machine gets `view` only.** The
  host grants `control` explicitly. A view-only client's input is dropped on the
  host, not hidden on the client.
- **Visible on the host:** when a client is attached, the host shows it (e.g. a
  "Laptop is viewing" chip on the session tab), so remote control is never silent.
- **Off switch:** a single "Allow remote connections" setting, off until the
  first pairing. Turning it off closes the listener.
- **Everything logged:** connects, attaches, and control operations go to the
  Allele log with the peer's machine name.
- **Unchanged:** Claude Code's own permission prompts still apply. A remote user
  answering a prompt is equivalent to the local user answering it, and the
  per-peer `control` grant is what authorises that.

### 3.4 The wire protocol

Per connection, one long-lived **control stream** (bidirectional, framed JSON),
plus one **screen stream** per attached session.

```
client → host   hello { protocol: 1, machine_name }
host   → client hello { protocol: 1, machine_name, permissions: ["view","control"] }

client → host   request { id, op: <dispatch Request> }       // reuse protocol.rs ops
host   → client response { id, result }

host   → client status_snapshot { sessions: [...] }          // on subscribe
host   → client status_changed { session_id, status, label, ... }

client → host   attach { session_id }        → host opens a screen stream
client → host   input { session_id, bytes }  // control only
client → host   detach { session_id }
```

**Reuse the dispatch `Request` and response types,** with one versioned wrapper
on top. Over time `control.sock` and the remote listener stay two servers that
share one handler. The wire types are already deliberately separate from the
internal ones (`protocol.rs`: "wire types, deliberately not the internal ones"),
which is exactly the property a network protocol needs.

### 3.5 Status feed

The cheap, always-on part, and the one that answers "does anything need me?"

- When a client subscribes, the host sends a snapshot of every session: id,
  label, project, status, attention kind, last activity. After that it sends a
  delta each time a hook event changes a session's status.
- On the client, a **Machines** section in the sidebar lists each paired host,
  with its sessions grouped as they are on the host, marked remote, and a
  reachability dot.
- **Remote sessions that need input join the client's attention bar,**
  labelled with the host name.
- The client raises its own local notification (`show_notification`) when a
  remote session moves to AwaitingInput, so the laptop on the lunch table
  chimes.

### 3.6 Screen mirroring: send screen state, not bytes

alacritty's `EventLoop` owns reading from the PTY and feeds the bytes straight
into `Term` (`src/terminal/pty_terminal.rs:274`). Allele never sees the raw
output stream.

**Decision: mirror screen state.** The host sends a snapshot of the `Term`
grid (cells, attributes, cursor, plus scrollback on request) when a client
attaches. After that it sends diffs of changed rows, at most ~30 per second and
merged while the connection is slow. The client renders them with the existing
grid element, read from a mirrored grid instead of a local `Term`.

This is the approach mosh uses, and the reasons transfer:

- **Late join is free.** Attaching mid-session gets the current screen at once,
  with no byte history to replay.
- **A slow or dropped connection self-heals.** Diffs merge, so a stalled link
  catches up to the present instead of replaying a backlog.
- **No terminal emulator runs on the client.** The host's `Term` is the only
  emulator, so the two machines can't parse escape sequences differently.

**Rejected: tee the raw PTY bytes.** It would mean wrapping alacritty's PTY
type to copy reads, replaying from a ring buffer on attach, and running a second
`Term` on the client. It looks smaller at first, but late join, slow links, and
any mismatch between the two emulators all become problems to solve.

### 3.7 Input and window size

- **Input:** keystrokes and pastes go to the host as `input { bytes }` and
  through `PtyTerminal::write`, exactly like local typing. Only peers with
  `control` can send it.
- **Size:** the host's terminal size stays authoritative. The client renders the
  host's grid at its own font size, scrolls or letterboxes when its window is
  smaller, and shows a **"Fit to this window"** action that resizes the host
  PTY. When the host user interacts again, the size returns to the host's window.
  No automatic size changes.

### 3.8 Remote session management

Using the existing ops over the control stream:

- **New Session gets a machine picker** (default: this Mac). On a remote host
  the project list comes from that host's `projects_list`.
- **Interrupt and discard** in the remote session's context menu, `control` only.
- Dispatch admission (`src/dispatch/admission.rs`) applies unchanged on the
  host. A remote create counts against the host's cap like any other.

### 3.9 Reachability and sleep

- **The host has to be awake.** While any session is Running or AwaitingInput,
  Allele holds an `IOPMAssertion` that prevents idle sleep (system sleep, not the
  display), and releases it when everything is idle. This is useful on its own
  and ships in Phase 0.
- **Clients reconnect** with backoff after sleep or network changes. The host
  shows as "unreachable since 13:04" and its sessions stay listed, greyed out,
  until it returns.

## Part 4 — Phasing

| Phase | Delivers | Useful on its own? |
|---|---|---|
| **0 — Away, today** | Keep-awake while sessions run; one-click "Away" that turns on Claude Code remote control for every running session | Yes. Solves the lunch case with no networking. |
| **1 — Foundation** | iroh spike → machine identity → pairing + trusted peers → remote listener with the wire protocol | No user-visible feature yet, but pairing is done |
| **2 — See** | Status feed, Machines sidebar, remote attention + notifications | Yes. You know what needs you from any Mac. |
| **3 — Attach** | Screen mirroring, remote input, attach UI + size policy | Yes. The core feature. |
| **4 — Drive** | Machine picker in New Session, remote interrupt/discard | Yes. Full remote management. |

Tracked in the Linear project
[Multi-Machine](https://linear.app/deverge/project/multi-machine-b394db1ef154),
one milestone per phase:

| Phase | Tickets |
|---|---|
| 0 | DEV-1156 keep-awake · DEV-1157 Away |
| 1 | DEV-1158 iroh spike (go/no-go) → DEV-1159 identity → DEV-1160 pairing → DEV-1161 remote listener |
| 2 | DEV-1162 status feed → DEV-1163 Machines sidebar · DEV-1164 remote attention + notifications |
| 3 | DEV-1165 screen mirroring → DEV-1166 remote input · DEV-1167 attach UI |
| 4 | DEV-1168 machine picker + remote interrupt/discard |

## Part 5 — Out of scope (deliberately)

- **Moving sessions.** That's Session Sync.
- **A shared file system or remote file browsing.** The Reader tab stays local.
  A remote Reader is a possible later phase.
- **Phone or web clients.** The protocol is designed so one could be built later
  (iOS can link iroh via its FFI bindings), but nothing here builds one.
- **Push notifications to a phone.** That needs a push service, which conflicts
  with "no third-party opt-in". The client Mac's own notifications cover the
  laptop case.
- **A Tailscale or LAN adapter.** The `PeerTransport` seam allows one later.
- **Multi-user sharing** (pairing with someone else's Mac). The model would
  allow it, but the permission UI is designed for one person's own machines.
- **Linux hosts.** Blocked on the clone layer, not on this feature.

## Part 6 — Risks to validate

| Risk | How we find out | Where |
|---|---|---|
| iroh adds unacceptable binary size, build time, or dependency conflicts with GPUI/rust-s3 | Phase 1 spike measures them | Spike ticket |
| iroh 1.x stability (regressions reported after 1.0) | Spike runs a 24-hour connection between two Macs across sleep/wake | Spike ticket |
| Typing `/remote-control` into a busy session misbehaves | Phase 0 ticket tests idle, mid-turn, and at-prompt cases before shipping | Away ticket |
| Locking `Term` to snapshot it competes with local rendering | Measure lock hold time under a heavy-output session | Mirroring ticket |
| Diff bandwidth under heavy output (e.g. `cargo build` scrolling) | Measure bytes/sec with frame merging on | Mirroring ticket |
