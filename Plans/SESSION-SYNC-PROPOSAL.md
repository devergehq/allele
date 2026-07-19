# Allele Session Sync — Cross-Machine Continuity

**Status:** Proposal (2026-07-18). No code changes yet.
**Problem:** A user who moves between Macs can't pick up their sessions on the
other machine — the session list and, more importantly, the Claude conversation
live only on the machine that created them.
**Scope (chosen):** Carry over **(a) the session list** and **(b) the Claude
conversation** so a session can be *resumed* on a second Mac. Uncommitted code is
**out of scope** — code moves via git. Transport is a **self-hosted store**
(S3/R2/MinIO or a small relay). This is *checkpoint/handoff* sync, not real-time
replication.

**Interaction model (decided).** Sync is **manual and per-session**, never
automatic and never whole-app:
- The unit of sync is **one session** ("push this session up" / "pull this
  session down"), not the app state. `state.json` is *never* transferred as a
  file — a pull **upserts one row** into the local index.
- It is **not backup/restore.** Mac A's hundreds of sessions/projects do not
  mirror to Mac B; only the session you explicitly move does.
- Sync is **project-gated**: a session only pulls if its project already exists
  on the target machine (see §2.6).
- Round-trips **replace, never merge** — a Claude transcript is a linear
  append-only history, so two divergent branches cannot be reconciled; one whole
  side wins, guarded by a divergence check (see §2.4).

---

## Part 1 — What a session actually is (grounded in the code)

A session is spread across four locations. Only two are in scope to sync.

| Piece | Location | In scope? | Portability problem |
|---|---|---|---|
| **Session metadata** — `PersistedSession` row (label, `claude_session_id`, `project_id`, `clone_path`, timers, pins, comment, `branch_name`) | `~/.allele/state.json` (`src/state/mod.rs`) | **Yes** | `clone_path` embeds an absolute home dir |
| **Claude conversation** — turn-by-turn transcript that `claude --resume` replays | `~/.claude/projects/<dashed-cwd>/<uuid>.jsonl` (+ `<uuid>/subagents/agent-*.jsonl`) (`src/transcript.rs:9`) | **Yes** | Keyed by *dashed cwd* = the clone path → differs per machine |
| **Workspace** — the working tree / code | `~/.allele/workspaces/<project>/<short_id>/` — APFS `clonefile(2)` clone (`src/clone/mod.rs:13`) | **No** (re-materialize from git) | `clonefile(2)` is volume-local; clones never transfer |
| **Live PTY** | in-memory | **No** | Ephemeral by definition |

### The two hard facts

1. **Machine-specific absolute paths.** Both `clone_path` (state.json) and
   Claude's transcript directory key (the *dashed cwd*, derived from the cwd,
   which *is* the clone path) embed the local home dir and workspace path. The
   same logical session has a *different* transcript key on each Mac. **Path
   rewriting is the actual engineering** — the file copying is trivial.

2. **APFS clones don't move.** `clonefile(2)` is a same-volume operation
   (`src/clone/mod.rs:130`). You never transfer the clone; on the target Mac you
   **re-materialize the workspace from git** at the recorded `branch_name`, then
   drop the transcript beside it.

### The lucky fact — resume already exists

`resume_session` (`src/session_ops.rs:842`) cold-resumes a `Suspended` session by:
1. requiring `clone_path` to **exist on disk**, then
2. running `claude --resume <claude_session_id>` **inside it**, gated by
   `claude_session_history_exists(&session_id)` (the local `.jsonl` must exist).

Rehydration from `state.json` already lands every session as `Suspended`
(`src/state/mod.rs`). **Cross-machine sync = make a session authored on device A
satisfy those same two preconditions on device B**, then let the *existing*
resume path fire. We are extending rehydration, not inventing a subsystem.

---

## Part 2 — Design

### 2.1 What lives in the store

Per session UUID, in the self-hosted bucket — a **session bundle**, the atomic
unit of sync:

```
sessions/<uuid>/meta.json         # normalized PersistedSession + sync header (see 2.2, 2.4)
sessions/<uuid>/transcript.jsonl  # the main Claude transcript
sessions/<uuid>/subagents/agent-*.jsonl
```

There is deliberately **no whole-`state.json` object** in the store — the bundle
*is* the granularity, and a pull upserts its `meta.json` as a single row into the
local `state.json`. Transcripts are **append-only** in practice, so pushes are
tail-deltas (byte offset → end), not whole-file re-uploads. `meta.json` is small;
push whole.

### 2.2 Path normalization (the core mechanism)

Never store absolute paths in the synced payload. Introduce a portable form and
rebase on import.

- **`clone_path`** → store as logical `{project_slug}/{short_id}` relative to the
  workspaces root. On import, rebase onto the *local* `~/.allele/workspaces`.
- **Transcript directory key** → recompute the *local* dashed-cwd from the local
  `clone_path` and place the `.jsonl` under the local `~/.claude/projects/...`.
- **In-transcript `cwd` fields** → each jsonl line carries the authoring
  machine's `cwd`. **RESOLVED by the Phase-0 spike (2026-07-18): no rewrite
  needed.** See §2.8. Placement under the correct local dashed-cwd dir is
  necessary *and sufficient*; the stale `cwd`/`gitBranch` fields are harmless
  historical metadata.

### 2.3 Transport — self-hosted store

**Pluggable backends (open/closed).** Every backend implements the `SyncStore`
trait (`put`/`get`/`list`/`delete`); nothing above the trait knows which one is
active. A config-driven selector picks the backend:

```rust
enum StoreConfig { S3(S3Config), /* Filesystem(..), Webdav(..) added later */ }
fn build_store(cfg: &StoreConfig) -> anyhow::Result<Box<dyn SyncStore>>
```

Only the **S3 adapter** is built now. Adding a NAS/filesystem/WebDAV target later
is a new enum variant + adapter file — no edits above the trait. (A tiny
`FilesystemStore` is worth adding early purely to keep the trait honest and to
cover "point it at a mounted NAS path".)

**S3 adapter — crate + auth (decided).** Use the **`rust-s3` crate** for
transport. Credentials are **never stored by Allele** — the user supplies a
**profile name** that Allele resolves via `awscreds::Credentials::from_profile(Some(name))`,
reading `~/.aws/credentials`. Allele is deliberately **unopinionated about how
credentials get into that file** — it only requires that they are *materialized*
there under the named profile:

- **Static IAM keys** → `aws_access_key_id` + `aws_secret_access_key`. SigV4. ✅
- **SSO / STS temp creds** (e.g. via `yawsso`, `aws configure export-credentials`,
  `aws-vault`) → the above **plus `aws_session_token`**. rust-s3 adds the
  `X-Amz-Security-Token` header. ✅

Same code path for both — rust-s3's profile reader picks up the session token when
present. This sidesteps rust-s3's one real gap: it can NOT resolve a *pure* SSO
profile (creds only in `~/.aws/sso/cache`, requiring a `GetRoleCredentials` call)
— but a materialized profile is just ordinary (temporary) keys, which it reads
fine. The user owns keeping the session valid (`aws sso login` / their yawsso
alias); Allele just consumes the profile.

**S3 provider config** (settings, all plain strings, no secrets):
`{ bucket_name, region, profile, endpoint? }`. rust-s3 addresses by bucket
**name + region**, not ARN. The user only ever supplies the **profile name** and
picks/types the **bucket**; the **region is auto-resolved** (see below), not typed.
`endpoint` + path-style is set only for non-AWS S3-compatible targets
(R2/MinIO/NAS), whose keys live in a named profile too — so the "profile name"
model is uniform across backends.

**Credential lifecycle:** resolve `from_profile` **fresh per operation** (a cheap
file read) so a re-auth is picked up without restarting Allele. On a 403 / expired
token, surface "credentials for profile `<name>` are missing or expired — refresh
them", never a raw S3 error.

**Connection validation + bucket discovery (config-time feedback loop).** The
settings flow validates the profile *at config time* instead of failing on first
sync — it front-loads the same calls sync would make anyway, and pinpoints where a
misconfiguration is (wrong profile / account / region / bucket / permission).

Critical IAM nuance: **`s3:ListAllMyBuckets` (the `ListBuckets` API — list every
bucket name in the account) is account-level and a least-privilege policy omits
it.** It is *not* the same as `s3:ListBucket` (list objects in one named bucket),
which sync *does* need. So bucket *discovery* must degrade gracefully:

1. **Discovery (convenience):** try `ListBuckets`. If allowed → show a **picker**.
   If AccessDenied (a properly scoped least-privilege key) → fall back to a
   **"type the bucket name"** field. Never a dead end.
2. **Authoritative validation (always):** on the chosen/typed bucket, run
   `ListObjectsV2(prefix = "allele/")` (or `HeadBucket`). This exercises the
   **exact permission sync uses** (`s3:ListBucket` on that bucket) — a more
   meaningful check than `ListBuckets`, which tests a broader permission sync never
   uses.
3. **Region auto-resolve:** send `HeadBucket` and read the **`x-amz-bucket-region`**
   response header — it returns the bucket's true region even when the request hit
   the wrong one. So a wrong-region setup self-corrects instead of failing silently.

Errors are surfaced *specifically* — invalid/expired profile vs. no-such-bucket vs.
access-denied vs. wrong-account — not as a raw S3 error.

*Implementation check:* confirm `rust-s3` cleanly exposes `ListBuckets`,
`HeadBucket`, and the region response header (it has `bucket.list()` for objects
and `bucket.location()` for `GetBucketLocation`; the all-buckets list + HeadBucket
header may need a raw signed request). Not a blocker — a fallback path exists.

**Encryption is orthogonal and always on.** Payloads are client-side encrypted
before `put` (DEV-189) regardless of transport creds, so bucket *contents* are
protected independent of who can reach the bucket. The encryption key is the only
secret Allele holds (macOS Keychain) — there are no stored AWS credentials.

**Push** is a **manual, explicit per-session action** ("Sync session up"), invoked
at a checkpoint. *No* auto-push on save / Stop-hook / quit — automatic sync is what
manufactures the divergence this design is trying to avoid. **Pull** is likewise
manual: a "session available from &lt;device&gt;" list the user browses, pulling a
chosen session on demand. A lightweight index of *available* bundles may refresh in
the background, but transcripts and materialization happen only on an explicit pull.

### 2.4 Versioning — revision + base, replace-never-merge

Because a manual per-session workflow *will* produce divergence (Mac A hands off
v1 → Mac B continues to v2b → meanwhile Mac A also continues v1 to v2a), the model
must **detect divergence and force an explicit choice**, never silently merge.
Modelled on git's fast-forward-vs-force distinction, but resolution is always
"pick one whole side."

**Sync header** in each `meta.json`:
- `revision` — monotonic integer, bumped on every push.
- `last_writer_device`, `updated_at` — for human display only.

Each device remembers, per session, the **`base_revision`** it last synced
(pulled or pushed). Divergence is then *computed*, not guessed from wall-clock
timestamps (which drift across machines):

- **Push (sync up):** if `remote.revision > local.base_revision` → the bucket
  advanced since you last synced → pushing would clobber the other machine's
  progress → **warn and require confirmation.** (The v2a-over-v2b case.)
- **Pull (sync down):** if `local.revision > local.base_revision` → you have local
  progress since last sync → pulling would discard it → **warn** ("your local
  version looks newer than what you're pulling — this replaces it. Sure?").
- Neither moved past the common base → clean fast-forward, no prompt.

**Resolution is replacement.** A pull overwrites the local `PersistedSession` row
(upsert by UUID) and the local transcript. **The losing side is archived, not
deleted** — rename the pre-sync transcript to `<uuid>.jsonl.pre-sync-<rev>` so a
mis-click is recoverable. `/clear` already forks `claude_session_id`
(`src/state/mod.rs:22`), so a cleared session syncs as a genuinely distinct
conversation and never collides with its pre-clear self.

*(A soft "active on &lt;device&gt;" lease is a possible Phase-3 nicety, but the
revision/base guard above is the actual safety mechanism and does not depend on
it.)*

### 2.5 Push preconditions (sync up from machine A)

The bundle carries the transcript but **not the code** (code = git, per scope).
The transcript continuously references file/line state, so if the branch isn't
pushed, machine B re-materializes a workspace *missing the code the conversation
assumes* — resume "works" but Claude sees a different codebase than the transcript
describes. Therefore "Sync session up" must, before uploading:

- Verify the session's branch has **no unpushed commits** and the tree is
  **clean** (or only expected runtime cruft) → warn/block otherwise.
- Treat "sync the session" and "push its branch" as **one ritual**. The push
  action can offer to `git push` the branch as part of the flow.

### 2.6 Project identity + the sync gate

A session only pulls if its **project already exists on the target machine**
(Mac B must have added the project first). The bundle records a project key; on
pull, Mac B matches it, and if absent, **blocks with "add project X first"** (or
offers to add it).

Today projects are keyed by **opened-folder name** (≈ repo name), which is fine
for the common case but fragile (two repos with the same folder name; renamed
folders; monorepo subdirs). **Cheap hedge, do it now:** also record the project's
**git remote URL** in the bundle as a sturdier secondary match key, so a later
move to robust project identity doesn't require a data migration. Match on remote
URL first, fall back to folder name.

### 2.7 Resume flow on machine B

1. **Browse available remote sessions** → pull a chosen one → its row upserts into
   the local index as `Suspended` (existing rehydration).
2. **On pull / resume** → ensure preconditions:
   - Project gate satisfied (§2.6), else block.
   - `clone_path` missing → re-materialize: `git` clone/worktree the project at
     `branch_name` into the local workspaces root. (Code returns via git.)
   - Pull `transcript.jsonl` (+ subagents) into the local
     `~/.claude/projects/<local-dashed-cwd>/`, archiving any pre-existing local
     transcript per §2.4.
3. **Existing `resume_session` fires** → `claude --resume <claude_session_id>` in
   the clone. No new resume logic.

### 2.8 Phase-0 spike results (2026-07-18) — the blocking risk is cleared

Ran on one machine, simulating machine B with a second directory at a different
path (`claude` 2.1.214, headless `-p`). Findings:

- **Resume is directory-scoped by `(dash_cwd(cwd), uuid)`.** Placing the
  `.jsonl` under `~/.claude/projects/<dash(macB)>/<uuid>.jsonl` and launching
  `claude --resume <uuid>` from macB loaded the full conversation history.
  Launching the same `--resume <uuid>` from a *third* dir with no transcript
  placed there returned **"No conversation found."** → **Import MUST compute the
  exact local dashed-cwd and drop the file there.** `dash_cwd` already exists at
  `src/transcript.rs:44` (rule: every non-alphanumeric char → `-`).
- **Stale internal `cwd`/`gitBranch` fields are harmless.** The copied transcript
  had all 90 `cwd` lines pointing at macA. On resume from macB, Claude recalled
  the prior turn correctly AND ran new file operations against the *actual* macB
  cwd (read `macB/foo.txt`, reported the macB path) — it did not follow the baked-in
  macA path. → **No field rewrite required for correctness.**
- **Cosmetic caveat:** the model can *see* the old machine's paths in history and
  may note the discrepancy. Rewriting `cwd`/`gitBranch` on import is optional
  polish, not a functional need.

**Design impact:** §2.2's transcript step is just "compute local dashed-cwd, copy
`.jsonl` in." No transcript transformation. Import stays trivial.

### 2.9 Encryption (DEV-189) — decided (2026-07-19)

Bundle payloads carry the actual conversation and must be unreadable to anyone who
can reach the bucket. Encryption is **client-side and always on**, independent of
transport creds.

**Placement — a decorator.** `EncryptingStore<S: SyncStore>` wraps any backend:
`put()` encrypts then delegates; `get()` delegates then decrypts. Everything above
(push/pull, ledger) sees a plain `SyncStore` and never touches ciphertext.
`build_store()` wraps the S3/Mem/FS store in `EncryptingStore`.

**Scope.** Object *payloads* are encrypted (`meta.json` now, transcripts in Phase 2).
Object *keys* (`sessions/<uuid>/…`) are **left visible** — a documented non-goal;
they leak only random UUIDs + a session count, no content. Obscuring them is
over-engineering for a personal bucket.

**Crate + scheme — `age`, two-tier key hierarchy.** Use `age` (vetted format: AEAD +
nonce/stream framing + versioning; streams large transcripts):
1. **Data identity** — a random age X25519 keypair = the encryption key. Objects are
   encrypted to its recipient (fast; no per-object password hashing).
2. **Passphrase** — wraps the data identity (age scrypt) into one bucket object,
   `keyring/identity.age`. Scrypt runs **once per device** at setup, not per object.

(Rejected: age passphrase-mode-per-object — scrypt per file, painful for Phase 2's
many transcript chunks. Rejected: hand-assembled RustCrypto primitives — more format
surface to get wrong.) Future path: age supports multiple X25519 recipients, so
per-device keypairs (revocable) can be added later without a format change.

**Key distribution — passphrase-wrapped in the bucket:**
- *Mac A (first):* generate data identity → cache in Keychain → wrap with the user's
  passphrase → upload `keyring/identity.age`.
- *Mac B:* enter the same passphrase once → fetch + unwrap `keyring/identity.age` →
  cache identity in Keychain. No passphrase re-entry after that.
- Per operation: identity read from Keychain (fast). Self-contained — no iCloud /
  Apple-ID dependency.

**Keychain:** store the identity via the `security-framework` crate — **already in
the dependency tree** (pulled by rust-s3's native-tls), so no new dep for key
storage. Item scoped to Allele, `WhenUnlocked`.

**Fail-closed:** wrong/missing key or tampered ciphertext → hard error, **never**
silent plaintext. Missing identity in Keychain → prompt to bootstrap (enter
passphrase), never fall back to plaintext.

**Tests:** encrypt→decrypt round-trip over `MemStore`; ciphertext ≠ plaintext
(no leakage); wrong-key fails closed; tamper detection.

---

## Part 3 — Phasing

- **Phase 0 — Spike.** ✅ **Done (2026-07-18, §2.8).** Confirmed `claude --resume`
  reattaches to a transcript relocated to a different path, with no field rewrite —
  as long as it sits under the correct local dashed-cwd dir.
- **Phase 1 — Manual push/pull of one session bundle.** "Sync session up" (with
  git precondition, §2.5) uploads a normalized `meta.json`; "browse remote
  sessions" lists available bundles; pull upserts one row → appears `Suspended`.
  Proves transport, encryption, path normalization, project gate. No transcript yet.
- **Phase 2 — Transcript sync + resume.** Tail-delta transcript push/pull; wire the
  re-materialize-from-git + drop-transcript preconditions into resume. Delivers the
  headline feature.
- **Phase 3 — Divergence guard + polish.** revision/base divergence prompts (§2.4),
  pre-sync archival, optional "active on &lt;device&gt;" lease, conflict UX.

---

## Part 4 — Risks to validate

1. ~~**`claude --resume` cwd tolerance** (blocking)~~ — **RESOLVED (§2.8).** Foreign
   `cwd` in history resumes cleanly; no rewrite needed. Placement under the local
   dashed-cwd dir is necessary and sufficient.
2. **Transcript size/churn** over the store — mitigated by append-only tail deltas.
3. **Secrets in transcripts** — mandatory client-side encryption before upload.
4. **Workspace re-materialization** assumes work is committed/pushed. Enforced by
   the §2.5 git precondition on push; still surface it in the UI so users aren't
   surprised by missing WIP.
5. **Store credentials handling** — Allele stores **no AWS credentials**; it holds
   only a profile-name string and resolves creds from `~/.aws/credentials` per
   operation (§2.3). The *only* secret in Keychain is the client-side encryption
   key (DEV-189). This shrinks DEV-188 to plain config strings.
6. **Project identity** — folder-name keying is fragile (§2.6). Recording the git
   remote URL now as a secondary key is cheap insurance against a later migration.
7. **Divergence UX** — the revision/base guard (§2.4) must present the choice in
   human terms ("local edited 3pm vs remote edited 4pm — keep which?"), and the
   losing side must be archived, not destroyed.
