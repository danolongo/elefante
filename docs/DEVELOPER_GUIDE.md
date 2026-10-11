# Elefante developer guide

This guide explains the code that exists today, the contracts between crates, and the
invariants a contributor must preserve. Read [`PLAN.md`](./PLAN.md) for the longer-term
architecture and [`MILESTONES.md`](./MILESTONES.md) for delivery status.

## 1. Current scope

The repository is at M3, plus M3.1 and the memory-facing parts of M5, M6 and M7 (see `MILESTONES.md`): a provider-backed coding loop behind a daemon, driven by either a
headless client or an OpenTUI terminal client, with a per-repository index that gives each turn
relevant context and gives the model a search tool, and durable notes learned from conversations.
The implemented vertical slice can:

- send a prompt and stable tool catalog to OpenRouter;
- stream reasoning and text, separated end to end;
- assemble provider events into replayable conversation history that persists across turns;
- execute model-requested tools concurrently, and cancel them mid-flight;
- enforce path, shell, and output-scrubbing policy, suspending for approval when the shell
  classifier escalates;
- serve many clients per session and many sessions per client, with resume after disconnect;
- index the repository in the background — files, symbols, imports, and files that change
  together — and keep that index current as turns edit it;
- add an ephemeral block of relevant files, and of notes learned in earlier sessions, to each turn,
  and a short repository brief to each session, none of which enters history;
- ask a model, in the background, what is worth remembering from a conversation, and keep only what
  survives verification (quote present, wording safe, evidence from the right speaker) — after the
  person has said yes, once, to a question that names the model and what is sent;
- show and control that memory from the clients: a `/memory` panel and `/forget <id>` in the TUI, a
  transcript line for each note learned or used, `memory:` lines on a headless terminal's stderr,
  and `elefante memory` going through the daemon when one is running; and
- continue the conversation until the provider ends the turn, refuses, is interrupted, or
  reaches a configured round limit.

Every gate through M3 is green, hermetic and credentialed. The credentialed tests for M0–M2 have been run: the
OpenRouter cache hit, the live repository edit, and `live_memory_context_reaches_the_model`, which
proves a real provider accepts the injected message shape and that a real model uses the index it is
sent. M3's, `live_learned_memory_is_extracted_stored_and_used`, which proves a real provider accepts
the extraction request, that a real model's reply passes the store's verification, and that the note
reaches a later prompt, passes against a live provider (it failed once first; see `MILESTONES.md`). CI runs
the hermetic gates on every push and the credentialed tests behind `workflow_dispatch`; see
`.github/workflows/`.

## 2. Design principles

The implementation follows four project-level convictions:

1. **Keep standing instructions small.** If a behavior can be enforced by a type, policy,
   gate, or test, enforce it there. The core prompt describes intent; it does not pretend to
   be a filesystem or process sandbox.
2. **Normalize at boundaries.** Core code works with provider-neutral requests, content,
   events, usage, and stop reasons. OpenRouter JSON and SSE details stay in the provider crate.
3. **Make unsafe paths fail closed.** Filesystem aliases are resolved before access, shell
   syntax is classified across the full parse tree, and unclassified mutation suspends for
   an explicit approval rather than proceeding.
4. **Treat milestones as gates.** A later milestone cannot use incomplete earlier behavior
   as accepted debt. The complete earlier gate must remain green.

The result is intentionally more explicit than a one-file agent prototype. Provider wire
formats, orchestration, policy decisions, and syscalls change for different reasons and need
separate test seams.

## 3. Repository map

```text
elefante/
├── Cargo.toml                     workspace and shared dependency/lint policy
├── Cargo.lock                     reproducible binary dependency resolution
├── README.md                      contributor and user entry point
├── docs/
│   ├── PLAN.md                    design of record and milestone gates
│   ├── MILESTONES.md              completion record
│   └── DEVELOPER_GUIDE.md         this guide
└── crates/                     every crate carries its own README.md
    ├── cli/                       `elefante` terminal binary
    ├── core/                      provider/tool conversation loop
    ├── provider/                  normalized model API + OpenRouter transport
    ├── guard/                     path, shell, and secret-output policy
    ├── tools/                     schemas and syscall-facing tool executor
    ├── rpc/                       wire envelopes and framing
    ├── srv/                       daemon, session registry, ring
    └── mem/                       repository index: graph, indexer, retrieval
```

All crates inherit Rust 2024 edition, the minimum Rust version, dependency versions, and
workspace lints from the root manifest. `unsafe` code is forbidden. Clippy's `all` and
`pedantic` groups are warnings locally and are promoted to errors by the documented check.

## 4. Crate responsibilities and dependency direction

### The system as it stands

Three processes, one socket, one outbound connection. Section 5 traces a single turn through
this; here it is as structure — which crate lives where, and what crosses each boundary.

```text
  ── clients ──────────────────────────────────────────────────────────────────

   elefante  (bin)                        tui/  (Bun + OpenTUI)
   ├─ elefante-cli    StreamRenderer      ├─ protocol.ts   framing, seq window
   │                  stdout = answer     ├─ session.ts    pure reducer, no I/O
   │                  stderr = reasoning  └─ ui/*.tsx      components
   └─ elefante-rpc    wire types             (speaks the wire; links no Rust)
             │                                        │
             └───────────────────┬────────────────────┘
                                 │  NDJSON, one value per line, ≤ 8 MiB
                                 │  Unix socket inside a 0700 directory
  ── daemon ─────────────────────▼─────────────────────────────────────────────

   elefante-srv  (bin)
   ├─ server.rs        listener · session registry · ring 4096 · dispatch
   ├─ queue.rs         per-client queue 256; coalesces same-method deltas only
   ├─ credentials.rs   0700 directories, 0600 key file
   └─ stub.rs          scripted Provider for tests; validates tool pairing
             │
             ├──► elefante-rpc ──► elefante-provider   re-exports StopReason, Usage
             │                                         so wire and loop cannot drift
             │
             └──► elefante-core     Session · turn loop · ToolGate · JoinSet
                       │            owns history across turns
                       │
                       ├──► elefante-guard      PathPolicy · BashClassifier · Scrubber
                       │                        decides; never performs
                       │
                       ├──► elefante-tools ──► elefante-guard
                       │         │             7 schemas; every path through the
                       │         │             policy, every result scrubbed at
                       │         ▼             one exit point
                       │    filesystem · bash -lc in its own process group
                       │
                       └──► elefante-provider   Provider trait · SSE decoder
                                 │              normalized Event stream
                                 ▼
                            HTTPS + SSE ──► OpenRouter

  ── the index ───────────────────────────────────────────────────────────────

   elefante-mem        <repo>/.elefante/{memory.db, cache.db}
   ├─ indexer          walk · hash · extract · graph · imports · co-change
   ├─ retrieve         FTS5 seeds · graph hops · token-budget packing
   └─ brief            ~300-token repository summary
        used by core (memory.rs: context + memory_search) and opened by srv
        (one Store and one Indexer per repository, shared by every session)

Three things are visible here that a dependency list cannot show.

**The wire has two independent implementations.** A Rust client and a TypeScript client speak
it, sharing no code — which is why the contract lives in `elefante-rpc` and section 5 rather
than inside either client, and why a change to it lands on both sides or lands broken.

**There are exactly two egress points.** Everything that leaves this system leaves through
`tools` (filesystem and `bash`) or `provider` (HTTPS). Both sit below `core`, and `guard` sits
beside them depending on nothing — so the policy layer is exercisable with no filesystem, no
network, and no subprocess, and there is no third path to audit.

**The daemon is the only process that holds state.** Clients render; they do not remember.
Kill every one of them mid-turn and the ring, the history, and the running tools are untouched.

### Dependency direction

The internal graph, precisely:

```text
elefante-guard      ──► (nothing)      pure policy; the graph's floor
elefante-provider   ──► (nothing)

elefante-tools      ──► guard
elefante-mem        ──► guard                        (Scrubber on every stored string)
elefante-rpc        ──► provider                     (re-exports StopReason, Usage)
elefante-core       ──► guard, mem, provider, tools
elefante-srv        ──► core, mem, rpc, provider, tools   the daemon; top of the graph
elefante-cli        ──► rpc                          a client: it does not link core
```

Two edges are worth stating outright, because both look like mistakes and are not. **`cli`
depends only on `rpc`** — it talks to the daemon over a socket and runs no turn in process,
which is what makes "reconnect and the turn completes intact" true. And **`rpc` depends on
`provider`** solely to re-export `StopReason` and `Usage`, so the wire and the agent loop cannot
drift apart; the cost is that `rpc` is not a true leaf.

Each crate carries a `README.md` with its module map, entry points, invariants, and how to test
it alone. The subsections below are the overview; the READMEs are the detail.

### `elefante-cli`

The binary is a client. It parses arguments and environment defaults, spawns `elefante-srv` if
none is listening, and then either streams one headless turn over the socket (a prompt was
given) or launches the Bun TUI (none was). It links only `elefante-rpc` — it constructs no
provider, no `ToolExecutor`, and no `RunConfig`; those live in the daemon. The answer goes to
stdout and reasoning plus tool activity to stderr, which is what resolves issue #4 — the two can
no longer render as one undelimited run, by construction rather than by formatting.

Keep provider protocol parsing, policy, and orchestration out of this crate.

### `elefante-core`

Core owns conversation semantics:

- building provider-neutral requests;
- consuming normalized streaming events;
- retaining assistant content in provider order;
- validating tool-use lifecycle and IDs;
- executing a batch of tools concurrently while retaining deterministic result order;
- returning every tool result together as one batch; and
- enforcing refusal and round-limit behavior; and
- placing per-turn repository context in a request — as an ephemeral `user` message after the
  turn's user message, never `system`, never stored in history (`ContextSource`, `memory.rs`).

Core does not know OpenRouter field names, does not know how a result batch is shaped on the
wire, and does not perform filesystem operations.

### `elefante-provider`

The crate has two layers:

- `lib.rs` defines the normalized provider contract and shared data models.
- `sse.rs` owns the transport-generic SSE decoder.
- `openrouter.rs` maps that contract onto `POST {base}/chat/completions`, drives the decoder,
  and maps wire chunks back to normalized events.

Capability differences are descriptive. A transport may emit only fields supported by the
selected model. Capabilities are resolved per model by prefix-matching the OpenRouter slug,
with a conservative all-false default for unrecognized slugs: an unknown model must not
silently receive fields it cannot handle. Update the resolver's tests whenever supported
model families change.

### `elefante-guard`

Guard decides whether an operation is acceptable. It does not execute commands or write
files. The crate exposes:

- `PathPolicy` and `PolicyError`;
- `BashClassifier`, `BashDecision`, and severity-ordered `Verdict`; and
- `Scrubber`.

Keeping decisions separate from syscalls makes security behavior directly unit-testable.

### `elefante-tools`

Tools is the syscall boundary. It owns the seven permanent tool schemas and a cloneable
`ToolExecutor`. Every filesystem operation resolves through `PathPolicy`; bash must pass the
classifier; every success and error result passes through `Scrubber`.

`memory_search` is a trait (`MemorySearch`) defined here and implemented in `core`, so this crate
still depends only on `guard`. Without an implementation attached it returns one fixed refusal.

Do not add an alternate read, write, process, glob, or search path that bypasses this crate's
policy calls.

### `elefante-rpc` and `elefante-srv`

Both shipped in M1 and are covered by its gate. `rpc` holds the wire types shared by daemon and
client; `srv` is the daemon itself — socket, session registry, ring, and bounded delivery.
Section 5 is the contract they implement. M2 added no method, notification, or error code; the
memory additions (the `memory` result field, three events, six methods, two error codes) are
listed under **Memory wire** in §5. `srv` additionally owns the lifetime of each repository's index (below).

### `elefante-mem`

The repository index. The crate README is the detail; this is the contract other crates rely on.

**Storage.** `<repo_root>/.elefante/` holds `memory.db` (the graph: `node`, `edge`, `import_ref`,
`node_fts`, `meta`) and `cache.db` (`file_state`, a content-addressed `extraction` cache, `brief`),
both WAL, directory `0700`, files `0600`, and a self-ignoring `.gitignore` written *before* either
database so `git status` is clean at every instant. `repo_root` is the nearest ancestor of a
session's working directory containing `.git` (a directory or a worktree file), else the directory
itself; `/` and `$HOME` are refused.

**Identity.** `node` is keyed by `(kind, key)`: a file by its repository-relative path, a symbol
by `path::qualified_name` (`#2`, `#3` for duplicates). The indexer upserts, so ids are stable
across edits, and it touches only rows whose `source = 'index'` — learned rows (`source =
'learned:*'`) are never read, changed, or swept by it.

**Consistency between the two files.** They cannot share a commit. A random *pairing epoch* ties
`cache.db` to the `memory.db` it was written against (a mismatch clears `file_state` but keeps the
content-addressed extractions); each batch commits to the graph first and the cache second; and an
orphan sweep after a complete walk reconciles deletions. Deleting either database converges on the
same graph.

**The pipeline.** Walk (honours ignore files beneath the root, skips symlinks, binaries, files
over 1 MiB, and anything `PathPolicy::is_secret_path` flags — unread) → stat comparison with a
racy-timestamp rule → `blake3` of what changed → extraction by content hash (tree-sitter for Rust
and TypeScript/TSX, a heading scan for Markdown) → batched graph writes → import edges derived from
stored raw imports and the set of paths → co-change edges from `git log`, only when `HEAD` has
moved. Edge derivation is flagged by a persistent `meta.edges_dirty`, written in the same
transaction as the change that makes edges stale.

**What is stored.** Names, signatures cut off before any body or initializer with string literals
elided, one doc sentence, line numbers. Never source. Everything passes `Scrubber`.

**Retrieval.** Prompt words → FTS5 seeds (rank-based) → two hops over `depends_on` and
`co_edited_with`, damped near hubs → packed by file into 1,500 estimated tokens → rendered as an
escaped, framed block that says it is reference data. `None`, not an empty envelope, when nothing
matches. `partial` means the first full index has not finished; a refresh on a complete index is not
partial. Results are cached by generation, from an in-process mirror of it, so a hit never reaches
the reader thread.

**Integration.**

- *Per turn:* `core` asks a `ContextSource` for a block once per turn and inserts it immediately
  after the turn's user message in every round's request. It is not stored. A leading brief message
  is adopted once and frozen. Both are `user`-role: repository text never has system authority.
- *`memory_search`:* `{query, limit?, kind?}` → ranked `path:line  kind name — description` lines.
- *Lifecycle:* `srv` opens one `Store` and `Indexer` per repository root when the first session
  starts beneath it, and calls `Indexer::refresh()` at session create, turn start and turn end.
  `refresh()` returns immediately and coalesces; N triggers become at most two runs.
- *Switch:* `ELEFANTE_MEMORY=0` (`DaemonConfig::memory`) — nothing opened, nothing written, no
  context added, `memory_search` refuses.

**Learned memory.** `preference`, `convention`, `decision` and `fact` nodes in `memory.db`
(`source = 'learned:explicit' | 'learned:stated' | 'learned:assistant'`, plus `status`,
`last_reinforced_ns` and a `mac`). A model proposes; `mem::learn` disposes. The crate README has the
detail; the contract is:

- *Input.* `core::learn::Learner` reads only the text of what the user and the assistant said —
  never tool results, tool calls or reasoning — scrubbed, behind a gate (a cue, enough text, or
  enough turns) with a watermark that advances only after a commit. It runs at turn end, on session
  close, after `ELEFANTE_LEARN_IDLE_SECS` of quiet, and as the daemon exits.
- *Verification.* Each candidate's quote must be in the excerpt; its wording may contain no command,
  URL, path or identifier the user did not write; it must survive the `Scrubber` unchanged; and the
  speaker of the evidence assigns its origin. Assistant-only claims are stored `pending` and never
  shown until the user confirms them. Only the user's words reinforce, promote or replace a note.
- *Dedupe.* Near-identical wording (Jaccard ≥ 0.9, same negations) is a duplicate whatever the model
  said; a model verdict is honoured only if verified; a contradiction supersedes, it does not merge.
- *Integrity.* Both databases are stamped and every learned row carries a MAC, keyed by
  `~/.elefante/memory.key`. An unstamped or foreign store is discarded and rebuilt; a failing row
  is treated as absent; retrieval renders only `source = 'index'` rows and verified learned rows.
- *Decay.* Confidence halves every half-life since last reinforcement, computed on read; below 0.1 a
  note is no longer retrieved and compaction removes it.
- *Presentation.* A separate `<repo-memory>` section, at most 400 estimated tokens, framed as
  unverified notes. A query with no notes renders exactly what it did before they existed.
- *Controls.* `elefante memory list | forget | approve | clear | learn on|off`. They go through the
  daemon (`memory.*` by `cwd`) when one is listening and open the store directly when none is.
  `list` checks the stamp first and prints only an explanation for a foreign store.
- *Display.* Every string a person or a client sees (all columns, every `Note` on the wire) goes
  through `learn::display_safe`, which removes Unicode categories Cc, Cf, Zl and Zp and collapses
  whitespace. The same function strips candidates at ingest, so notes are stored clean; older
  rows are cleaned at display.
- *Consent.* Learning asks first (see **Memory wire** in §5). Until a person answers, the daemon
  never calls `Learner::observe`, so nothing is read or held, and a later "yes" is not retroactive.
- *Switches.* `ELEFANTE_LEARN` unset asks, `1`/`on` is the operator's consent, `0`/`off` stops
  extraction alone; `ELEFANTE_MEMORY=0` stops everything.
  `ELEFANTE_EXTRACT_MODEL` and `ELEFANTE_EXTRACT_EFFORT` choose the extractor, defaulting to the
  session's model at low effort.

## 5. Runtime interaction

Elefante runs as a **daemon plus clients**. The `elefante` binary is a client; `elefante-srv`
holds the sessions. A client that finds no daemon spawns one and waits for it to bind, so
this is invisible in normal use — but the split is what makes reconnect, interrupt, and
multiple simultaneous clients possible at all.

```text
elefante (headless)      TUI client (bun)
        │                       │
        └───────┬───────────────┘
                │  newline-delimited JSON over a Unix socket
                ▼
          elefante-srv ── session registry
                │           ├─ ring (4,096 events)  ← resume
                │           └─ per-client queue (256) ← backpressure
                ▼
          core::Session ── history across turns
                │
                ▼
      Provider::stream ──► OpenRouter request ─► SSE response
        ▲                                        │
        └──────── normalized Event stream ◄──────┘
                │
                ├─ reasoning/text delta ─► notification ─► every attached client
                └─ stop_reason == tool_use
                          │
                          ▼
                  concurrent tool batch (JoinSet, cancellable)
                          │
             ┌────────────┴─────────────┐
             ▼                          ▼
       guard decision              tool syscall
             │                          │
       Escalate ─► approval.requested   │
             └────────────┬─────────────┘
                          ▼
                    scrub every result
                          │
                          ▼
            all tool results, consecutively
                          │
                          └──────────────► next provider round
```

### Transport

Unix domain socket at `$XDG_RUNTIME_DIR/elefante.sock`, falling back to
`~/.elefante/run/elefante.sock`. `ELEFANTE_SOCKET` overrides the path, and test isolation
depends on it.

**Access is enforced by a `0700` parent directory, created before the socket.**
`UnixListener::bind` applies the process umask and there is no safe API to bind with an
explicit mode; the umask route needs `libc::umask`, which is `unsafe`, and the workspace sets
`unsafe_code = "forbid"`. Chmod-after-bind leaves a window. A private directory closes it
with no `unsafe`.

**Framing is newline-delimited JSON** — one value per line, `\n` terminated, UTF-8, no
embedded raw newlines (`serde_json` escapes them inside strings, which is what makes the
framing safe). A line longer than 8 MiB is a protocol error and closes the connection.

**JSON-RPC-*shaped*, not JSON-RPC 2.0 conformant.** Requests and responses follow the 2.0
member layout, but notifications carry top-level `seq`, `turn_id`, and `session_id`, which
strict 2.0 does not permit. That is deliberate — they are routing and ordering metadata every
consumer needs before it looks at `params` — but it means **an off-the-shelf JSON-RPC library
will not round-trip our notifications**. Both sides hand-roll the envelope.

### Sequencing

```jsonc
{ "session_id": "s1", "seq": 413, "turn_id": "t7", "method": "text.delta", "params": { } }
```

- **`session_id` is mandatory on every notification.** A client may attach to several sessions
  on one connection and `seq` is per-session; without it a merged stream reads `413, 7, 414, 8`
  and trips the gap rule on every alternation.
- `seq` is **monotonic per session**, starting at 1, never reused, never reset — across turns
  and across reconnects. `session.create` returns 0, so the first event is 1.
- `turn_id` groups events; "the third event of turn 7" is counted client-side, so there is no
  second counter to keep in sync.
- Events outside a turn carry `turn_id: null`. `memory.status` and `memory.learned` always do, even
  when a turn happens to be running: they belong to the session, not to that turn.

A gap in `seq` means the client missed something and must resynchronise. It must never be read
as "nothing happened."

### Method set

Client → server:

| Method | Params | Result |
|---|---|---|
| `session.create` | `{ cwd, model?, effort?, max_rounds?, max_tokens? }` | `{ session_id, seq, memory? }` |
| `session.prompt` | `{ session_id, text }` | `{ turn_id }` — notifications follow |
| `session.interrupt` | `{ session_id }` | `{}` |
| `session.attach` | `{ session_id, since?: number }` | see **Attach and resume** |
| `session.close` | `{ session_id }` | `{}` |
| `session.resolve_approval` | `{ session_id, approval_id, decision: "allow" \| "deny" }` | `{}` |
| `key.set` | `{ api_key }` | `{}` — first-run setup |
| `memory.consent` | `{ answer: "learn" \| "off" }` | `{ consent }` — see **Memory wire** |
| `memory.list` | `{ cwd, all?: bool }` | `{ notes: [Row] }` |
| `memory.forget` | `{ cwd, id }` | `{}` |
| `memory.approve` | `{ cwd, id }` | `{}` |
| `memory.clear` | `{ cwd }` | `{ removed }` |
| `memory.status` | `{ cwd }` | `MemoryStatus` |

`session.create` validates `cwd`: it must exist, be a directory, and be canonicalisable. A
long-lived daemon that roots a `ToolExecutor` at any path a caller names is a larger blast
radius than a CLI confined to `env::current_dir()`.

`session.close` exists because sessions otherwise accumulate for the daemon's lifetime, each
holding a ring plus full history. Closing is idempotent and cancels a live turn first.

Server → client notifications:

| Method | Params |
|---|---|
| `turn.started` | `{}` |
| `reasoning.delta` | `{ text }` |
| `text.delta` | `{ text }` |
| `tool.started` | `{ tool_call_id, name, input_preview }` |
| `tool.finished` | `{ tool_call_id, is_error, output_preview, duration_ms }` |
| `approval.requested` | `{ approval_id, command, rule }` |
| `approval.resolved` | `{ approval_id, approved }` |
| `turn.finished` | `{ stop_reason, usage }` |
| `session.error` | `{ message }` — turn ended abnormally, always followed by `turn.finished` |
| `memory.status` | `MemoryStatus` — session event, `turn_id: null` |
| `memory.learned` | `{ note, change, replaced? }` — session event, `turn_id: null` |
| `memory.context` | `{ notes }` — carries the turn's `turn_id` |

`approval.requested` carries **no `reason`**: `BashDecision` has `verdict` and `rule` only, and
a prose reason is the M4 triage-critic verdict.

`approval.resolved` is required by fanout — with several clients attached, one resolves the
prompt and the rest must dismiss their modal.

**`duration_ms` is execution time only.** It starts when the tool begins running, so time spent
queued for or waiting on an approval is not counted. A call that never ran (denied by policy or
by the user, or interrupted while waiting for approval) reports `0`; one interrupted while running
reports how long it ran.

Reasoning *replay* state (`ReasoningDetails`) is never sent to clients. It exists so the next
request can echo it verbatim; rendering it would present internal replay tokens as output.

**`stop_reason` is a union.** `StopReason` derives `rename_all = "snake_case"`, so
`Other(String)` serialises as the **object** `{"other": "..."}`, not a string:

```jsonc
"end_turn" | "tool_use" | "max_tokens" | "refusal" | "stop_sequence"
  | "interrupted" | { "other": string }
```

**`usage`** carries all six fields, always present, `cost` nullable — `cost: null` means *not
reported*, which is distinct from `0.0`:

```jsonc
{ "input_tokens": 0, "output_tokens": 0, "reasoning_tokens": 0,
  "cached_input_tokens": 0, "cache_write_tokens": 0, "cost": 0.0 | null }
```

**Previews are cut at 2 KiB** on a UTF-8 character boundary, never mid-codepoint, with the
removed span replaced by exactly `\n…`.

**Previews are tool-aware.** A `read` is previewed as just the path it was given
(`input_preview: "src/x.rs"`, falling back to the raw input if `path` is missing or not a string),
and a successful `read` sends an empty `output_preview`; a failed one keeps its error text. The
model still receives the whole file. Every other tool previews its input as JSON in which each
string value is clipped to 512 bytes (the strings, not the serialized text, so a long `write` body
cannot cut the JSON in half and clients can always parse it), and its output as text.

### Attach and resume

`session.attach` has three outcomes and a client must handle all three:

1. **Resumed** — `since` is still inside the ring:
   `{ "kind": "resumed", "from": 413, "events": [ ... ] }`
2. **Snapshot** — `since` omitted, or older than the oldest buffered event:
   `{ "kind": "snapshot", "seq": 981, "snapshot": { ... } }`
3. **Unknown session** — a normal JSON-RPC error.

`since` is **exclusive**: `since: 412` yields `from: 413`. `events[]` elements are complete
notification envelopes, identical to live delivery — one shape, one parser.

**The snapshot case must never be silent.** A daemon that answers a stale `since` with recent
events produces a transcript with an invisible hole.

**The daemon holds notification delivery on a connection from the moment an `attach` for that
session arrives until the response has been written.** Without that rule a notification at
`seq: 415` can arrive before a result carrying 413–416, and a client deduping on a high-water
mark discards 413 and 414 as already-seen. **Client-side dedupe is therefore a seen-set over
the resume window, not a watermark** — a watermark cannot distinguish "already applied" from
"arrived out of order."

Both `resumed` and `snapshot` results also carry the optional `memory` object (see **Memory wire**).

The snapshot is materialised state, not an event log: `session_id`, `seq`, `cwd`, `model`,
completed `turns[]`, and `active_turn` (null when idle). Tool entries use **one shape
everywhere** — `{ tool_call_id, name, state, is_error }` — in both, so the reducer never
branches on where it found the value. `state` is `"running" | "ok" | "error" | "cancelled"`.

### The ring and the two buffers

These are different things, and conflating them breaks resume.

**Session ring** — 4,096 events, the source of truth for resume. Stores events *as emitted*,
each with its own `seq`, and is **never coalesced**, because `since: 412` must address a real
event.

**Per-client output queue** — 256 events, where backpressure is handled. Under pressure it
coalesces adjacent deltas *of the same method*, and a merged event carries **`seq_from` and
`seq_to`** rather than a single `seq`:

```jsonc
{ "session_id": "s1", "seq_from": 495, "seq_to": 500, "turn_id": "t7",
  "method": "text.delta", "params": { "text": "..." } }
```

The range is what distinguishes a merge from a drop. Carrying only the highest `seq` would make
every coalesce look like a gap and trip the client's own resynchronise rule during normal
operation.

**Coalescing never crosses channels.** A `reasoning.delta` is never merged into an adjacent
`text.delta`. Merging across channels would destroy the reasoning/answer distinction *in the
transport*, and only under load — so issue #4 would reappear as an intermittent defect no
render-site fix could reach. A queue fed `reasoning, reasoning, text, text` emits **two**
events, not one. Discrete events — `tool.started`, `tool.finished`, `approval.requested`,
`turn.finished`, and the three `memory.*` events — are never merged or dropped.

Coalescing only helps on runs of the same method, so a turn interleaving
`text, tool.started, text, tool.finished` coalesces nothing and a hung client on that workload
falls through to eviction. That is a known limit: merging across a discrete event would reorder
it, and reordering `tool.started` against the text explaining it is worse than dropping a client
that has stopped reading.

When a queue overflows even after coalescing, the daemon **closes the connection** and forgets
the client. It does not send `session.error` first — a client whose queue is overflowing cannot
receive it, and `session.error` promises a following `turn.finished`, which is not true here.
The connection closing *is* the signal; the session is unaffected.

### Session lifecycle

- **Prompt** takes the session's turn lock. A second prompt while a turn is live is **rejected,
  not queued** — silent queueing makes an interrupted session behave unpredictably.
- **Attach/detach** is many clients per session and many sessions per client. Detaching changes
  nothing about execution.
- **A turn continues with zero clients attached.** If every terminal dies mid-turn, tools keep
  running and events keep landing in the ring. That is what makes "reconnect and the turn
  completes intact" achievable.
- **Idle exit** happens after an idle period with no clients *and* no active turns; checking
  only the client count would make the daemon kill its own work. Defaults to 15 minutes, set by
  `ELEFANTE_IDLE_SECS`.
- **Interrupt** cancels the in-flight provider stream and aborts the tool `JoinSet`. Tools that
  already completed still return their results, so every `tool_use` keeps exactly one matching
  `tool_result` — an unpaired id makes the next request 400. The turn ends with
  `turn.finished { stop_reason: "interrupted" }`.

### Error model

Standard JSON-RPC error objects, application codes in the `-32000` range:

| Code | Meaning |
|---|---|
| `-32001` | Unknown session |
| `-32002` | Turn already in progress |
| `-32003` | `since` exceeds the session's current `seq` — the client claims events that do not exist. A `since` that merely predates the ring is **not** an error; it returns a snapshot. |
| `-32004` | No API key configured — client should prompt and call `key.set` |
| `-32005` | Unknown approval id, or approval already resolved |
| `-32006` | `UNKNOWN_NOTE`: `memory.forget` or `memory.approve` named a note that is absent, is not a learned note, or (for approve) is not pending |
| `-32007` | `MEMORY_OFF`: a `memory.list`, `forget`, `approve`, `clear` or `status` for a `cwd` whose repository has no usable store — `ELEFANTE_MEMORY=0`, no repository root (`/`, `$HOME`), or the store could not be opened |

Transport and provider failures *inside* a turn are not RPC errors — the request already
succeeded. They arrive as `session.error` followed by `turn.finished`.

### Memory wire

Everything the learned-memory feature adds to the protocol. It is additive: nothing above changed
shape, and a client that ignores it loses only the memory UI.

**State is truth, events are hints.** What a client must rely on — consent, note counts, whether
learning is working, cumulative memory spend — is the `memory` object, which arrives in the
`session.create` result and in **both** `session.attach` result kinds (`resumed` and `snapshot`),
as a sibling of `kind`. A lost event costs a missing transcript line, never a wrong total. The
field is optional: a daemon that omits it means "no memory UI", and a client must show nothing.

```jsonc
"memory": {
  "enabled": true,            // ELEFANTE_MEMORY is on and this repository's store opened
  "consent": "granted",       // "unset" | "granted" | "declined" | "off"
  "learning": "working",      // "working" | "paused" | "failing"
  "extractor": "provider/model",   // null unless learning could run in this session
  "notes": { "active": 12, "pending": 1 },   // learned notes in the repository's store
  "index": "ready",           // "off" | "building" | "ready"
  "spend": { /* Usage: this session's cumulative extractor spend, cost null until reported */ },
  "problem": null             // fixed vocabulary below, or null
}
```

- `consent`: `granted` and `declined` are the person's answer for **this extractor model**;
  `unset` means no answer (or one made for a different extractor, or an unreadable file); `off`
  means the operator switch is off (`ELEFANTE_LEARN=0`, or `ELEFANTE_MEMORY=0`). `unset` with
  `extractor: null` means the session has nothing to ask about (memory unavailable for it); a
  client asks only when `consent` is `unset` **and** `extractor` is not null.
- `learning`: `working` only when memory is enabled and consent is `granted`; `failing` when, in
  addition, the learner has failed three times in a row (cleared by the next success); `paused` is
  every other state, including `declined` and `off`, so read `consent` for why. In `paused`
  **no conversation text has been read or kept**, and a later grant is not retroactive: a turn is
  read only if consent was granted when it began and still is when it ends.
- `problem` is never the provider's text. It is one of `"the provider refused the request (HTTP
  <status>)"`, `"no reply from the provider"`, `"the reply could not be read"`, `"the memory key is
  unavailable"` (also reported when memory is on but the store did not open), or `"memory was
  reset: it was not made by this installation"` (the store was rebuilt; cleared when the next turn
  ends). The status code is the only provider-supplied datum.
- `spend` is per session and cumulative, so displayed cost is the client's turn total plus
  `spend.cost`, and a snapshot restores it exactly. There is no usage event.
- `notes` and `index` describe the repository store, which sessions in one repository share.

**Events.** Three notifications, in the ordinary ring and sequence space:

| Event | Params | When |
|---|---|---|
| `memory.status` | `MemoryStatus` | Any field of the session's status changed: after an extraction commits, spend or health changes, a turn ends (this clears the rebuilt-store `problem`), or a `memory.consent` answer arrives (sent to **every** live session). Not sent at create or attach: those return the status. |
| `memory.learned` | `{ note, change, replaced? }`; `change` is `added`, `promoted`, `superseded` or `revived`; `replaced` is the retired note for `superseded` | After an ingest, one per changed note (at most 8 per extraction). A **reinforced** note emits nothing. `turn_id` is always `null`. A `memory.status` follows when the counts changed. |
| `memory.context` | `{ notes }` | At turn start, directly **after** `turn.started`, only when learned notes were injected into that turn. It carries the turn's `turn_id`. |

`note` is `{ id, kind, status, statement, origin }`; every field has already passed
`learn::display_safe` (no control, bidirectional or zero-width characters). A `pending` note
(only the assistant vouched for it) is a valid `memory.learned` payload but is never used by the
model until approved, and the shipped clients draw no line for it.

**`memory_note_ids`.** Both `CompletedTurn` and `ActiveTurn` in a snapshot carry the ids of the
learned notes injected into that turn. The field is a list of integers, empty when none, and is
omitted by the Rust types when empty; read an absent field as empty. The daemon fills it from
`memory.context`, so a client attaching late can still say a turn used notes.

**Lenient clients.** A notification with an unrecognised `method` is not an error. The Rust
`NotificationEvent` deserialises it as `Unknown { method }` (never serialised), the TypeScript
parser accepts it, and both ignore it while still counting its `seq`. The Rust parser still
requires an object `params`, a `turn_id` and a valid sequence. New events therefore never need a version field
and share the one sequence space, so `seq` has no holes for any client.

**Methods.** The `memory.*` methods other than `consent` take a `cwd`, not a `session_id`: they work
with no session open, and after the session that learned a note has closed. `cwd` is validated like
`session.create`'s and resolved to a repository root; the daemon opens that repository's store on
demand, without starting an indexer refresh.

| Method | Result and behaviour |
|---|---|
| `memory.list` | `{ notes: [Row] }`; `Row` is a `Note` plus `confidence` (effective, after decay), `verified` and `reinforcements`. `all: true` includes superseded notes. |
| `memory.forget` `{cwd,id}` | `{}`. `UNKNOWN_NOTE` if the id is absent or not a learned note. Index rows can never be forgotten through it. |
| `memory.approve` `{cwd,id}` | `{}`. Promotes a pending note. `UNKNOWN_NOTE` if there is no such pending note. |
| `memory.clear` `{cwd}` | `{ removed }`. Deletes every learned note and leaves the code graph alone. |
| `memory.status` `{cwd}` | A `MemoryStatus`: the live session's if one in that repository exists, else a fresh one for the default extractor. |
| `memory.consent` `{answer}` | `{ consent }`, the resulting state. User-level, not per repository: it persists, applies to every live session at once, and emits `memory.status` to each. It targets the extractor of the **last session this connection created or attached** (a fresh connection targets the configured default extractor), so a connection with none gets `-32602`. When the operator switch decides (`ELEFANTE_LEARN` is `1`/`on` or `0`/`off`), it changes nothing and returns `off` or `granted`. An answer that revokes consent discards any text the learner was holding. |

All of these answer `MEMORY_OFF` (`-32007`) when the repository has no usable store, except
`memory.consent`, which does not need one. A mutation advances the store's generation, so a
`forget` or `clear` affects the next retrieval; statements already in a ring or on a screen are
not recalled.

**Consent.** `ELEFANTE_LEARN` unset means *ask*: consent comes from `~/.elefante/memory-consent`,
beside the credentials and the memory key. It holds `{ answer, extractor, stamp }`, the stamp a
keyed MAC over answer and extractor made with `memory.key`. A missing, malformed or unstamped file
reads as `unset` (fail closed), and so does one whose `extractor` differs from the session's
current one. `1` or `on` is the operator's consent (headless, CI): no file, no question. `0` or
`off`, or `ELEFANTE_MEMORY=0`, turns learning off and nothing is read; any other value means ask.
An operator setting always beats the file. Before
this wire existed learning was on unless `ELEFANTE_LEARN=0`; `ELEFANTE_LEARN=1` restores that.

### Round construction

`run_agent_loop` begins history with one user `Message`. Each round sends the same system
prompt and deterministic tool catalog plus accumulated history. `RunConfig` defaults to:

| Setting | Default |
|---|---:|
| Effort | `high` |
| Maximum output tokens | `16,384` |
| Maximum provider rounds | `24` |
| System prompt | `DEFAULT_SYSTEM_PROMPT` |

`receive_round` consumes the stream. `RoundAccumulator` retains content blocks in arrival
order, coalesces adjacent text deltas, tracks tool IDs, accumulates incremental JSON input,
and rejects incomplete or duplicate tool calls.

### Tool dispatch

When the stop reason is `tool_use`, core runs every call with `join_all`. The futures are
polled concurrently, while the returned vector preserves request order. Each failure remains
a `ToolResult` with `is_error: true`; dropping a failure would desynchronize tool IDs on the
next provider request.

Core then appends every `ToolResult` block from the batch together, before the next assistant
turn. This is a protocol invariant, not a formatting preference: splitting a result batch
across turns trains the model out of parallel tool calls. Section 7 describes how the provider
fans that batch onto the wire.

### Refusals

A refusal ends the invocation without dispatching pending tools. If the provider refuses
while a tool block is incomplete, core discards that partial block before normal tool-input
validation. Refusal text and incomplete replay state are not retained.

## 6. Data models

### Provider contract

`Provider` is object-safe, thread-safe, and deliberately small:

```rust
pub trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> Capabilities;
    fn stream(&self, request: Request)
        -> BoxStream<'static, Result<Event, ProviderError>>;
}
```

The call returns a stream immediately; the HTTP request is performed inside that stream.
This lets core consume every provider through the same asynchronous interface.

### `Capabilities`

Six independent flags describe what may be sent or observed:

| Field | Meaning |
|---|---|
| `adaptive_thinking` | The model reasons before answering. |
| `effort` | The model accepts `reasoning: { effort }`. |
| `prompt_caching` | Stable system/tool cache breakpoints may be sent. |
| `mid_conversation_system` | `true` for OpenRouter — an OpenAI-shaped `messages` array accepts a `system` role at any position. No core caller yet. Repository context is deliberately an ephemeral `user` message rather than a `system` one, so its first consumer is M4, where critic findings are harness-authored and `system` is correct. |
| `structured_output` | Strict schemas may be requested. |
| `cache_metrics` | Cache creation/read usage is observable. |

Core must degrade behavior based on capabilities rather than matching model names. Within a
single provider this matters more, not less: model variance inside OpenRouter is wider than
the cross-provider variance it replaced.

### Request and conversation types

`Request` contains a `ModelId`, `max_tokens`, system blocks, sorted tool definitions,
conversation messages, and optional `Effort`.

`Message` has a `Role` and ordered `ContentBlock` values. Current content variants are:

| Variant | Purpose |
|---|---|
| `Text` | Visible assistant or user text. |
| `Reasoning` | Ordered `ReasoningDetail` values, preserved exactly for replay. |
| `ToolUse` | Tool ID, name, and parsed JSON input. |
| `ToolResult` | Matching tool-use ID, content, and error flag. |

Reasoning blocks deserve special care. A `ReasoningDetail` keeps the model's original object
verbatim in `raw`, alongside a `kind` discriminator (`reasoning.text`, `reasoning.summary`,
`reasoning.encrypted`); `display_text()` extracts human-visible text where a block carries any,
and returns `None` for encrypted blocks. A typed enum was rejected deliberately, because an
unknown future `reasoning.*` type must round-trip rather than fail to parse.

`ReasoningDelta` is display-facing, but `ReasoningDetails` events carry replay state. Core
stores those complete blocks in the assistant message without rendering them. Reordering,
editing, or dropping these blocks can invalidate a tool continuation: the provider requires
the reasoning sequence to be returned unmodified and unreordered in `reasoning_details`.
Lossless round-trip outranks ergonomic access here, which is why the raw object is kept.

### Streaming events

`Event` normalizes provider output into:

- `ReasoningDelta` and `TextDelta` display fragments;
- `ReasoningDetails` replay blocks, which a frontend must not render;
- `ToolUseStart`, `ToolUseInputDelta`, and `ToolUseComplete`;
- `Usage`; and
- a terminal `Stop(StopReason)`.

Stop reasons normalize as `stop` → `EndTurn`, `tool_calls` → `ToolUse`, `length` →
`MaxTokens`, `content_filter` → `Refusal`, and anything else → `Other(String)`.

`Usage` tracks input, output, reasoning, cached-input, and cache-write tokens plus an optional
`cost` in real dollars reported by the provider. `cached_input_tokens` is the cache-hit signal.
Usage is parsed and reported on `turn.finished`, but not aggregated into cost or staleness
reporting; that belongs to M5, where the reported `cost` replaces a local price-table
calculation.

### Core result types

- `RunConfig` controls model, effort, token ceiling, round ceiling, and system prompt.
- `OutputEvent` is the frontend-facing subset: reasoning/text deltas and tool lifecycle.
- `RunOutcome` reports the terminal reason, number of rounds, and accumulated visible text.
- `LoopError` distinguishes provider failure, normalized protocol failure, and round limit.

### Tool types

`ToolDefinition` is a name, description, and JSON Schema. Definitions use
`additionalProperties: false` and are returned in lexical name order for cache stability.

`ToolResult` always contains text plus `is_error`. After executor construction, operational
errors are values rather than Rust errors so every requested tool ID receives a response.

Tool input structs are private Serde models with `deny_unknown_fields`. Invalid or extra
fields become ordinary error results.

### Guard types

`Verdict` is ordered by severity:

```text
Allow < Escalate < Deny
```

The maximum decision across the full shell syntax tree wins. `PolicyError` names the policy
and distinguishes outside-root, denied-secret, resolution, and custom-pattern failures.

### RPC scaffold

The wire envelopes live here. A notification carries no `id` and adds top-level `seq`,
`turn_id`, and `session_id`, so it cannot be represented by a request envelope; section 5 is
the contract.

## 7. OpenRouter transport details

`OpenRouterProvider` uses raw `reqwest` against OpenRouter's OpenAI-compatible surface, because
there is no official Rust SDK assumed by the project. Production requests use:

- `POST {base}/chat/completions` with `stream: true`;
- `Authorization: Bearer` carrying `OPENROUTER_API_KEY`; and
- `Accept: text/event-stream`.

The base URL defaults to `https://openrouter.ai/api/v1` and is injectable through
`OPENROUTER_BASE_URL`, which is also how transport tests use a loopback mock server.

### Stable request construction

- `system` blocks become leading `{ role: "system" }` messages.
- Tool definitions become `[{ type: "function", function: { name, description, parameters } }]`,
  sorted by name and serialized with stable key order.
- `Effort` becomes `reasoning: { effort }`, and is omitted entirely when the model does not
  declare effort support.
- Assistant `ToolUse` becomes `tool_calls[{ id, type: "function", function: { name, arguments } }]`,
  with `arguments` serialized to a string rather than an object.
- Assistant `Reasoning` is replayed verbatim as `reasoning_details` on the assistant message.
- A `cache_control` breakpoint is placed on the final system block, and only when the model
  declares `prompt_caching`.

Caching is model-dependent here rather than guaranteed by the transport, so an unsupported
model must receive no breakpoint at all. Changing system bytes, tool order, JSON key order, or
messages before a cache breakpoint can invalidate the prefix. Treat request serialization tests
as behavioral tests, not snapshots that can be casually regenerated.

### Tool results are one message each

This is the one place where the normalized model and the wire genuinely differ, and it is not
worth papering over. Anthropic returned every tool result inside a single user message;
OpenAI-shaped APIs require one `{ role: "tool" }` message per result. The provider therefore
fans a `ToolResult` batch out into consecutive tool messages, in the same order as the
`tool_calls` that produced them.

The invariant that actually mattered survives the change: a result batch is never split across
turns, so every tool message belonging to one batch is emitted consecutively before the next
assistant turn. A test asserts that ordering; it is the reason the model keeps issuing parallel
tool calls.

### SSE parsing

`sse.rs` owns `SseDecoder`, which handles frames split across arbitrary network chunks, both LF
and CRLF frame boundaries, comments, and multi-line data. It is transport-generic on purpose:
it was salvaged from the deleted Anthropic transport and belongs to no single provider.

`openrouter.rs` owns chunk interpretation, and three details are worth knowing before debugging
a stream:

1. The terminal sentinel is `data: [DONE]`.
2. OpenRouter emits `: OPENROUTER PROCESSING` comment keep-alives. They must be ignored, not
   treated as frames — this is the likeliest source of a spurious protocol error.
3. `delta.tool_calls[]` accumulate by array `index`, not by id. `id` and `function.name` arrive
   on the first fragment for an index only, and `function.arguments` arrives as string fragments
   that must be concatenated in order. This differs from a content-block-index model, and
   confusing the two is the second likeliest bug.

`usage` arrives on the final chunk before `[DONE]`, and `finish_reason` on the chunk before
that. Unknown future fields and event types are ignored so compatible protocol additions do not
terminate a session.

Accumulated tool `arguments` are parsed with `serde_json`. Malformed accumulated JSON is an
error, never a silent `{}`; never reconstruct JSON with string splitting.

## 8. Tool surface

The seven tools are intentionally permanent prompt cost, so additions require a strong case.

| Tool | Input | Behavior |
|---|---|---|
| `read` | `path` | Reads one allowed UTF-8 file. |
| `write` | `path`, `content` | Creates parents and writes one allowed file. |
| `edit` | `path`, `old_string`, `new_string` | Replaces exactly one literal occurrence. |
| `glob` | `pattern`, optional `path` | Lists matching allowed files deterministically. |
| `grep` | `pattern`, optional `path` | Regex-searches allowed UTF-8 files with line numbers. |
| `bash` | `command` | Executes only commands classified `Allow`. |
| `memory_search` | `query`, optional `limit` (1–20), `kind` (`file`/`symbol`) | Ranked `path:line  kind name — description` matches from the repository index; a fixed refusal when no index is attached. |

Recursive glob and grep walks respect `.gitignore` through `ignore::WalkBuilder`, filter
denied paths, and sort results. Direct access to a denied target is an error; a broad walk
silently omits denied files so it never reveals their names or contents.

`edit` rejects an empty search string, a missing match, or multiple matches. This avoids
ambiguous model edits and whole-file rewrites.

## 9. Safety invariants

### Path policy

`PathPolicy` binds an executor to a canonical workspace root. Resolution:

1. joins relative inputs to that root and lexically normalizes `.` and `..`;
2. rejects lexical escapes and secret-looking lexical names;
3. finds the closest existing ancestor with `symlink_metadata`;
4. canonicalizes that ancestor, failing closed on dangling symlinks;
5. rebuilds any nonexistent suffix; and
6. rechecks root containment and secret rules on the canonical path.

Checking both lexical and canonical forms matters: `.env -> safe.txt` must remain denied,
while `alias -> .env` must also be denied. New filesystem tools must call policy immediately
before the syscall and must not accept pre-resolved paths from untrusted input.

Built-in protected paths include `.env`, `.env.*`, private-key extensions, `id_rsa*`, common
credential files, `.aws/credentials`, and `.ssh/**`. Custom repository-relative globs can be
added when constructing a policy.

### Bash classifier

The classifier parses with tree-sitter-bash and walks every command and file-redirection node,
including pipelines, lists, subshells, and command substitutions. It aggregates the most
severe result:

- explicitly read-only commands may run;
- unknown or potentially mutating commands escalate;
- destructive, privileged, environment-disclosing, secret-reading, or secret-writing forms
  are denied; and
- write-capable shell redirections escalate, even when attached to `echo` or `printf`.

`Escalate` suspends the tool call and emits `approval.requested`; the decision arrives as
`session.resolve_approval`. A `Deny` never reaches that channel at all — it is terminal, and a
denied command emits no approval request. Any future approval path must reuse the same
classifier result rather than bypass it.

Static shell analysis cannot understand runtime aliases, generated program names, `eval`, or
every interpreter payload. The classifier deliberately documents known-not-caught cases; do
not describe it as a sandbox.

### Output scrubber

Every `ToolExecutor::execute` result—success or error—is scrubbed before returning to core.
Current patterns cover Anthropic key prefixes (`sk-ant-`), OpenRouter key prefixes
(`sk-or-v1-`), AWS access-key shapes, PEM blocks, and token/secret/password assignments. The
same implementation is intended for future memory writes so transient leaks cannot become
durable graph data.

The Anthropic pattern stays even though Elefante no longer calls Anthropic. These are *secret*
patterns, not provider dependencies: Anthropic keys still leak out of the repositories Elefante
reads and edits, regardless of which API Elefante itself talks to. Adding a pattern is cheap;
removing one costs a leak.

The scrubber is defense in depth, not permission to weaken path or shell policy.

## 10. Configuration and CLI behavior

Configuration precedence and defaults:

| Concern | Highest precedence | Environment fallback | Default |
|---|---|---|---|
| Model | `--model` | `OPENROUTER_MODEL` | `anthropic/claude-sonnet-5` |
| Effort | `--effort` | `ELEFANTE_EFFORT` | `high` |
| Max rounds | `--max-rounds` | — | `24` |
| Max tokens | `--max-tokens` | — | `16,384` |
| API origin | — | `OPENROUTER_BASE_URL` | `https://openrouter.ai/api/v1` |
| API key | — | `OPENROUTER_API_KEY` | required |
| Extractor model | — | `ELEFANTE_EXTRACT_MODEL` | the session's model |
| Extractor effort | — | `ELEFANTE_EXTRACT_EFFORT` | `low` |
| Learning | — | `ELEFANTE_LEARN`: `1`/`on` consents, `0`/`off` disables | ask once (consent file) |
| Idle flush | — | `ELEFANTE_LEARN_IDLE_SECS` | `300` |

Models are named by OpenRouter slug, not by vendor model name. The default routes to Claude
through OpenRouter; a different vendor's model is a different slug, not a different transport.

Prompt words after options are joined with spaces. `--` ends option parsing. If no prompt is
provided, stdin is read to EOF. Empty input, invalid numbers, unknown effort, unknown options,
invalid base URLs, and missing credentials produce named errors before the agent loop begins.

The process working directory is the tool-policy root. Running the CLI from this monorepo
permits access to this monorepo; running it from a nested directory intentionally narrows the
accessible tree.

Never store a real key in this repository. `.gitignore` excludes common environment and key
files, and Elefante's own tools deny them. Supply credentials through the process environment
or an external secret manager.

## 11. Testing and quality gates

Run the complete local gate before handing work off:

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git diff --check
```

Test ownership follows behavior:

- `elefante-guard` tests path traversal, symlinks, secret variants, compound shell syntax,
  redirections, known classifier limits, and scrub patterns.
- `elefante-tools` tests strict schemas, exact editing, path enforcement at syscall boundaries,
  deterministic walks, bash non-execution, scrubbing, and cloned/concurrent executors.
- `elefante-provider` tests deterministic request JSON, per-model capabilities, arbitrary SSE
  chunking, `[DONE]` and keep-alive handling, `tool_calls` accumulation by index, malformed tool
  JSON, verbatim `reasoning_details` replay, consecutive tool-result messages, usage including
  cached-input tokens, refusals, and forward-compatible events.
- `crates/core/tests/loop.rs` uses a scripted provider to test a complete edit loop,
  parallel result pairing, refusal, and malformed/incomplete streams without network access.

The provider's always-on transport tests bind a loopback TCP listener. If a sandbox rejects
local sockets, permit loopback binding rather than disabling the test.

### Credentialed provider check

The live cache test is ignored by default because it uses network access and billable API
requests. Run it explicitly against a slug whose upstream supports prompt caching:

```sh
export OPENROUTER_API_KEY="..."
export OPENROUTER_MODEL="anthropic/claude-sonnet-5"
cargo test -p elefante-provider live_second_identical_request_reads_prompt_cache -- --ignored
```

Caching is model-dependent under OpenRouter, so the check is only meaningful for a model that
declares `prompt_caching`. It passes when the second identical request reports nonzero
`cached_input_tokens`. The hermetic suite proves the *parsing* of that field; this gate proves
the cache actually hit.

A milestone is not complete until this check reports a nonzero cache read on the second
request and a live agent request lands a trivial, reviewed edit. Both run in CI through
`.github/workflows/credentialed.yml`.

## 12. Making changes safely

### Adding or changing a tool

1. Decide whether the behavior belongs in one of the seven existing tools. Tool definitions
   consume prompt tokens on every provider round.
2. Add or update the strict schema in `elefante-tools::definitions`.
3. Add a private `Deserialize` input type with `deny_unknown_fields`.
4. Route execution through `ToolExecutor::execute`.
5. Resolve every path with `PathPolicy` immediately before filesystem access.
6. Return operational failures as `ToolResult { is_error: true, ... }`.
7. Let the single executor exit path scrub the result.
8. Add policy-boundary, error-pairing, deterministic-order, and schema tests.
9. Confirm the provider's request-order and cache-stability tests remain green.

Do not expose a second tool executor or construct tool results in core to work around this
flow. The single boundary is what makes “every result is scrubbed” auditable.

### Adding a provider

1. Implement the normalized `Provider` trait.
2. Resolve `Capabilities` per model when support varies.
3. Send only fields the model declares.
4. Normalize text, reasoning, tools, usage, refusal, and stop behavior without leaking wire
   types into core.
5. Preserve any opaque replay blocks exactly and in order.
6. Use a configurable endpoint and hermetic transport fixtures.
7. Test malformed chunks, provider errors, refusal, tool-result continuation, and capability
   degradation.

OpenRouter is the only transport, and the `Provider` trait exists to keep wire types
out of core rather than to host a second implementation. Prefer a different model slug to a
second transport: OpenRouter already reaches other vendors' models, which is what lets
adversarial review run on a different vendor without a second integration. Adding a transport
is a milestone decision recorded in `PLAN.md`, not a fallback bolted onto the existing path.

### Changing the loop

Protect these invariants with regression tests first:

- one live turn per invocation;
- at most one assistant history message per provider response;
- reasoning blocks retained verbatim and in order;
- every tool call receives one matching result;
- all parallel results returned together, before the next assistant turn;
- refusal dispatches no unfinished tools; and
- the round cap terminates rather than spinning.

### Changing safety behavior

Add a fixture demonstrating the bypass before changing code. Test lexical and canonical
paths, existing and nonexistent targets, symlinks, recursive walks, nested shell syntax, and
both success/error scrubbing. Prefer a conservative escalation over a new allow rule whose
effects are not statically clear.

## 13. Milestone and documentation discipline

`PLAN.md` is the design of record. Amend it in the same change when implementation teaches
the project that a recorded decision is wrong. Do not silently make the code and plan disagree.

`MILESTONES.md` is evidence, not a roadmap. Append a completed entry only when the baseline
and entire gate pass together, including prior milestone gates. Record what shipped, what the
gate proved, anything deliberately deferred, and any plan amendment.

When behavior changes, update:

- nearby Rust documentation and tests;
- the crate's own `README.md` when its module layout, public entry points, invariants, or test
  layout changed — it quotes file line counts and specific line numbers, which is what makes it
  checkable and also what makes it rot;
- this developer guide if a public model, interaction, invariant, or workflow changed;
- the README if setup, user-visible behavior, or project status changed; and
- the design/milestone documents when their respective contracts require it.

## 14. Known limitations and next boundaries

- The index understands Rust, TypeScript/TSX and Markdown headings. Import edges resolve Rust
  `mod`/`crate::`/`self::`/`super::` and TypeScript relative specifiers; cross-crate paths, `tsconfig`
  aliases and bare packages are not resolved. There is no call graph, and `.js`/`.jsx` files are
  indexed by path only.
- Learned memory is per working tree (a worktree keeps its own store and its own notes), has no
  user-level graph, and does not follow a file across a rename or a branch switch: the note's link
  to the file goes with it and the note stays, found by its words. Retrieval is lexical, so a note
  phrased in vocabulary the prompt never uses is not found. There is no transcript compaction;
  one added later must call `Learner::flush` before dropping history.
- Memory spend is accumulated and shown; the rest of token accounting and staleness reporting
  waits for M5 proper.
- Forgetting a note is not durable: the same preference, said again, can be saved again. Consent is
  per user and extractor, not per repository, and a fresh attach does not summarise what was learned
  while the client was away. `docs/MEMORY-UI-DESIGN.md` § 13 keeps the open questions.
- There is no model-backed critic.
- There is no `session.list`, so a client cannot enumerate sessions it did not create, and
  sessions live only in daemon memory — they end with `session.close` or the daemon's idle exit.
  `PLAN.md` § Session picker and milestone M6 cover the list and durable history.
- The guard layer reduces blast radius but is not an OS sandbox and cannot eliminate
  time-of-check/time-of-use filesystem races.

These limitations are milestone boundaries. Implement them through the corresponding plan
and gate rather than hiding partial versions behind existing APIs.

### Open protocol questions

Two questions the wire contract left deliberately unsettled. Decide and record them here.

1. **Snapshot growth is unbounded per session.** Full assistant text is retained per completed
   turn — tool bodies are not, only previews — so a snapshot is bounded per turn but not per
   session, and turn count has no limit. Whether the TUI ships a scrollback view of completed
   turns or only the live turn plus a compact history decides this, because it decides what a
   snapshot has to carry.
2. **Whether `tool.started` should stream argument fragments.** It currently fires once with a
   complete `input_preview`. Streaming arguments would show tool inputs building in real time,
   at the cost of a much noisier event stream.

## 15. Troubleshooting

### `OPENROUTER_API_KEY is not set`

Export the key into the process environment. Do not create a repository `.env`; it is ignored
by Git and denied by Elefante's own path policy.

### `bash requires approval`

The classifier returned `Escalate`, so the call is suspended pending an approval. Answer the
prompt, or use a narrower existing tool (`read`, `write`, `edit`, `glob`, or `grep`). Do not
broaden the shell allowlist just to make a task convenient.

### `secret-path-policy: ...`

The requested lexical or resolved path is outside the workspace or matches a protected rule.
Move non-secret fixture data to an allowed path; never rename real credentials to evade policy.

### `memory_search is unavailable: this session has no repository index`

Memory is off or could not start for this session: `ELEFANTE_MEMORY=0` was set, the working
directory is `/` or `$HOME` (never indexed), or the store could not be opened — the daemon's stderr
says which. `read`, `glob` and `grep` work regardless.

### A block says `partial="true"`

The first full index of this repository has not finished, so a symbol's absence from it is not
evidence it does not exist. It resolves itself; a refresh on an already-complete index is never
partial.

### Provider stream ends with a protocol error

Start with the provider fixture tests. Check SSE framing, `: OPENROUTER PROCESSING`
keep-alives being mistaken for frames, `tool_calls` fragments accumulated by the wrong key, the
`[DONE]` sentinel, terminal stop events, and verbatim `reasoning_details` replay before changing
core validation.
