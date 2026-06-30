# Projects & Session Lifecycle

How Allele models projects, how a session is materialised, and how the
per-project **startup** (session-start) and **shutdown** (session-end) hooks
run. This is the authoritative reference for the orchestration system; the
README carries only a short summary.

> **TL;DR**
> - A **project** is a source directory; each **session** is an APFS
>   copy-on-write clone of it under `~/.allele/workspaces/<project>/<session-id>/`.
> - Session orchestration (drawer terminals, startup/shutdown hooks) is
>   configured in **project settings** (`~/.config/allele/settings.json`, edited via the
>   Settings UI). A project-root **`allele.json`** is still supported and
>   **takes precedence** when present.
> - **startup** runs on session creation *and on every cold-resume*, before the
>   terminals open. **shutdown** runs *only when a session is discarded*.
> - Both hooks run off the UI thread; a slow or failing hook never freezes the
>   app and never strands a clone.

---

## Projects

A **project** is a registered source directory that hosts zero or more
sessions (`src/project/mod.rs`). Each session runs in its own APFS
`clonefile(2)` clone of the source, stored at:

```
~/.allele/workspaces/<project-name>/<session-id>/
```

Each project carries a `ProjectSettings` (`src/settings.rs`), persisted in
`~/.config/allele/settings.json`:

| Field                 | Purpose                                                                   |
|-----------------------|---------------------------------------------------------------------------|
| `default_branch`      | Override auto-detected default branch. `None` → detect, fallback `master`. |
| `merge_strategy`      | How session work is integrated back into canonical.                       |
| `rebase_before_merge` | Fetch + rebase canonical onto the remote tip before merging. Default on.  |
| `remote`              | Remote name for fetch/rebase. `None` → `origin`.                          |
| `terminals`           | Drawer terminals spawned for each session (see below).                    |
| `startup`             | Session-start hook command (see below).                                   |
| `shutdown`            | Session-end hook command (see below).                                     |

The bottom three (`terminals`, `startup`, `shutdown`) are the **session
orchestration** fields. They moved here from `allele.json` in the June 2026
work — `allele.json` predates this and still works as an override.

### Creating a session — branch selection

The **New Session** modal (`src/new_session_modal.rs`) has a **branch name**
field with a live hint that resolves three ways:

- **Empty** → a branch name is **auto-generated** (LLM-named from the first prompt).
- **Matches an existing local branch** → that branch is **checked out** in the
  clone (hint: `✓ existing branch — will be checked out`).
- **Anything else** → a **new branch** is created.

This lets you resume work on an existing branch in a fresh isolated clone, not
just start from the default branch.

---

## Configuration sources & precedence

Session orchestration is resolved from two sources, in this order
(`src/main.rs`, `src/session_ops.rs` — `ProjectConfig::load(...).or_else(from_settings)`):

1. **`allele.json`** in the **session clone root** — if present and parseable,
   it wins. (Because it's read from the clone, a session can override its own
   config by editing its copy.)
2. **Project settings** (`~/.config/allele/settings.json`) — the fallback, and the
   recommended place to configure orchestration. Edit it in the **Settings →
   project** pane (`src/settings_window.rs`).

A missing or malformed `allele.json` is silently ignored (a parse failure logs
one warning) and the project-settings values are used instead.

### Feature parity between the two sources

| Capability                       | Project settings | `allele.json` |
|----------------------------------|:----------------:|:-------------:|
| `terminals[]` (drawer tabs)      | ✅               | ✅            |
| `startup` hook                   | ✅               | ✅            |
| `shutdown` hook                  | ✅               | ✅            |
| `preview.url`                    | ❌               | ✅            |
| `agent` override                 | ❌               | ✅            |

`preview` and `agent` are currently **`allele.json`-only** — `from_settings()`
does not carry them, and the per-project agent override is read directly from
the project-root `allele.json` (`src/main.rs`). If you need a preview URL or a
per-project agent, use `allele.json`.

### `allele.json` example

```json
{
  "terminals": [
    { "label": "Server",   "command": "./bin/dev -p {{unique_port}}" },
    { "label": "Logs",     "command": "tail -f {{folder}}/log/development.log" },
    { "label": "Terminal", "command": "" }
  ],
  "preview": { "url": "http://127.0.0.1:{{unique_port}}" },
  "agent":   "claude",
  "startup":  "bin/setup",
  "shutdown": "docker compose down"
}
```

### Placeholders

Substituted in every terminal `command`, the `startup`/`shutdown` commands, and
`preview.url` (`config::substitute`):

- `{{unique_port}}` — a free TCP port (see [Port allocation](#port-allocation)),
  allocated once per session and shared by every occurrence, so the server tab
  and the preview URL always match. Left unsubstituted if no port is free.
- `{{folder}}` — the session's **clone path** (the APFS workspace for that
  session, not the original source). Use it for absolute paths in log/subprocess
  commands.

---

## Drawer terminals

Each `terminals[]` entry becomes a tab in the bottom drawer
(`spawn_terminals_and_preview`, `src/main.rs`):

- `label` — the tab's display name.
- `command` — piped into the tab's **interactive login shell** (`$SHELL`) as its
  first input, so it runs as if you typed it. Aliases, rc files, and job control
  (Ctrl+C / `bg` / `fg`) all work. When the command exits or you interrupt it,
  the shell stays so you can re-run or do anything else. An empty string leaves
  the shell at a bare prompt.

The drawer opens automatically when a session declares terminals. Switch tabs
with **Cmd+[ / Cmd+]**.

---

## Session-start hook (`startup`)

A one-shot command that runs **before** any drawer terminal or preview is
spawned (`src/main.rs`, `apply_project_config`).

- **When it runs:** on session **creation** *and on every cold-resume*
  (re-materialisation after an app restart). Make it **idempotent**.
- **Where:** via `sh -c` with the **session clone** as the working directory.
- **Threading:** runs on a background executor — never on the UI thread. Its
  stdout is streamed line-by-line to the session's **sidebar status** (e.g. a
  live "Installing dependencies…" indicator) while it runs.
- **Ordering:** terminals and preview wait for it to exit. Use it for
  `bin/setup`, dependency installs, booting a background daemon, or writing a
  Traefik route file the preview URL depends on.
- **Failure policy:** a non-zero exit logs a warning and the session **continues**
  to materialise. A failing hook never blocks the session.

---

## Session-end hook (`shutdown`)

A one-shot command that runs when a session is **discarded**
(`src/session_ops.rs`, the discard/archive pipeline).

- **When it runs:** **only on discard** — *not* on plain close or suspend, which
  keep the clone for later resume.
- **Ordering:** runs **before** the clone is archived and trashed, so
  `{{folder}}` still exists and the working directory is valid.
- **Threading:** the command string is *resolved* on the UI thread (a cheap
  config load + substitution), but the command itself is **executed off the UI
  thread** inside the background archive pipeline. This is deliberate — a slow
  teardown (`docker compose down`, dev-server shutdown, proxy-route cleanup)
  must not give the user a spinning beachball.
- **Failure policy:** a non-zero exit (or a spawn failure) logs a warning and
  teardown **continues**, so a broken hook can't strand a clone on disk.

Use it to tear down whatever `startup` brought up: stop containers, remove the
session's Traefik route file, free external resources.

---

## Script-path resolution

For both `startup` and `shutdown`, the command's first token is resolved
(`config::resolve_script_command`):

- Absolute (`/…`) or home (`~/…`) paths are used as-is.
- A relative path that looks like a script (contains `/` or ends in `.sh`) is
  resolved against the project's script directory:

  ```
  ~/.allele/projects/<project-name>/scripts/
  ```

- A bare command word (e.g. `docker`) is left untouched and resolved on `$PATH`.

This lets you keep per-project setup/teardown scripts outside the repo (so they
don't pollute the source) while still referencing them by short relative name.

---

## Port allocation

Allele hands each session one free local TCP port in **`40000..=49999`**
(`config::allocate_port`), shared by every `{{unique_port}}` in its config.

Allocation **skips ports already claimed** by other sessions. Two sources are
unioned (`src/main.rs`, `base_infra::registered_ports`):

1. **Durable Traefik route files** in `~/.allele/base-infra/traefik/dynamic/` —
   parsed for `host.docker.internal:<port>` backends. A **suspended** session
   keeps its route file (suspend doesn't run the session-end teardown) even
   though its dev server is no longer listening, so this is the durable record
   of ownership.
2. **In-memory ports** held by other live sessions in the current run.

A session **excludes its own** route file from the reserved set, so on resume it
reclaims the same port instead of reserving it against itself. If no port in
range is free, `{{unique_port}}` is left unsubstituted and a warning is logged.

> A plain TCP-bind probe alone would see a suspended session's port as free and
> hand it out again, colliding two sessions on one port. The route-file scan is
> what prevents that.

---

## Base infrastructure (Traefik reverse proxy)

Opt-in, toggled in **Settings** (`src/base_infra/mod.rs`). When enabled, Allele
manages **exactly one container and one network** to make multi-session HTTPS
routing work — and nothing more. It is **not** a general Docker orchestrator.

What Allele owns:

- The **`allele`** Docker network (external, shared).
- One **`allele-traefik`** container (Traefik v3.4), with:
  - a **file provider** watching `~/.allele/base-infra/traefik/dynamic/`, and
  - a **docker provider** for containers that self-register via labels.
- TLS certs in `~/.allele/base-infra/certs/`.

Scaffold (written once on first enable, **never overwritten** so your edits
survive):

```
~/.allele/base-infra/
├── docker-compose.yml          # the managed Traefik service (editable)
├── certs/                      # drop wildcard *.pem certs here
└── traefik/
    └── dynamic/
        ├── _middlewares.yml    # shared https-redirect + default-headers
        └── <session>.yml       # per-session route files (written by your startup hook)
```

**The contract:** Allele guarantees the network exists, Traefik is running, and
the dynamic dir is watched. **Your project's `startup` hook writes the
per-session route file** (pointing Traefik at `host.docker.internal:<port>`),
and your `shutdown` hook removes it. Allele does not manage databases, Redis,
Mailpit, or any project service — bring those up on the shared `allele` network
yourself (you can add them to the managed compose file, which Allele only ever
`docker compose up -d`s).

Everything degrades gracefully when Docker is unavailable; enabling the toggle
without Docker running surfaces a clear error rather than failing the app.

---

## End-to-end flow

**Create / resume:**

1. Resolve config: `allele.json` (clone root) → else project settings.
2. Allocate a `{{unique_port}}`, skipping ports other sessions own.
3. Run `startup` (off-thread, streaming status to the sidebar); wait for exit.
4. Spawn the drawer terminals (commands piped into interactive shells).
5. Open the preview URL (if `allele.json` declared one).

**Discard:**

1. Resolve the `shutdown` command on the UI thread.
2. Drop the session (kills the PTY process group).
3. Off-thread: run `shutdown` in the clone → archive the session branch into
   canonical (`refs/allele/archive/<id>`) → trash the clone.

(Plain **close** / **suspend** keep the clone and do **not** run `shutdown`.)
