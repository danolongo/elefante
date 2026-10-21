# elefante — lightweight coding harness (Rust core + OpenTUI client)

> **Living document.** This is the design of record. When a decision changes during implementation, amend this file in the same change that implements it — a plan that drifts from the code is worse than no plan. Milestone outcomes are recorded in [`MILESTONES.md`](./MILESTONES.md).

## Context

The repo began empty (one README: "a quick modifiable harness for coding"). The goal is a coding agent harness that can be modified quickly and goes **deep on four pillars rather than wide on features**:

| Pillar | Has today | Gets deeper in |
|---|---|---|
| **Security** | Path policy, bash classifier, output scrubber, keyed memory rows, consent | M4 (environment, key), M5 (critics), M6 (audit log) |
| **Sandboxing** | Nothing — the guard reduces blast radius but is not an OS sandbox | M4, then reused by M6 |
| **Memory**, including isolated subagents | Repository index, learned notes, memory in the UI | M6 (shared with subagents), M7 |
| **Adversarial review** | Nothing | M5, reused by M6 to review a subagent's diff before it merges |

Two principles hold across all four:

1. **Minimal standing prompt.** Behavioral rules that can be enforced mechanically should be code — gates, validators, the sandbox — not sentences in a system prompt. Target: base system prompt under ~400 tokens.
2. **Incremental**, with real gates between increments.

Work that serves none of the pillars is out of scope. What the 2026-10-19 rescope cut, and why, is listed in § Cut in the rescope; reopening any of it means amending this file first.

---

## Development discipline

**Milestones are gates, not labels.** Work proceeds one milestone at a time. A milestone is complete only when:

1. Every item in its **Baseline** is implemented.
2. Every test in its **Gate** passes, run from a clean checkout.
3. The gate is run *as a whole* — not just the newly written tests. A later milestone that breaks an earlier gate is a regression to fix before proceeding, not a known issue to carry.

**We do not begin milestone N+1 until milestone N's gate is green.** If a gate can't be met, the correct responses are to fix it, or to explicitly renegotiate the baseline and write that decision into this file — not to proceed with it failing.

Each milestone ends with a short written record appended to `docs/MILESTONES.md`: what shipped, what the gate proved, and anything deliberately deferred. This is what keeps "incremental" from decaying into "unfinished."

---

## Decisions made

| Decision | Choice | Rationale |
|---|---|---|
| UI | Rust daemon + Bun/OpenTUI client over JSON-RPC on a unix socket | The same split opencode runs in production, so it is a proven shape rather than one invented here. A headless core is also what lets subagents run as further sessions of the same daemon (M6). |
| Retrieval | SQLite FTS5 + graph hop expansion | No embedding model, no API cost, deterministic and debuggable. Graph edges do the semantic work. |
| Storage | Project graph in `<repo>/.elefante/`, self-ignoring; user-level keys and consent in `~/.elefante/` | Project memory sits next to what it describes; nothing is ever committed, so no session noise reaches a diff. |
| Isolation | OS sandbox (Seatbelt on macOS, bubblewrap + seccomp on Linux) plus a git worktree per editing subagent | Bubblewrap can mask secrets and protect `.git` inside a writable root; Landlock cannot subtract access once granted. Linux requires bubblewrap and unprivileged user namespaces. Harness write git gains its own sandbox in M4.1. A microVM backend remains a proposal. |
| Cache | In-process `moka` LRU inside the daemon + SQLite WAL | Redis considered and rejected: subagents all attach to the same daemon, so they already share its cache. Redis would add a required daemon and an IPC hop to beat microsecond page-cache reads. Kept behind a trait for possible cross-machine use later. |
| Provider | OpenRouter only | One transport, one auth, one code path. Reaching Claude via `anthropic/claude-opus-5` means this narrows the *code*, not the *models*. `Capabilities` survives and matters more, now resolved per model. |

---

## Crate topology and dependency direction

```
elefante/
  Cargo.toml                 # workspace
  docs/PLAN.md               # this document
  docs/MILESTONES.md         # per-milestone completion record
  docs/DEVELOPER_GUIDE.md    # shipped behaviour, incl. the wire contract (§5)
  docs/LOCKFILE.md           # why tui/ commits exactly one lockfile
  crates/
    provider/                # Provider trait + Capabilities; sse decoder, openrouter
    guard/                   # PathPolicy, BashClassifier, Scrubber; sandbox policy (M4)
    mem/                     # graph store, indexer, retrieval, cache, learned notes
    core/                    # session, turn loop, context, learner, tools; critics (M5), subagents (M6)
    elefante/                # the one binary: wire/ types, daemon/ (`elefante daemon`), client/
  tui/                       # Bun + @opentui/core client (TypeScript)
```

**Dependency direction is a constraint, not an accident:**

```
elefante ──► core, mem, provider
core     ──► provider, mem, guard   (core::tools uses PathPolicy at the syscall boundary)
mem      ──► guard                  (Scrubber on the write path)
guard    ──► (nothing)
provider ──► (nothing)
```

`guard` must never depend on `core`, `mem` or `provider`. The practical consequence: `guard` is pure policy — it decides, it never performs. This is what makes the whole gate layer unit-testable without a filesystem or a network. Critics need a model, so they live in `core` (M5), not in `guard`: `mem` depends on `guard`, and a `guard → provider` edge would pull the HTTP stack into the memory crate.

Five crates, after the consolidation (§ Milestones › Consolidation): eight was more seams than the pillars need. `tools` became `core::tools`; `rpc`, `srv` and `cli` became one `elefante` crate and one binary. Client and daemon now share a crate, so one rule the old graph enforced is now a rule of review: **the client never runs a turn** — it always goes through the socket.

Key crates: `tokio 1.53`, `reqwest 0.13`, `rusqlite 0.40` (bundled + FTS5), `tree-sitter 0.27` (+ `tree-sitter-bash`), `blake3 1.8`, `moka 0.12`, `ignore`, `serde`/`serde_json`.

---

## Provider

One transport: **OpenRouter**. OpenAI-compatible `/chat/completions`, SSE, single key,
base `https://openrouter.ai/api/v1`.

Two impls doubled the transport surface of a harness whose value is elsewhere — memory,
guard, critics. Since OpenRouter reaches Claude anyway (`anthropic/claude-opus-5`), dropping
the Anthropic impl narrows the *code*, not the *models*. Adversarial review still runs on a
different vendor's model; it just no longer needs a second integration to do it.

**What is given up**, recorded so it is not rediscovered later: Anthropic-native request
shapes (`output_config.effort`, `thinking.display`, native `cache_control` placement for
non-Anthropic upstreams), and a proxy hop on every request. Cache behaviour becomes
model-dependent rather than guaranteed.

**What survives:** reasoning control, verbatim reasoning replay, prompt caching with real
hit metrics, structured outputs, parallel tool calls.

```rust
struct Capabilities {
    adaptive_thinking: bool, effort: bool, prompt_caching: bool,
    mid_conversation_system: bool, structured_output: bool, cache_metrics: bool,
}

trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> Capabilities;
    fn stream(&self, req: Request) -> BoxStream<Result<Event>>;
}
```

`Capabilities` survives the collapse to one provider and **matters more, not less**: within
OpenRouter, per-model variance is larger than the old cross-provider variance was. Resolution
is prefix matching on the model slug, defaulting to all-false for unrecognised slugs — an
unknown model must never silently receive fields it cannot handle.

Degradation stays at the *mapping* boundary inside the impl, so `elefante-core` never learns
provider shape. `Capabilities` gains one core-side caller later, where the branch is genuinely
core's: `structured_output`, for critic findings (M5). `mid_conversation_system` has no planned
caller: repository context and critic findings are both `user` messages on purpose, because both can
carry text the repository controls — see step 3.

### Wire facts that constrain the design

- **Reasoning** — request `reasoning: {effort}` maps 1:1 onto the internal `Effort` enum
  (`low|medium|high|xhigh|max`). Responses carry `reasoning_details[]` in three shapes
  (`reasoning.text`, `reasoning.summary`, `reasoning.encrypted`).
- **Reasoning replay is strict.** The sequence of reasoning blocks must be returned
  unmodified and unreordered. Blocks are therefore stored verbatim as opaque JSON rather
  than parsed into typed variants — an unknown future `reasoning.*` type must round-trip,
  and a typed enum would silently violate the rule instead of erroring.
- **Caching** — `cache_control` breakpoints on Anthropic/Vertex/Azure/Bedrock upstreams.
  Hits verified with `usage.prompt_tokens_details.cached_tokens`; writes with
  `cache_write_tokens`.
- **Cost is reported directly** as `usage.cost`, so M5 attribution stops being a price-table
  calculation.
- **Tool calls** stream as `delta.tool_calls[]` accumulating by **array index** — `id` and
  `function.name` on the first fragment only, `function.arguments` as string fragments.
- **Structured outputs** via `response_format: {type: "json_schema", json_schema: {...}}`.
- `: OPENROUTER PROCESSING` comment keep-alives must be ignored, and `data: [DONE]`
  terminates the stream.

Full request/response mapping, as shipped: [`DEVELOPER_GUIDE.md`](./DEVELOPER_GUIDE.md) § OpenRouter transport details.

### Role configuration

A role is a `(model, effort)` pair: one provider means there is nothing else to choose. Roles are set
through the environment, each falling back to the author's model when unset:

| Role | Variables | Lands in |
|---|---|---|
| author | `OPENROUTER_MODEL`, `ELEFANTE_EFFORT` | M0 |
| extractor (learned memory) | `ELEFANTE_EXTRACT_MODEL`, `ELEFANTE_EXTRACT_EFFORT` | M3 |
| critic | `ELEFANTE_CRITIC_MODEL`, `ELEFANTE_CRITIC_EFFORT` | M5 |
| bash triage | `ELEFANTE_TRIAGE_MODEL`, `ELEFANTE_TRIAGE_EFFORT` | M5 |

No `config.toml` role table: four variables do not need a file format. The critic should be a
*different vendor's* model, which is what makes review adversarial rather than self-agreeing; triage
can be anything cheap and fast. Per-model pricing needs no local table — `usage.cost` comes back on
every response.

### Key setup

**Shipped (M1).** With no key in the environment, the daemon answers `-32004` and the TUI asks for an
OpenRouter key, sends it with `key.set`, and the daemon stores it under `~/.elefante/` at mode `0600`
— outside every working tree, so the agent's blast radius never contains it. An exported
`OPENROUTER_API_KEY` wins over the stored key. A headless one-shot never prompts.

The model picker, catalogue fetch, pricing display, a guided first-run flow and capabilities derived
from catalogue metadata were planned here and cut in the rescope: models are chosen by slug, and
capabilities stay a conservative prefix table that defaults to all-false.

---

## Process topology

```
  ┌──────────────┐                     ┌──────────────────────────────┐
  │   elefante   │──starts if absent──►│       elefante daemon        │
  └──────┬───────┘                     │  ┌────────────────────────┐  │
         │ spawns                      │  │   Session registry     │  │
         ▼                             │  │  (many sessions, 1..n  │  │
  ┌──────────────┐   JSON-RPC over     │  │   clients per session) │  │
  │ Bun + OpenTUI│◄──unix socket ─────►│  └────────────────────────┘  │
  │   (tui/)     │   framed NDJSON     │   provider clients (shared)  │
  └──────────────┘                     │   mem stores (lazy per repo) │
                                       │   moka cache · tool executor │
                                       │                              │
                                       │   subagent sessions (M6):    │
                                       │   own worktree + sandbox     │
                                       └──────────────────────────────┘
```

Socket at `$XDG_RUNTIME_DIR/elefante.sock`, falling back to `~/.elefante/run/elefante.sock`, mode `0600`. A client connects, and starts the daemon (its own executable, as `elefante daemon`) only if the connect fails. The daemon exits after a configurable idle period with zero attached clients.

**Why the daemon exists even for single-user CLI use:** it is what lets subagents (M6) run as further sessions beside the parent, sharing one warm memory store and one set of provider connections, and lets a client attach to any of them with the existing `session.attach`. Retrofitting that onto an in-process design is the expensive path, so the split is paid for up front and is cheap now.

Sessions live in daemon memory and end with `session.close` or the daemon's idle exit. A session
picker, `session.list`/`resume`/`delete` and durable conversation history were planned here and cut
in the rescope. What survives of them is the **audit log** in M6: a durable, scrubbed record of what
an agent did, which serves security rather than convenience.

---

## The RPC protocol (`elefante::wire`)

Newline-delimited JSON, JSON-RPC 2.0 shaped, bidirectional over one unix socket. Requests carry ids; streaming uses server→client notifications, because the TUI needs sub-frame latency and polling would jitter.

Every notification carries a monotonic per-session `seq` and a `turn_id`. `session.attach` takes an optional `since` and returns either resumed deltas or a full snapshot — never a silent gap. That is what lets a second client, or a reconnecting one after a crash, render a session already in flight; building it in from the start is far cheaper than retrofitting it once the TUI assumes it owns the session.

**The method set, wire types, snapshot shape, error codes, ring sizing, and backpressure rules are specified in [`DEVELOPER_GUIDE.md`](./DEVELOPER_GUIDE.md) §5.** That section is the single source of truth for the protocol; this one covers only why it is shaped that way.

---

## The turn lifecycle

This is the central interaction, and the place where every crate meets. One numbered trace, with the owner of each step:

**1. Prompt received** — `srv` routes `session.prompt` to `core::Session`, which takes a turn lock. A second prompt on the same session while a turn is live is rejected, not queued: silent queueing makes an interrupted session behave unpredictably.

**2. Context assembly** (`core` → `mem`) — **shipped in M2**

```
core::Session::prompt → ContextSource::turn_block(prompt)           once per turn
    mem::Retriever::retrieve(Query { text, files, scope, budget_tokens: 1500 })
        → in-process cache keyed (query, scope, budget, repo_generation, partial)
        → miss: FTS5 seeds → graph expand (2 hops, damped, ≤ 8 per node) → pack to budget
    → Option<MemoryBlock { nodes, tokens, partial, generation, rendered }>
```

`partial: true` until the **first full index** has finished — retrieval never blocks on indexing.
A refresh running on an already-complete index is *not* partial: refreshes run at every turn, and
flagging all of them would teach the model to distrust an index that is merely being kept current.
No match yields `None`, never an empty envelope.

**3. Placement** (`core`) — **ephemeral, `user` role, immediately after the turn's user message.**
This replaces an earlier capability-dependent design (`system` role when
`mid_conversation_system`, else prepended to the user turn), for two reasons:

- *Authority.* The block is repository-derived text — doc comments, file names — which in a cloned
  repository is attacker-controllable. `system` would give it the highest authority a message can
  have; the `read` tool returns the same text at tool-result authority. A `user` message with
  escaped, framed, truncated content is the compromise. (A synthetic `memory_search` call and result
  would be lower still, but a fabricated assistant `tool_use` turn carries no `reasoning_details` and
  is rejected by providers that require them on a tool-use turn.) The brief, which sits in front of
  every request, is further limited to plain paths, counts and identifiers.
- *Cache.* The block is built into each request and **never stored in history**, so it does not
  accumulate. Placed *after* the user message, the request prefix is byte-identical across
  consecutive turns through that message and only the block and what follows differ; placed before,
  the user message and everything after it would shift. It is fetched once per turn and re-sent at the
  same index every round, so the cache holds across tool rounds.

A preamble (the repository brief) leads the request. It is adopted at most once per session and then
frozen, so a changing index cannot invalidate the prefix mid-session.

**4. Request construction** (`core` → `provider`) — `core` builds a provider-agnostic `Request { system, tools, messages, role }`. The provider maps it to wire format, applying only capabilities it declares, and places cache breakpoints itself (end of the system block, when the resolved model declares `prompt_caching`). `core` never writes provider-specific fields.

**5. Stream** — provider yields a normalized `Event`:

```rust
enum Event {
    ReasoningDelta(String),                  // renderable
    ReasoningDetails(Vec<ReasoningDetail>),  // replay state; never rendered
    TextDelta(String),
    ToolUseStart { id, name }, ToolUseInputDelta(String), ToolUseComplete { id, input: Value },
    Usage(Usage), Stop(StopReason),
}
```

`core` forwards each as an RPC notification *and* accumulates the assistant message. Two consumers, one pass. The split between renderable reasoning deltas and non-renderable replay state is load-bearing: replay blocks must be echoed back verbatim on the next request, and a frontend that renders them would invite a user to think they are ordinary output.

**6. Tool dispatch** on `stop_reason == tool_use` — the most interaction-dense step.

> **Status: shipped in M1.** Dispatch is a `JoinSet` and bash is classified through
> `guard::check` in `core`, not inside `tools`. This diagram was the target while it was still
> design; reading it as description back then is what produced a protocol draft promising
> interrupt guarantees the code could not keep.

```
for each tool_use block  ──►  guard::check(&call) ──┬─► Allow    ─► tools::execute
   (all concurrently,                               ├─► Deny(r)  ─► synthesize error result,
    tokio::JoinSet)                                 │              never touches the filesystem
                                                    └─► Escalate ─► approval flow (below)
                                    ↓
                          guard::scrub(result)
                                    ↓
              ALL results collected into ONE user message
```

Three details that are easy to get wrong and expensive to fix later:

- **All results must be emitted consecutively, before the next assistant turn.** OpenAI-shaped APIs need one `{role:"tool"}` message per result, so a batch is several messages rather than one — but it must never be split across turns, which trains the model out of parallel tool calls. Order must match the `tool_calls` that produced them.
- **A failed tool still returns a result** with `is_error: true`. Dropping it desynchronizes `tool_use_id` pairing and the next request 400s.
- **`guard::scrub` runs on every result**, including denials and errors — it is the layer that catches what the path policy cannot.

**`Escalate` and `Deny` are different modalities, not severities.** `Deny` is terminal and no critic or flag clears it, while `Escalate` is a question whose answer may be yes. That is what stops an approve button appearing on `rm -rf /`.

**Approval flow (Escalate):** `core` emits `approval.requested` with the command and the rule that fired. The turn suspends on a `oneshot` channel until `session.resolve_approval` answers. From M5 the triage critic's verdict rides along, and a pass may resolve it on its own — **only while the M4 sandbox is active**, because the sandbox is what bounds an approval that turns out wrong. An interrupt cancels the whole `JoinSet`, and already-completed tool results are still returned so history stays consistent.

**7. Loop** back to step 4 until `stop_reason != tool_use`.

**8. Post-turn observation** (`core` → `mem`) — refreshes the index for the agent's own edits, hands the turn's text to the learner, and accumulates usage.

**9. Review gate (M5)** — when the model ends a turn that edited files, `core::critic` reviews the session root's diff. Findings that cite a changed line come back as a `user`-role message and the loop resumes, capped at two revision rounds so it cannot spiral. Findings are kept as pending `source='critic'` notes, so the user decides which critiques this repository should remember.

---

## Memory subsystem

### Storage

```
~/.elefante/
  credentials.toml           # the OpenRouter key from `key.set`, 0600
  memory.key                 # 32 random bytes, 0600: authenticates every memory store this user owns
  memory-consent             # keyed, one record per extractor model
  run/elefante.sock          # socket fallback when $XDG_RUNTIME_DIR is unset

<repo>/.elefante/
  memory.db                  # the graph: node, edge, import_ref, node_fts, meta
  cache.db                   # derived: file_state, content-addressed extractions, briefs
  worktrees/<id>/            # M6: one per editing subagent, removed after merge
  audit/<session>.jsonl      # M6: append-only, scrubbed record of every tool call
  .gitignore                 # generated, contains `*` — self-ignoring, written before either db
```

The generated `.gitignore` containing a single `*` means the directory ignores itself and the host
repo's own ignore rules are never touched. Written first, so `git status` is clean at every instant,
not merely after a clean exit. Directory `0700`, databases and their `-wal`/`-shm` `0600`.

**Repo identity** is the nearest ancestor of a session's working directory containing a `.git` entry
(a directory, or the file a worktree has), else the working directory itself. `/` and `$HOME` are
refused. `repo_root` is for grouping and for where `.elefante/` lives, **never** for tool policy: a
session started in `repo/subdir` is still confined to `subdir`, and retrieval is *scoped* to match so
it never names a file the session's tools cannot open. M6 reuses this resolver.

**`cache.db` is content-addressed.** An extraction is keyed by the `blake3` of a file's content plus
an extractor id (`rust@1`, `typescript@1`, `tsx@1`, `markdown@1` — TS and TSX are separate because
identical bytes can parse differently), and is path-independent. A rename, a branch switch back, two
byte-identical vendored files, and a rebuilt `memory.db` all cost no parse. It is disposable: any
schema-version mismatch drops and recreates it. `memory.db` is not — from M3 it holds learned memory,
and migrates.

**Keeping two databases honest.** They cannot share an atomic commit. A random *pairing epoch* in
`memory.db.meta` is recorded in `cache.db`; a mismatch clears `file_state` and keeps the extractions.
Each batch commits to the graph first and the cache second, so a crash leaves the cache *behind* the
graph and the files are re-examined and re-upserted identically. An orphan sweep after a *complete*
walk reconciles deletions. Deleting either database, or both, converges on the same graph.

### Concurrency: the memory actor

`rusqlite::Connection` is not `Sync` and SQLite calls block. Rather than scattering `spawn_blocking`
across the codebase, `mem` runs **two actor threads**: a *writer* owning both write connections
(`memory.db` and `cache.db`) and a *reader* owning a read-only `memory.db` connection. WAL lets the
reader answer retrieval while the writer commits, so retrieval never waits on an index write. Heavy
work — walking, hashing, parsing — runs on worker threads without touching a database, so a write
transaction is as long as the rows it changes. Actors serialise access, keep `Connection` off the
async runtime, and give one place to count calls. A result cache sits in front of the reader, keyed
by an in-process mirror of `repo_generation`, so a hit never crosses a channel.

### Schema

```sql
node(id, kind, key, name, body, file_id, attrs, confidence, source, created_at_ns, updated_at_ns,
     status, last_reinforced_ns, mac, UNIQUE(kind, key))
edge(src, dst, rel, weight, created_at_ns, PRIMARY KEY(src, dst, rel),
     CHECK(rel <> 'co_edited_with' OR src < dst))
import_ref(file_id, kind, spec)                 -- raw imports; edges are derived from these
node_fts(name_terms, path_terms, body_terms)    -- FTS5, rowid = node.id, pre-split identifiers
meta(key, value)   -- graph_epoch, repo_generation, index_complete, cochange_head, edges_dirty, ...
```

Kinds: `fact`, `preference`, `decision`, `convention`, `file`, `symbol` — **not enforced by a
`CHECK`**, because SQLite cannot alter one and adding a kind in M3 would force a table rebuild; a Rust
enum validates at the boundary. Relations: `about`, `depends_on`, `co_edited_with`, `contradicts`,
`supersedes`, `derived_from` — M2 uses `depends_on` (file → file) and `co_edited_with` (file ↔ file).

- **`(kind, key)` is a stable natural key**: a file's repository-relative path; a symbol's
  `path::qualified_name`, `#2`, `#3` for duplicates.
- **`source` is the ownership discriminator.** The indexer reads, updates and deletes only
  `source = 'index'` rows. It never touches a learned row, and the orphan sweep ignores them.
- **Ids are stable across edits.** The indexer upserts; a file node is updated in place and a symbol
  keeps its id while its key does, so a learned edge into either survives re-indexing. A rename is a
  delete plus an add, so a learned link into the old path is lost; re-linking is M3's decision.
- **`symbol → file` is a column (`file_id`), not an edge.** `ON DELETE CASCADE` removes a deleted
  file's symbols, raw imports and incident edges in one statement.
- **Edges are derived, not parsed.** An import edge depends on a file's raw imports *and the set of
  paths*, so creating `foo.ts` can resolve an import in a file that did not change. `meta.edges_dirty`
  is written in the same transaction as the change that makes edges stale, so a crash cannot leave
  them stale with nothing to say so.

### Two ingestion layers

The split is what keeps this cheap:

**Mechanical** — no LLM, fully cacheable, runs in the indexer:

```
ignore::WalkBuilder (honours ignore files beneath the root; skips symlinks, binaries, > 1 MiB,
                     anything PathPolicy::is_secret_path flags — unread)
  → stat comparison, with a racy-timestamp rule → blake3 of what changed
  → extraction by content hash: tree-sitter (Rust, TypeScript/TSX), heading scan (Markdown)
      signatures end before any body or initializer, string literals elided, one doc sentence,
      every string through the Scrubber; never source
  → batched graph writes (graph first, cache second); one generation bump per batch
  → import edges derived from stored raw imports + the set of paths
  → git log --name-status -M -z -n 500 --no-merges, only when HEAD has moved
        skip commits touching > 50 files · follow renames · min support 2 · top 20 per file
        weight = raw commit count (stored); normalised at retrieval
```

Git co-change is the highest-value-per-cost signal here: it captures "these files change together" —
real coupling that imports alone miss — for the price of one `git log`. Git runs without a shell,
under a timeout and a size cap, with the repository's own config unable to run a program.

**Learned** — decisions, conventions, preferences and facts distilled from conversations by a model
call, **incrementally at turn end** (there is no reliable session end) and flushed on session close, after
`ELEFANTE_LEARN_IDLE_SECS` of quiet and as the daemon exits. The model proposes; `elefante-mem` disposes:

- *Input.* Only the text of what the user and the assistant said, scrubbed — never tool results,
  tool calls or reasoning. A gate (an explicit cue, ~600 characters of user text, or 6 user messages)
  decides *when*, never *whether*; skipped turns stay pending, and the pending text is the watermark,
  which shrinks only after a commit.
- *Verification.* A verbatim quote present in the excerpt; no command, URL, path or identifier in the
  wording that the user did not write; unchanged by the `Scrubber` (a changed note is dropped, never
  stored redacted); and the speaker of the evidence assigns the origin and starting confidence
  (explicit 0.9, stated 0.6, assistant 0.4 and `pending` — never shown until the user confirms).
- *Dedupe and supersede.* Near-identical wording (Jaccard ≥ 0.9 over stemmed words, negations kept,
  same negations) is a duplicate whatever the model said. A model verdict (`same_as`, `replaces`) is
  honoured only if it names a note it was shown, of the same kind, lexically related or sharing an
  anchor; `replaces` needs the user's own words. A replaced note is `superseded` (kept for 90 days,
  out of search), never merged.
- *Integrity.* Both databases carry a keyed stamp and every learned row a keyed MAC; anything
  unstamped, foreign or failing is discarded or treated as absent, and retrieval renders only
  `source = 'index'` rows and verified learned rows.
- *Decay.* `confidence × 0.5^(age ÷ half-life)` since last reinforcement (180 days preference and
  convention, 90 decision, 30 fact), computed on read; below 0.1 a note is not retrieved and compaction
  removes it.
- *Controls.* `elefante memory list | forget | approve | clear`.

### Indexer scheduling

`Indexer::refresh()` runs on a dedicated thread, is **single-flight**, and never blocks: a call while
a run is in progress marks it dirty and returns, and the run goes round once more — N triggers become
at most two runs. It is requested at session create, turn start and turn end (the user's editor edits
land at the next turn start, the agent's own at turn end). Retrieval against a partially-built index
is valid and flagged.

The repo brief (~300 estimated tokens, deterministic, no model call) is injected once per session and
**frozen**. It contains only paths, counts and identifiers, and a path is admitted only if it matches
`[A-Za-z0-9._/@+-]+` — with no whitespace a hostile file name cannot form a sentence. It is cached by
`blake3(tree fingerprint ‖ co-change head ‖ template version)`, because "central files" depends on
edges as well as content; the newest 8 are kept.

### Anti-bloat

This decides whether the whole subsystem works. LLM memory fails by accumulating contradictory sludge:

- Dedupe before insert: lexical bands, authoritative at commit time inside the writer transaction.
- `supersedes` edges rather than deletion — history stays inspectable for a retention window.
- Confidence decays with age unless reinforced.
- Compaction removes decayed, expired, unverifiable and excess notes (300 kept).

**Synergy worth protecting:** transcript compaction extracts facts into the graph first, so the summary can be terse without being lossy — the durable substrate holds what the summary drops.

---

## Guard subsystem

### Secret protection — four layers, none sufficient alone

1. **`PathPolicy` at the tool path resolver.** Every filesystem tool calls `policy.resolve(path)?`, which canonicalizes *then* matches — defeating `foo/../.env` and symlinks. It lives in `tools`, directly above the syscall, so a newly added tool cannot accidentally bypass it. Set: `.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `.netrc`, `.npmrc`, `.git-credentials`, `.aws/credentials`, `.ssh/**`, user-extensible. Applies to read, glob, **and grep** — grep is the one people forget, and it leaks contents just as readily.
2. **Bash gate rules** for `cat .env`, `source .env`, `env`, `printenv`.
3. **`Scrubber` on every tool result** — `sk-ant-` prefixes, AWS key shapes, PEM blocks, `TOKEN=`/`SECRET=`/`PASSWORD=` assignments. This is the layer that actually holds, because shell bypasses are unbounded (`cat .e''nv`) and cannot be enumerated.
4. **The same `Scrubber` gates memory writes.** Without this, a leaked secret is persisted into the graph permanently — strictly worse than transient context exposure. Two call sites, one implementation, no drift.

Denials return a deterministic tool error naming the policy, so the model stops retrying instead of burning turns.

### Bash classifier

```
tree-sitter-bash parse
  → walk every command node — including those inside &&, ||, |, ;,
    subshells, and command substitution
  → per command: extract program + args → rule table
  → verdict = most severe across all commands in the line
```

Classifying only the first command is the obvious bug here; `ls && rm -rf /` must not read as `ls`.

Stated plainly: **shell obfuscation is unbounded.** This is deny-by-default for destructive verbs outside an allowlist, not pattern-matching for badness. It reduces blast radius; it is not a sandbox, and the tests assert that we don't claim otherwise.

### Critic

**Planned (M5), in `core`.**

```rust
trait Critic { fn review(&self, artifact: Artifact, ctx: &Context) -> Verdict; }
enum Verdict { Pass, Findings(Vec<Finding>) }
struct Finding { severity, location, claim, evidence }
```

One abstraction, two call sites: bash escalation (triage) and the diff at the end of an editing turn,
including a subagent's diff before it merges (M6). Findings come back as structured output under
`strict: true`, parsed leniently for models without it, and **must cite a changed file and line** —
checked mechanically, never trusted. The critic takes a `Provider` handle by injection, never
constructing one, which is what makes it testable against a stub. A plan critic was cut in the
rescope: a plan has no lines to cite, so its findings cannot be checked.

---

## Agent loop details

Seven tools: `read`, `write`, `edit`, `glob`, `grep`, `bash`, `memory_search`; M4 adds `web_search`
and M6 adds `agent`, the ninth. Every tool is permanent prompt cost, so the bar for another is high.

- Edits are exact-string-replace. Highest reliability, lowest tokens, no whole-file rewrites.
- The system prompt stays under ~400 tokens because rules live in code: the sandbox *is* "don't touch anything outside the workspace"; the bash classifier *is* "don't run destructive commands"; the path policy *is* "never read .env"; the diff critic *is* "review before done".
- Cache stability: frozen system text first (with `cache_control`), then a deterministic tool list — sorted, stable JSON key order — then messages. An unsorted tool list silently invalidates the cache every turn, which is exactly the class of bug the M0 gate checks for.

---

## Milestones

Each has a **Baseline** (what must exist) and a **Gate** (what must pass before the next milestone starts).

### M0 — Loop

**Baseline.** Workspace skeleton. `Provider` trait + `Capabilities` + a single OpenRouter impl (OpenAI-compatible SSE, per-model capability resolution, `reasoning` control and verbatim replay, `tool_calls` normalization, prompt caching where the upstream supports it). Transport-generic SSE decoder in its own module. Agent loop with the seven tools, plain stdout, no TUI. **`PathPolicy` and the deterministic half of the bash classifier ship here** — an ungated bash tool should never exist, even in a prototype.

*The Anthropic impl was written first and then removed.* Doing this inside M0 rather than later matters because **M1 puts the neutral types on the RPC wire**; changing them after M1 would mean revising a published protocol instead of a private enum.

The decision it forced, now settled: reasoning blocks are stored **verbatim as opaque JSON** (`ReasoningDetail { kind, raw }`), not parsed into typed variants. OpenRouter's replay rule requires the block sequence to return unmodified and unreordered, so an unknown future `reasoning.*` type must round-trip — a typed enum would silently violate the rule rather than erroring.

**Gate.**
- Bash classifier fixture table: commands → expected `Allow`/`Deny`/`Escalate`, including compound commands (`ls && rm -rf /` must classify as the `rm`), pipes, subshells, and command substitution. Includes obfuscation cases asserted as **known-not-caught**, so the test documents the limit rather than implying coverage.
- `PathPolicy` canonicalization: `./x/../.env`, a symlink pointing at `.env`, `.env.local`, and a grep whose *pattern* would match secret content — all denied.
- Secret fixture: an `.env` containing an `sk-ant-`-shaped string. Read, glob, and grep all deny; `cat .env` is gated; the scrubber redacts it from a bash result that reaches it another way.
- Cache: `usage.prompt_tokens_details.cached_tokens > 0` on the second identical request of a session — zero means a silent invalidator. Hermetic fixture proves parsing; the live check stays credentialed and separate.
- Requests carry name-sorted tools with stable key order — an unsorted tool list silently invalidates the cache every turn, which is exactly the bug this catches.
- A model that does not declare reasoning support receives no `reasoning` field at all.
- Streaming edge cases: frames split mid-boundary, mixed line endings, `: OPENROUTER PROCESSING` keep-alives ignored, `data: [DONE]` terminates, unknown future event types ignored.
- `delta.tool_calls[]` accumulate by **array index** — `id`/`name` on the first fragment only, `arguments` concatenated then parsed. Malformed accumulated JSON is a hard error, never a silent `{}`.
- `reasoning_details` round-trips byte-identical through a tool continuation.
- Two parallel `tool_calls` produce two consecutive `{role:"tool"}` messages in call order.
- An unrecognised model slug yields all-false capabilities and a request carrying no `reasoning` field.
- No request carries a field the resolved model does not declare — no `reasoning`, no `cache_control`.
- Every request carrying `tools` also carries `provider: {require_parameters: true}`, and a request with no tools does not. Without it an endpoint may silently drop `tools`, and the loop then finds no tool calls and stops after one round — the agent does nothing while appearing to work.
- `openrouter/auto` (and `openrouter/auto-beta`, by prefix) resolves to reasoning and structured output but **not** caching: the catalogue declares `reasoning`/`reasoning_effort`/`structured_outputs` for that slug, and its pricing carries no `input_cache_read`, so caching is unknown rather than absent and no breakpoint is sent on a guess.
- A missing `OPENROUTER_API_KEY` produces a named, graceful error rather than a transport failure.
- End-to-end: run against this repo, request a trivial edit, confirm the tool loop completes and the edit lands.

### M1 — Daemon + TUI

**Baseline.** `elefante-rpc` protocol; daemon over unix socket with session registry; Bun/OpenTUI client rendering streamed text, reasoning, tool calls, and approval prompts. The CLI stops running the loop in process and becomes a client — one-shot prompts use a headless client, which doubles as the daemon's test harness. **First-run key setup** rides on the same channel.

Shipped. The wire contract now lives in [`DEVELOPER_GUIDE.md`](./DEVELOPER_GUIDE.md) §5, and the gate that proved it is recorded in [`MILESTONES.md`](./MILESTONES.md); the items below are the summary. The contract was drafted as two documents — core types first, then the wire derived from them — because an adversarial review found the reverse order had produced four blocking defects. Both were retired once the code carried the same information.

M1 also lands three things M0 left as design rather than code: `guard::check` moves from `tools` into `core`'s dispatch so approval has somewhere to suspend, dispatch becomes a `JoinSet` so an interrupt can keep completed tool results, and `run_agent_loop` becomes a `Session` that owns history across turns.

The headless client splits streams: **`stdout` is the answer, `stderr` is reasoning, tool activity, and errors.** Issue #4 recorded the current CLI rendering reasoning and answer through one match arm, producing an undelimited wall of prose. Splitting by stream removes that class of bug by construction — a redirected `stdout` has no reasoning in it to distinguish — rather than relying on formatting to keep them apart.

**Gate.**
- Kill and reconnect a client mid-turn: `session.attach` replays state and the turn completes with output intact.
- Two clients attached to one session both render the same stream.
- Interrupt cancels in-flight tools, and already-completed tool results still appear in history (no `tool_use_id` desync — assert the next request does not 400).
- Socket is mode `0600`; daemon exits after idle timeout with zero clients.
- First launch with no key resolvable prompts for one, stores it under `~/.elefante/` at mode `0600`, and never writes it into a working tree. An already-set `OPENROUTER_API_KEY` wins over the stored value.
- A model that reasons at length while emitting no renderable text does not present as a frozen UI — the failure the old `thinking.display` note warned about.
- **M0 gate still green.**

### M2 — Repo cache + mechanical graph

**Baseline.** Schema design pass first. Walk + blake3 cache, tree-sitter symbols, import edges, git co-change edges, repo brief, FTS5 + graph-hop retrieval injected per turn, and a working `memory_search`.

**Gate.**
- Warm start: second launch on an unchanged repo performs **zero** tree-sitter invocations (asserted by counter, not by timing).
- Touching one file re-parses exactly one file and bumps `repo_generation` once.
- `co_edited_with` weights match a hand-computed expectation on a fixture repo with known commit history.
- A prompt issued before indexing completes returns a `partial: true` memory block and does not block.
- Retrieval respects the 1500-token budget under an adversarially large graph.
- Editing a file leaves its node id, and the ids of its unchanged symbols, intact.
- Deleting `cache.db` or `memory.db` converges on an identical graph — the latter with zero parses — and a crash between the graph commit and the cache write converges too, edges included.
- No secret fixture string survives in any row or in the raw bytes of any file under `.elefante/`; `git status` is clean after a session and at a mid-index kill; files are `0600`, the directory `0700`.
- Hostile repository text cannot close or forge the injected envelope; no request carries a `system` message other than the configured prompt; the block is absent from history and from the next turn's request.
- FTS-hostile prompts never error; a session in a subdirectory sees only its subtree; history before a rename counts for the renamed file; a non-repository indexes fully with no co-change.
- N concurrent refresh requests collapse to at most two runs and none blocks.
- `ELEFANTE_MEMORY=0` creates no `.elefante/` and adds no context.
- Golden retrievals: a fixture repository's queries find the expected file in the top three.
- **M0–M1 gates still green**, and the MSRV check passes with the new dependencies.
- *Credentialed:* a real model, given `--max-rounds 1`, names a function it can only know from the index it was sent.

### M3 — Learned memory

**Baseline.** Learned notes (`preference`, `convention`, `decision`, `fact`) in the project graph;
incremental extraction behind a gate with a watermark; verification, provenance by speaker, hybrid
dedupe and supersession; keyed integrity for stores and rows; read-time decay and compaction; a
separate `<repo-memory>` section in retrieval; and `elefante memory list | forget | approve | clear`.
Deferred from the original baseline: the user-level graph (since cut) and transcript compaction (M7)
— M3 ships the seam (`Learner::flush`, and a watermark that only shrinks after a commit), so whatever a
future compactor drops has already been read.

**Gate.**
- `git status` is clean after a full session in a test repo.
- The same fact stated twice, in reordered or paraphrased words, is one note with raised confidence;
  a paraphrase merges only on a verified model verdict.
- A contradicting fact, in the user's own words, supersedes the old note; retrieval returns the survivor.
- No row and no byte under `.elefante/` contains the `sk-ant-` fixture after a session that touched it.
- Extract incrementally, truncate the history, and a fact from the dropped region is still retrievable.
- A store a repository shipped, a version-1 store, and a row with a bad or missing MAC render nothing.
- The extractor's captured request contains no tool result, tool call or reasoning; an assistant-only
  claim is `pending` and never shown; a quote not in the conversation is refused.
- A note cannot close its envelope; the learned section is ≤ 400 and the block ≤ 1,500 estimated tokens;
  with nothing learned the block is byte-identical to code-only retrieval.
- `ELEFANTE_LEARN=0` and `ELEFANTE_MEMORY=0` each make zero extractor requests.
- **M0–M2 gates still green.**
- **Credentialed, once:** `live_learned_memory_is_extracted_stored_and_used`.

### M3.1 — Learned-memory hardening and consent

**Shipped** (record in [`MILESTONES.md`](./MILESTONES.md)). Follow-up to M3. It fixes two defects
found in the merged code and changes one default.

**Baseline.** `learn::display_safe` (Unicode categories Cc, Cf, Zl, Zp) applied at ingest and to every
column the CLI prints; `elefante memory list` verifies the store's stamp; the learner clears its failure
state on success. `ELEFANTE_LEARN` becomes `LearnMode { Off, Ask, On }` with **unset meaning Ask**: the
TUI asks once at launch (Y/N, no default, `Esc` asks later), the answer is stored beside the credentials
with a keyed stamp and is bound to the extractor model, and until it is given the daemon reads and keeps
nothing. `elefante memory learn on|off` answers headlessly; `memory.consent` and an additive
`memory: { consent, extractor }` field on the create/attach results carry it.

**Gate.**
- No note, stored or displayed, holds a control or formatting character in any column; an unstamped
  store prints nothing.
- No extractor request is made before consent, on any client; the answer persists, is bound to the
  extractor, is tamper-evident, and `ELEFANTE_LEARN` overrides it.
- An old client ignores the new result field; a new client against a result without it shows no modal.
- Nothing but the launch-time modal opens an overlay or takes focus; with consent granted the screen is
  byte-identical to today's.
- **M0–M3 gates still green.**

### Shipped before the rescope

The memory-facing parts of the old M5, M6 and M7 shipped on 2026-10-11 and stay part of the baseline
every later gate must keep green (records in [`MILESTONES.md`](./MILESTONES.md), under "pre-rescope"):
`MemoryStatus` and the `memory.status` event, lenient clients on both parsers, `View::emit_session`,
one session cost including memory spend; `InjectedContext`, `memory.context` and `memory_note_ids`
on turns; `memory.learned`, `memory.list | forget | approve | clear | status` keyed by `cwd`, the
`/memory` panel and `/forget <id>` in the TUI, and the headless stderr lines. Everything else those
milestones planned was cut (§ Cut in the rescope) or moved into M4–M7 below.

### Consolidation (before M4)

**Implemented** (record in [`MILESTONES.md`](./MILESTONES.md)).

**Baseline.** Eight crates become five. `tools` moves into `core` as `core::tools`. `rpc`, `srv` and
`cli` become one `elefante` crate and one binary: `wire/`, `daemon/` and `client/` modules, with
`elefante daemon` in place of `elefante-srv`, and a client that starts the daemon by running its own
executable rather than looking for a sibling binary. `elefante memory` goes through the daemon only,
starting one if needed; the direct-store path and the "older daemon" fallbacks are deleted. A daemon
that answers "method not found" is named as an older build, with how to stop it, and nothing is
worked around. (A `version` field on create/attach was planned for this and dropped: every crate is
version 0.1.0, so it would have detected nothing.) The scripted stub provider is compiled only into
debug builds. No behaviour visible to the model or the TUI changes.

**Gate.**
- `cargo metadata --no-deps` lists exactly `elefante`, `elefante-core`, `elefante-guard`,
  `elefante-mem`, `elefante-provider`.
- No test is lost: the direct-mode `elefante memory` tests run through the binary and a daemon it
  starts, so the gate items they carried (hostile rows sanitised, an unstamped store shows nothing,
  consent per extractor) still hold.
- With no daemon running, a one-shot prompt and every `elefante memory` subcommand start one and work.
- A daemon that lacks a method is named as an older build, and nothing is written around it.
- A release build contains no `ELEFANTE_STUB_PROVIDER`.
- **M0–M3 gates still green**, credentialed ones included, since the client and daemon paths moved.

### M4 — Sandbox

Today bash runs with the user's full privileges: an allowed command such as `cargo test` executes
repository code (build scripts, tests, proc macros) that can read anything the user can and reach the
network, inherits the daemon's environment — the API key included, when it was exported — and has no
timeout. The classifier cannot fix this: every bypass costs another rule (`known_gaps()` lists eight
classes). M4 moves enforcement to the operating system and lets the classifier go back to intent.

**Threat model.** A prompt-injected model choosing tool calls. M4 keeps shell commands inside a
rooted OS sandbox, without the daemon's key or access to the daemon socket. The harness controls
git writes and web research. Code the user later chooses to run outside elefante — builds, IDE
tasks and hook managers — is outside this guarantee. Kernel exploits are also outside it; a
microVM backend is a proposal.

**The key never enters the sandbox.** The model runs at OpenRouter and every request comes from the
daemon, which is not sandboxed. Sandboxed processes run model-chosen commands and never need the key,
so there is no mapping or proxy to build: the key lives only in the daemon (and `credentials.toml`).

**Baseline.** Design pass first, with a spike on both platforms. The Linux spike uses a
temporary `ubuntu-24.04` Actions job. Phase 0 has separately approved setup, CI-iteration and
results commits: GitHub only dispatches a manual workflow when its file is on the default branch,
so the temporary job runs on `sandboxing` pushes. Probe-harness errors are fixed and rerun before
recording Linux behavior. The maintainer deferred
the billable OpenRouter web-search spike and other credentialed checks until the end of M4.1;
their results remain `not run` through M4 and must be obtained before completion is reported.
- **Policy, in `guard`.** `guard::sandbox::Policy { root, home, harness_dir, protected, temp,
  readable, writable, read_only, hidden, network }`, rendered to a Seatbelt profile (macOS) or bubblewrap arguments and seccomp
  programs (Linux). It is pure and tested by rendering. The root is validated before use: `/`,
  `$HOME`, ancestors of `$HOME`, and paths containing or contained by `~/.elefante` make the
  sandbox unavailable. The per-session temp directory is private (`0700`), outside the root and
  harness state. Readable paths are the root, system directories and exact executable/toolchain
  directories, never `$HOME` or a whole tool root. `.git` directories and validated gitdir targets
  are read-only; `.elefante`, secret names and Cargo credentials are hidden. Hidden paths carry
  an explicit file/directory kind so rendering performs no filesystem lookup. The home,
  harness directory and exact socket/token paths let renderers reject overlapping readable
  mounts, including custom socket locations. Network is off by default.
- **macOS.** `/usr/bin/sandbox-exec -p <profile> -D… bash -c …` with a deny-by-default profile.
  Every path enters through a checked `-D` parameter, never interpolated profile text. Secret
  reads and `.git` writes (in any case) are denied by pattern, including new nested directories.
  Off denies host loopback; approved network enables it. Host process inspection, keychain,
  pasteboard, Apple Events and launchd submission are denied. System reads use exact directories;
  the host's whole `/private/var` is excluded, with required descendants tested during the
  runtime gate. The profile grants exact literal reads of directory ancestors needed for path
  traversal, including `$HOME` when the workspace is beneath it, without granting any home
  subtree.
- **Linux.** Bubblewrap creates user, PID, IPC, UTS and (when network is off) network namespaces,
  clears the environment, binds exact read paths, binds the root writable, overlays each existing
  `.git` read-only, and masks secrets and harness state. `--disable-userns` prevents nested user
  namespaces when available. A `/tmp` tmpfs and private home replace host locations; `/run`, host
  `/tmp` and `$XDG_RUNTIME_DIR` are not bound. The daemon feeds two stacked `seccompiler` programs
  through repeated bubblewrap `--add-seccomp-fd` arguments: ENOSYS for `clone3` and
  `io_uring_*` so runtimes fall back, and EPERM for
  namespace/mount, process-inspection, keyring and other escape-prone calls. On x86_64 both
  programs explicitly reject x32 syscall numbers; `seccompiler` 0.5.0's generated architecture
  check does not cover them. The EPERM program also refuses `ioctl(TIOCSTI)` to prevent
  terminal input injection. The filter restricts
  socket families; approved network still blocks `AF_UNIX` to avoid abstract host sockets. The
  spike settled repeated `--add-seccomp-fd` arguments, Ubuntu's AppArmor user-namespace sysctl
  step, and `--disable-userns` without a clone-flag mask. Its toolchain probe required exact
  Cargo/Bun binary directories, the selected rustup toolchain and settings, a read-only Cargo
  registry, `/etc/alternatives` for GCC and `/etc/ssl/certs` for TLS clients; writable caches
  lived under the private temp directory. No whole real home or Cargo home was bound.
  A new nested `.git` is quarantined after the command, including under ignored directories.
- **Process lifetime.** Every call has a wall-clock timeout and process group. Linux also uses
  bubblewrap's PID namespace and `--die-with-parent` to kill orphaned descendants. On macOS a
  process that leaves the group can survive, but remains sandboxed; the output reader stops two
  seconds after bash exits so a held pipe cannot stall the turn.
- **The daemon's socket requires a token.** At start the daemon writes 128 random bits to
  `~/.elefante/run/token` (`0600`); every connection presents it first, and the CLI and TUI read it
  there. The sandbox cannot read `~/.elefante`, so a command that reaches the socket still cannot
  create a session, resolve an approval, set the key or change memory.
- **Git goes through the harness for writes.** Read git runs in the shell sandbox with
  `GIT_OPTIONAL_LOCKS=0`. A write routes only when the whole bash call is one plain `git`
  command with literal words or single-quoted strings. Pipes, lists, substitutions, expansions,
  redirections, environment prefixes and global options are refused. `guard::git` applies an
  exact option allowlist per local write subcommand (`add`, `rm`, `mv`, `restore`, `commit`,
  `switch`, `checkout`, `branch`, `stash`, `tag`, `merge`, `cherry-pick`, `rebase`) and validates
  paths and refs. `reset`, `clean`, remote/configuration commands, unknown options and aliases
  are refused. Every routed write asks for approval; the disabled sandbox asks for every bash
  call. The harness uses an absolute git binary, a cleared environment and flags that disable
  hooks, fsmonitor, pager, external diff, editors, credential helpers, signing and transports.
  It refuses repositories with in-root include or hooksPath targets or LFS filters. Hooks do not
  run on harness commits. Git attributes can select custom clean/smudge filters or merge drivers,
  so the harness also refuses a repository with any `filter=` or custom `merge=` attribute in
  the worktree, index fallback or `.git/info/attributes`. The invocation disables global
  attributes and an implicit custom merge default. The pure invocation builder also hardens
  indexer git.
- **`web_search`, the eighth tool.** Bash never gets the internet for research; the daemon searches
  instead and returns the results as an ordinary tool result (untrusted content), capped. The query is
  an outbound channel, so it is scrubbed, length-capped and shown to the user. The design pass picks
  the backend, preferring OpenRouter's web search because it needs no second key. There is no
  fetch-a-URL tool: a URL the model chooses is an exfiltration channel.
- **The standing prompt** gains two lines: `.git` is read-only and git runs one command per call with
  no remotes; research goes through `web_search`. It stays within the 400-token budget.
- **Fail closed.** If the sandbox cannot be applied, bash does not run and the error names why.
  `ELEFANTE_SANDBOX=0` is the only opt-out, and the status line shows it while it is set. Nothing in
  the workspace can change the policy: it is built from the daemon's own configuration only.
- **Process and file hygiene.** The environment is cleared to an allowlist (`PATH`, `HOME`,
  `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME`, fixed macOS `SDKROOT`, and
  sandbox-only git flags). On macOS, fixed `PATH` puts the resolved Command Line Tools compiler
  directory ahead of `/usr/bin`: the `/usr/bin/clang` shim created assembler temporaries outside
  the private temp directory even when `TMPDIR` was set, while the direct compiler did not.
  `OPENROUTER_API_KEY` is never passed. Shell uses `bash -c`, not a login shell, with a 120-second
  default timeout. `bash` and `grep` output is capped at 32 KiB, `read` at 128 KiB, keeping head,
  tail and the total size. Daemon file tools and the indexer open root-relative paths without
  following symlinks, after rejecting absolute and parent components; they refuse hard-linked
  regular files and write by same-directory rename. The macOS `rustix` flag for this needs
  `rustix` 1.1.5 or later: 1.1.4 lacks `OFlags::NOFOLLOW_ANY`.
- **Network on request.** `network: true` escalates and, if approved, enables the network for
  that one sandboxed call. It then has the whole internet, with no per-domain filter. macOS host
  loopback needs approval; Linux's private loopback works inside the network-off sandbox and
  cannot reach the host listener. This repository's existing tests bind host loopback, so a
  full `cargo test` run inside the macOS sandbox requires network approval; the off-profile
  probe failed exactly those ten tests and the approved-profile probe passed all 553.
- **The classifier shrinks.** With paths enforced by the OS, the `.elefante` argument scanning in
  `guard/src/bash.rs` (glob forms, grep/rg flag tables) is deleted. The classifier keeps intent:
  destructive and privileged commands are denied, unknown ones ask, plain git is routed.

**Proposals, outside M4's gate.**
- **Landlock inside bubblewrap**, as an additional Linux restriction if it brings measurable
  protection without breaking toolchains.
- **Hooks in the sandbox**, instead of skipping them on harness commits, after their behavior is
  specified and tested.
- **A per-domain egress proxy**, so an approved command reaches only named hosts. On Linux this needs a
  controlled network path rather than the host network namespace.
- **A microVM backend on macOS** (Apple `container`, which targets macOS 26, or Docker Sandboxes)
  behind a `Backend` trait that M4 shapes but does not implement. It isolates the kernel too, and is
  the fallback if Apple removes `sandbox-exec`. gVisor was considered and not chosen: Linux only, so on
  a Mac it needs Docker Desktop's VM, builds a Linux toolchain rather than the one shipped, and Docker's
  privileged daemon is itself a known way out.

**Gate.** The detailed positive and negative controls are in `M4_milestone.md`. They prove the
old classifier harness-state bypasses fail at the OS, outside paths and secrets stay hidden,
network off and approved network differ correctly on both operating systems, `.git` is
read-only in shell and write git routes through the harness, and repository config traps are
refused. The daemon socket requires a token even if reached. Process, key, link, timeout,
output-cap, seccomp and fail-closed cases are exercised. `web_search` rejects secrets and URLs.
Sandbox tests fail with setup instructions if the sandbox is missing, and each negative test has
a positive control. Both macOS and Linux CI legs, MSRV, TUI checks and **all M0–M3 gates** must
be green before M4.1 starts. Credentialed tests are run separately by the maintainer at the end
of M4.1, under the explicit deferral above.

### M4.1 — Sandboxed harness git

**Baseline.** Harness write git runs under its own profile with network off, nothing outside
the root readable, the root writable, and `.git` writable except `hooks/`, `config`, `info/`
and any protected hooksPath target. Linux uses bubblewrap overlays; macOS uses Seatbelt. The
exact per-subcommand allowlist remains as a second line of defence.

**Gate.** A test-only widening of the allowlist admits attempts such as
`--output=$HOME/x`, `--no-index ~/.ssh/f` and `merge -s evil`; the OS still blocks them. A
normal commit and rebase continue to work. M4 and earlier gates remain green on both operating
systems before the milestone is recorded.

### M5 — Critics

**Baseline.** `core::critic` with the `Critic` trait (§ Guard subsystem › Critic) and an injected
provider; the critic and triage roles (§ Role configuration).
- **Triage.** An escalated bash command gets the triage model's verdict beside the approval prompt. A
  pass resolves the approval on its own only while the M4 sandbox is active. `Deny` never reaches it.
- **Diff review.** At the end of a turn that edited files, the critic reviews the session root's
  `git diff` — scrubbed, with denied paths left out. A finding must cite a changed file and line,
  checked mechanically; others are dropped. Findings return as a `user`-role message; revision is
  capped at two rounds.
- **Findings into memory** as `source='critic'` notes, pending until the user approves them with
  `elefante memory approve`, so nothing unconfirmed is ever injected.
- **Lean accounting.** Cost per role (author, extractor, critic, triage) from `usage.cost`; a warning
  when a session's repeated prefixes get zero cache reads; and a test that the standing prompt stays
  under ~400 tokens.

**Gate.**
- A diff seeded with a known defect yields a finding citing the right file; the loop stops at the cap.
- The same request yields schema-valid findings on a model that declares structured output and on
  one that does not.
- A finding citing a line outside the diff is dropped.
- The critic's request contains no secret fixture and no denied path.
- Triage never auto-approves with the sandbox off, and never sees a denied command.
- Reported cost equals the sum of `usage.cost` per role over a scripted session.
- **M0–M4 gates still green.**

### M6 — Subagents

**Baseline.** Design pass first.
- **The `agent` tool** (the ninth): `{ prompt, mode: read_only | edit, max_rounds, max_cost }`,
  returning a summary, the diff stat and the critic's findings.
- **Isolation.** A subagent is a further daemon session. An `edit` subagent works in its own
  `git worktree` under `<repo>/.elefante/worktrees/<id>` — self-ignored, already denied to the parent
  — inside an M4 sandbox that can write only there. A `read_only` subagent gets no worktree and nothing
  writable. Subagents get fewer tools and cannot start subagents of their own.
- **Memory, one way.** A subagent reads the parent repository's index and verified notes; a worktree
  would otherwise resolve to its own store, so it is pointed at the parent's explicitly. It never runs
  the learner: its conversation is model to model, with no user whose words a note could quote.
- **Merge through review.** An `edit` subagent's diff passes the M5 diff critic, then the harness
  applies it to the parent tree (`git apply --3way`) after approval — never the model's bash — and
  removes the worktree.
- **Limits.** Budgets enforced in `core`; at most four subagents at once; interrupting the parent
  cancels its subagents; orphaned worktrees are swept at daemon start.
- **Visibility.** One TUI status line per running subagent; attach to one with `session.attach`.
- **Audit log.** An append-only JSONL record per session, parent and subagents alike, under
  `.elefante/audit/`: every tool call with its full input and output, duration, approval decision,
  and the ids of injected notes. Written through the `Scrubber`; the daemon is the only writer.

**Gate.**
- A subagent's write outside its worktree is blocked by the OS; the parent tree is unchanged until
  the merge, and `git status` afterwards shows only the merged patch.
- A subagent over its budget stops; an interrupted parent leaves no worktree, and one left by a
  `SIGKILL` is swept at the next start.
- A subagent makes zero extractor requests.
- A `read_only` subagent is offered no write tool and has no writable path.
- The audit log holds a tool output longer than the 2 KiB preview in full, and contains no `sk-ant-`
  fixture.
- **M0–M5 gates still green.**

### M7 — Memory depth

**Baseline.**
- **Tombstones.** A forgotten note is never learned again.
- **Consent per repository**, for repositories the user marks sensitive, and a stronger warning
  when the extractor is a different vendor's model from the session's.
- **Notes follow renames**, reusing the rename aliasing `cochange` already computes.
- **Bounded learner history** — today the learner holds the session's text for its whole life.
- **Transcript compaction** that calls `Learner::flush` before dropping history (the seam M3 shipped).
- **Measure, then cut.** Run the agent-memory measurement below; keep or remove the repository brief
  and the in-process retrieval cache (`moka`) according to what it shows.

**Measurement protocol** (manual and credentialed, never a CI gate; `scripts/measure-memory-use.sh`).
Three tasks on a scratch copy of a real repository, two models from different vendors, at least five
runs per cell: (1) "where does X live?", (2) a task whose convention is a saved note the injected block
does not contain — first confirm it is absent from the block and that a reasonable query finds it,
(3) a small edit. Record `memory_search` calls and `grep`/`glob` calls, tool rounds, tokens, cost, and
for (2) whether the convention was applied. A memory-off control (`ELEFANTE_MEMORY=0`) must show no
`memory_search` call and no refusal round. More calls alone is not success: memory earns its place
only if (1) and (2) improve and (3) does not get more expensive.

**Open for the design pass:** what a tombstone keys on (the statement's hash or its words); how a
repository is marked sensitive; whether a cross-vendor extractor needs a second confirmation or only
plainer wording.

**Gate.**
- A forgotten note, said again in the same words, is not saved again.
- A repository with consent off makes zero extractor requests while another repository learns.
- A note anchored to a renamed file still links to it.
- The learner's held text stays under its cap across a long scripted session.
- After compaction, a fact from the dropped history is still retrievable.
- The measurement's results are recorded in `MILESTONES.md`.
- **M0–M6 gates still green.**

---

## Cut in the rescope

Removed on 2026-10-19 because none of them deepens a pillar. Each was planned, none was built, and
reopening one means amending this file first.

- **Model picker, catalogue fetch, pricing display, capabilities from catalogue metadata, guided
  first-run flow, `config.toml` role table, multi-model routing.** Convenience; slugs and environment
  variables suffice.
- **Staleness signals, `session.stale`, the `--sentinel` diagnostic, per-turn effort routing.**
  Accounting beyond cost per role and the cache warning in M5. (The sentinel had already been rejected
  as lagging and mostly unmeasurable; it is no longer kept even as a diagnostic.)
- **Session picker, `session.list` / `resume` / `delete`, durable conversation history, bounded
  snapshots.** Convenience; the audit log in M6 keeps the part that serves security. Snapshot growth
  stays an open protocol question in `DEVELOPER_GUIDE.md` § 14.
- **Datadog log sink.** It would send a user's work to a third party.
- **Plan critic.** A plan has no lines to cite, so its findings cannot be checked mechanically.
- **tmux panes for subagents.** Replaced by sessions with worktrees and `session.attach` (M6).
- **Self-authored skills, hooks and plugins.** Each widens what can run inside the harness.
- **Python and C/C++ extraction, import resolution beyond tier 1, a call graph, `.js`/`.jsx`.**
  Language support is frozen at Rust, TypeScript/TSX and Markdown.
- **The user-level graph (`user.db`).** A second store and a second trust domain that can carry one
  repository's notes into another.
- **Provider beta features** — server-side compaction, context editing, task budgets. M7's compaction
  is client-side so it can flush the learner first; budgets are enforced in `core` (M6).
- **Memory UI extras:** a "learned while you were away" summary, a flag forcing the headless lines,
  approve / history / clear in the panel (the CLI has them), the panel's spend breakdown.
- **Collapsible reasoning in the TUI**, and **embedding-based retrieval** (dropped 2026-10-07).
