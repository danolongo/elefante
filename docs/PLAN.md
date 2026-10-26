# elefante — lightweight coding harness (Rust core + OpenTUI client)

> **Living document.** This is the design of record. When a decision changes during implementation, amend this file in the same change that implements it — a plan that drifts from the code is worse than no plan.

## Context

The repo began empty (one README: "a quick modifiable harness for coding"). The goal is a coding agent harness that can be modified quickly and goes **deep on four pillars rather than wide on features**:

| Pillar | Has today | Gets deeper in |
|---|---|---|
| **Security** | Path policy, bash classifier, output scrubber, keyed memory rows, consent | M4 (environment, key), M5 (critics), M6 (audit log) |
| **Sandboxing** | Nothing — the guard reduces blast radius but is not an OS sandbox | M4, M4.2 (microVM on macOS), M4.3 (microVM on Linux), then reused by M6 |
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

Each milestone closes with a pull request. Its description states what shipped, what the gate proved, and what the milestone deferred. Git history is the record; no file in `docs/` tracks progress.

---

## Decisions made

| Decision | Choice | Rationale |
|---|---|---|
| UI | Rust daemon + Bun/OpenTUI client over JSON-RPC on a unix socket | The same split opencode runs in production, so it is a proven shape rather than one invented here. A headless core is also what lets subagents run as further sessions of the same daemon (M6). |
| Retrieval | SQLite FTS5 + graph hop expansion | No embedding model, no API cost, deterministic and debuggable. Graph edges do the semantic work. |
| Storage | Project graph in `<repo>/.elefante/`, self-ignoring; user-level keys and consent in `~/.elefante/` | Project memory sits next to what it describes; nothing is ever committed, so no session noise reaches a diff. |
| Isolation | OS sandbox (Seatbelt on macOS, bubblewrap + seccomp on Linux) plus a git worktree per editing subagent | Bubblewrap can mask secrets and protect `.git` inside a writable root; Landlock cannot subtract access once granted. Linux requires bubblewrap and unprivileged user namespaces. Harness write git gains its own sandbox in M4.1. An opt-in libkrun microVM backend follows: macOS (Hypervisor.framework) in M4.2, then Linux (KVM) in M4.3. It gives bash its own kernel. |
| Cache | In-process `moka` LRU inside the daemon + SQLite WAL | Redis considered and rejected: subagents all attach to the same daemon, so they already share its cache. Redis would add a required daemon and an IPC hop to beat microsecond page-cache reads. Kept behind a trait for possible cross-machine use later. |
| Provider | OpenRouter only | One transport, one auth, one code path. Reaching Claude via `anthropic/claude-opus-5` means this narrows the *code*, not the *models*. `Capabilities` survives and matters more, now resolved per model. |

---

## Crate topology and dependency direction

```
elefante/
  Cargo.toml                 # workspace
  docs/PLAN.md               # this document
  docs/M4_milestone.md       # the current milestone's phases and gate tests
  docs/DEVELOPER_GUIDE.md    # shipped behaviour, incl. the wire contract (§5)
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

Five crates, after the consolidation (§ Milestones › Shipped): eight was more seams than the pillars need. `tools` became `core::tools`; `rpc`, `srv` and `cli` became one `elefante` crate and one binary. Client and daemon now share a crate, so one rule the old graph enforced is now a rule of review: **the client never runs a turn** — it always goes through the socket.

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
| web search | `ELEFANTE_SEARCH_MODEL` | M4 |
| critic | `ELEFANTE_CRITIC_MODEL`, `ELEFANTE_CRITIC_EFFORT` | M5 |
| bash triage | `ELEFANTE_TRIAGE_MODEL`, `ELEFANTE_TRIAGE_EFFORT` | M5 |

The search default follows the same fallback until the end-of-M4.1 S9 probe; that probe may pick a
cheaper default. No `config.toml` role table: a handful of variables do not need a file format. The critic should be a
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

1. **`PathPolicy` at the tool path resolver.** Every filesystem tool calls `policy.resolve(path)?`, which canonicalizes *then* matches — defeating `foo/../.env` and symlinks. It lives in `tools`, directly above the syscall, so a newly added tool cannot accidentally bypass it. Set: `.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `.netrc`, `.npmrc`, `.git-credentials`, `.aws/credentials`, `.cargo/credentials`, `.cargo/credentials.toml`, `.ssh/**`, user-extensible. Applies to read, glob, **and grep** — grep is the one people forget, and it leaks contents just as readily.
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

### Shipped

These milestones have shipped. Their gates now live in the test suite, and every later gate keeps
them green. Pull requests and `git log` hold the detail.

| Milestone | What shipped |
|---|---|
| M0 — Loop | `Provider` trait, OpenRouter transport, the agent loop, seven tools, `PathPolicy`, bash classifier, scrubber |
| M1 — Daemon + TUI | JSON-RPC daemon on a unix socket, headless client, OpenTUI client, attach and resume, approvals, first-run key setup |
| M2 — Repo cache + mechanical graph | blake3 walk cache, tree-sitter symbols, import and co-change edges, FTS5 + graph-hop retrieval, `memory_search` |
| M3 — Learned memory | Learned notes with verification, speaker provenance, dedupe, supersession and keyed rows; `elefante memory` |
| M3.1 — Hardening and consent | `display_safe`, store stamp checks, `LearnMode { Off, Ask, On }` with consent bound to the extractor model |
| Pre-rescope memory UI | `memory.status`, `memory.context`, the `/memory` panel, `/forget <id>`, headless `memory:` lines |
| Consolidation | Eight crates became five and one `elefante` binary; the stub provider exists only in debug builds |

`cargo test --workspace` and `bun test` run the hermetic gates. The credentialed gates are
`live_second_identical_request_reads_prompt_cache`, `live_edit_lands_in_a_scratch_repository`,
`live_memory_context_reaches_the_model` and `live_learned_memory_is_extracted_stored_and_used`;
`.github/workflows/credentialed.yml` runs them.

### M4 — Sandbox

Today bash runs with the user's full privileges: an allowed command such as `cargo test` executes
repository code (build scripts, tests, proc macros) that can read anything the user can and reach the
network, inherits the daemon's environment — the API key included, when it was exported — and has no
timeout. The classifier cannot fix this: every bypass costs another rule (`known_gaps()` lists eight
classes). M4 moves enforcement to the operating system and lets the classifier go back to intent.

**Threat model.** A prompt-injected model choosing tool calls. M4 keeps shell commands inside a
rooted OS sandbox, without the daemon's key or access to the daemon socket. The harness controls
git writes and web research. Code the user later chooses to run outside elefante — builds, IDE
tasks and hook managers — is outside this guarantee. Kernel exploits are also outside it; the
opt-in microVM backend of M4.2 (macOS) and M4.3 (Linux) addresses them.

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
The policy phase needed a separately approved gate-fix commit: pinned Linux Clippy saw two
Linux-only lint errors after the first push. No runtime phase begins until the Linux gate reruns
green; the test requirements are unchanged.
The runtime phase moves Linux CI's bubblewrap and SQLite installation and user-namespace probe
ahead of the later two-OS matrix change, because runtime tests must exercise isolation before
that phase can pass its gate.
- **Policy, in `guard`.** `guard::sandbox::Policy { root, home, harness_dir, protected, temp,
  readable, writable, read_only, hidden, network }`, rendered to a Seatbelt profile (macOS) or bubblewrap arguments and seccomp
  programs (Linux). It is pure and tested by rendering. The root is validated before use: `/`,
  `$HOME`, ancestors of `$HOME`, paths containing or contained by `~/.elefante`, and paths with a
  case-insensitive `.git` component make the
  sandbox unavailable. The per-session temp directory is private (`0700`), outside the root and
  harness state. Readable paths are the root, system directories and exact executable/toolchain
  directories, never `$HOME` or a whole tool root. `.git` directories and validated gitdir targets
  are read-only; `.elefante`, secret names and Cargo credentials are hidden. Linux mounts exact
  resolver files, not all of `/etc`, so approved DNS can use libc. Hidden paths carry an explicit
  file/directory kind so rendering performs no filesystem lookup. The home,
  harness directory and exact socket/token paths let renderers reject overlapping readable
  mounts, including custom socket locations. Network is off by default.
  Bubblewrap's `--dev /dev` supplies writable standard device nodes; the readable mount loop
  must not overlay `/dev/null`, `/dev/random` or `/dev/urandom` read-only.
  Its minimal PID 1 reaper is masked with a tmpfs overlay on `/proc/1`, so shell tools cannot
  inspect that parent process even though the PID namespace still reaps children.
- **macOS.** `/usr/bin/sandbox-exec -p <profile> -D… bash -c …` with a deny-by-default profile.
  Every path enters through a checked `-D` parameter, never interpolated profile text. Secret
  reads and `.git` writes (in any case) are denied by pattern, including new nested directories.
  An exact literal write exception for `/dev/null` lets Git reads open the cleared global config
  target read-write; no general out-of-root write grant is made.
  A Git read with `--output=<outside-scratch-path>` must still fail at the OS; `$HOME` inside
  the sandbox is private session temp, so it is not a suitable outside-root test target.
  Off denies host loopback; approved network enables outbound TCP/UDP and the exact system DNS
  socket while other Unix sockets remain restricted to the root and session temp. Host process inspection, keychain,
  pasteboard, Apple Events and launchd submission are denied. System reads use exact directories;
  the host's whole `/private/var` is excluded, with required descendants tested during the
  runtime gate. The profile grants exact literal reads of the ancestors of the root, temp,
  readable and writable paths, including `$HOME` when the workspace is beneath it. A command can
  list the names in those ancestors, but the profile grants no home subtree. Seatbelt applies
  the last matching rule, so every allow precedes every deny.
- **Linux.** Bubblewrap creates user, PID, IPC, UTS and (when network is off) network namespaces,
  clears the environment, binds exact read paths, binds the root writable, overlays each existing
  `.git` read-only, and masks secrets and harness state. `--disable-userns` prevents nested user
  namespaces when available. A `/tmp` tmpfs and private home replace host locations; `/run`, host
  `/tmp` and `$XDG_RUNTIME_DIR` are not bound. The renderer rejects a readable path under them,
  under `/var/run` or under `/dev`, apart from the device nodes, the resolver directory and
  validated git directories. The daemon feeds two stacked `seccompiler` programs
  through repeated bubblewrap `--add-seccomp-fd` arguments. It writes each program into an
  anonymous memory file that keeps close-on-exec. The fixed `bash` launcher reopens both files
  as fds 3 and 4 in the launched process only, then execs bubblewrap. The programs return
  ENOSYS for `clone3` and `io_uring_*` so runtimes fall back, and EPERM for
  namespace/mount, process-inspection, keyring and other escape-prone calls. On x86_64 both
  programs explicitly reject x32 syscall numbers; `seccompiler` 0.5.0's generated architecture
  check does not cover them. The EPERM program also refuses `ioctl(TIOCSTI)` to prevent
  terminal input injection. That rule compares the low 32 bits of the request, as the kernel
  does. The filter restricts
  socket families; approved network still blocks `AF_UNIX` to avoid abstract host sockets. The
  spike settled repeated `--add-seccomp-fd` arguments, Ubuntu's AppArmor user-namespace sysctl
  step, and `--disable-userns` without a clone-flag mask. Its toolchain probe required exact
  Cargo/Bun binary directories, the selected rustup toolchain and settings, a read-only Cargo
  registry, `/etc/alternatives` for GCC and `/etc/ssl/certs` for TLS clients; writable caches
  lived under the private temp directory. No whole real home or Cargo home was bound.
  A new nested `.git` is quarantined after the command, including under ignored directories.
  If a move fails, the next root scan refuses until the entry is gone. On every OS, a `.git` that
  was absent at session start makes bash and harness git refuse, and in-root include or hooksPath
  targets are read-only in the shell. The scan also reads the config of a repository that encloses
  the root.
- **Process lifetime.** Every call has a wall-clock timeout and process group. Linux also uses
  bubblewrap's PID namespace and `--die-with-parent` to kill orphaned descendants. On macOS a
  process that leaves the group can survive, but remains sandboxed; the output reader stops two
  seconds after bash exits so a held pipe cannot stall the turn.
- **The daemon's socket requires a token.** At start the daemon writes 128 random bits beside its
  socket (`~/.elefante/run/token` by default, `0600`); every connection presents it first, and the
  CLI and TUI read it there. The sandbox cannot read the token, so a command that reaches the
  socket still cannot create a session, resolve an approval, set the key or change memory.
- **Git goes through the harness for writes.** Read git runs in the shell sandbox with
  `GIT_OPTIONAL_LOCKS=0`. A write routes only when the whole bash call is one plain `git`
  command with literal words or single-quoted strings. Pipes, lists, substitutions, expansions,
  redirections, environment prefixes and global options are refused. `guard::git` applies an
  exact option allowlist per local write subcommand (`add`, `rm`, `mv`, `restore`, `commit`,
  `switch`, `checkout`, `branch`, `stash`, `tag`, `merge`, `cherry-pick`, `rebase`) and validates
  paths and refs. `reset`, `clean`, `push`, `fetch`, `pull`, `config` writes and unknown options
  are refused, also behind wrappers such as `env`, `exec` or `xargs`. `checkout <ref>` runs as
  `checkout <ref> --`, so git never reads the ref as a pathspec, and a branch may share its
  name with a file or directory. Every other subcommand, such as
  `grep`, `blame` or an alias, is a read and runs in the sandbox. Every routed write asks for approval; the disabled sandbox asks for every bash
  call. The harness uses an absolute git binary, a cleared environment and flags that disable
  hooks, fsmonitor, pager, external diff, editors, credential helpers, signing and transports.
  It acts only on the `root/.git` present at session start and shares the bash timeout. It
  refuses in-root or `~/` include or hooksPath targets and LFS filters. Hooks do not
  run on harness commits. Git attributes can select custom clean/smudge filters or merge drivers,
  so the harness also refuses a repository with any `filter=` or custom `merge=` attribute in
  the worktree, index fallback or `.git/info/attributes`. The invocation disables global
  attributes and an implicit custom merge default. Local repository config defining executable
  `filter.*` or `merge.*.driver` is refused too, because a target revision may introduce
  attributes during checkout. The pure invocation builder also hardens
  indexer git.
- **`web_search`, the eighth tool.** Bash never gets the internet for research; the daemon searches
  instead and returns the results as an ordinary tool result (untrusted content), capped. The query is
  an outbound channel, so it is scrubbed, length-capped and shown to the user. The design pass picks
  the backend, preferring OpenRouter's web search because it needs no second key. There is no
  fetch-a-URL tool: a URL the model chooses is an exfiltration channel.
- **The standing prompt** gains two lines: `.git` is read-only and git runs one command per call with
  no remotes; research goes through `web_search`. It stays within the 400-token budget. The
  per-session search cap is reserved in `check_with_tools` before parallel dispatch, using state
  shared by executor clones; `check_in` remains the pure bash policy entry point.
- **Fail closed.** If the sandbox cannot be applied, bash does not run and the error names why.
  `ELEFANTE_SANDBOX=0` is the only opt-out, and the status line shows it while it is set. Nothing in
  the workspace can change the policy: it is built from the daemon's own configuration only.
- **Process and file hygiene.** The environment is cleared to an allowlist (`PATH`, `HOME`,
  `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, `CARGO_HOME`, `RUSTUP_HOME`, fixed macOS `SDKROOT`, and
  sandbox-only git flags). On macOS, fixed `PATH` puts the resolved Command Line Tools compiler
  directory ahead of `/usr/bin`: the `/usr/bin/clang` shim created assembler temporaries outside
  the private temp directory even when `TMPDIR` was set, while the direct compiler did not.
  `OPENROUTER_API_KEY` is never passed. A fixed `bash` launcher closes every descriptor above
  stderr before it execs the sandbox, so a descriptor the daemon inherited never reaches a
  command. Shell uses `bash -c`, not a login shell, with a 120-second
  default timeout. `bash` and `grep` output is capped at 32 KiB, `read` at 128 KiB, keeping head,
  tail and the total size. Daemon file tools and the indexer open root-relative paths without
  following symlinks, after rejecting absolute and parent components; they refuse hard-linked
  regular files and write by same-directory rename. The macOS `rustix` flag for this needs
  `rustix` 1.1.5 or later: 1.1.4 lacks `OFlags::NOFOLLOW_ANY`.
- **Network on request.** `network: true` escalates and, if approved, enables the network for
  that one sandboxed call, including an otherwise unclassified command such as `curl`; denied
  commands stay denied and harness Git stays network-off. It then has the whole internet, with no per-domain filter. macOS host
  loopback needs approval; Linux's private loopback works inside the network-off sandbox and
  cannot reach the host listener. This repository's existing tests bind host loopback, so a
  full `cargo test` run inside the macOS sandbox requires network approval; the off-profile
  probe failed exactly those ten tests and the approved-profile probe passed all 553.
- **The classifier shrinks.** With paths enforced by the OS, the `.elefante` argument scanning in
  `guard/src/bash.rs` (glob forms, grep/rg flag tables) is deleted. The classifier keeps intent:
  destructive and privileged commands are denied, unknown ones ask, plain git is routed. A
  directory-only `cd` may name a protected directory so the OS denial is exercised, while
  literal content readers still deny; only read-only `sqlite3 <db> .schema` and exact `xargs cat`
  are newly allowed for the bypass gate. When the sandbox is disabled, every Bash call escalates
  before any of these rules apply.

**Proposals, outside M4's gate.**
- **Landlock inside bubblewrap**, as an additional Linux restriction if it brings measurable
  protection without breaking toolchains.
- **Hooks in the sandbox**, instead of skipping them on harness commits, after their behavior is
  specified and tested.
- **A per-domain egress proxy**, so an approved command reaches only named hosts. On Linux this needs a
  controlled network path rather than the host network namespace.

The microVM backend, formerly a proposal here, is required work: M4.2 and M4.3.

**Gate.** The detailed positive and negative controls are in `M4_milestone.md`. They prove the
old classifier harness-state bypasses fail at the OS, outside paths and secrets stay hidden,
network off and approved network differ correctly on both operating systems, `.git` is
read-only in shell and write git routes through the harness, and repository config traps are
refused. The daemon socket requires a token even if reached. Process, key, link, timeout,
output-cap, seccomp and fail-closed cases are exercised. `web_search` rejects secrets and URLs.
The socket gate uses a fake home and synthetic runtime/override sockets; the `/run` case is a
pure no-broad-mount assertion plus runtime outside/abstract-socket controls, because CI does not
guarantee a writable `/run` location for a listener. This retains the service boundary without
opening or disturbing the real daemon socket.
The current macOS profile's Keychain, clipboard, app-launch, launchd and Apple Event controls
run only in a dedicated disposable macOS CI job with synthetic fixtures, not on the maintainer's
machine. The job invokes explicitly ignored tests; if the runner lacks a required GUI service,
the gate fails with a setup reason and is reported rather than silently skipped.
Sandbox tests fail with setup instructions if the sandbox is missing, and each negative test has
a positive control. Both macOS and Linux CI legs, MSRV, TUI checks and **all M0–M3 gates** must
be green before M4.1 starts. Credentialed tests are run separately by the maintainer at the end
of M4.1, under the explicit deferral above.

### M4.1 — Sandboxed harness git

**Baseline.** Harness write git runs under its own profile with network off, nothing outside
the root readable, the root writable, and `.git` writable except `hooks/`, `config`, `info/`
and any protected hooksPath target. Linux uses bubblewrap overlays; macOS uses Seatbelt. The
exact per-subcommand allowlist remains as a second line of defence.

**`web_search` egress.** M4 lets a session send up to 30 queries of 300 characters each with
no approval. A prompt-injected model can therefore send about 9 KB of non-secret file content
out through queries. The URL filter (`://`, `www.`) is a courtesy, not a control: a `[.]`
spelling or base64 passes it. M4.1 closes the gap:
- The first `web_search` of each session escalates. The approval prompt shows the query.
- The threat model in `DEVELOPER_GUIDE.md` states the channel and its size bound.

**Gate.** A test-only widening of the allowlist admits attempts such as
`--output=$HOME/x`, `--no-index ~/.ssh/f` and `merge -s evil`; the OS still blocks them. A
normal commit and rebase continue to work. The first `web_search` in a session escalates and
shows its query; a later one within the cap runs without a prompt. M4 and earlier gates remain
green on both operating systems before the milestone is recorded.

### M4.2 — MicroVM backend on macOS

Seatbelt and bubblewrap share the host kernel, so a kernel exploit escapes them. M4.2 runs bash
in a Linux microVM with its own kernel, using libkrun: a Rust VMM library on Hypervisor.framework
(Apple silicon) and on KVM, so M4.3 reuses the same backend on Linux. It is also the fallback if
Apple removes `sandbox-exec`.

Considered and not chosen:
- **Apple `container`.** It is macOS 26 only and macOS only, so Linux would need a second
  backend. Its CLI also gives less control over shares and network devices.
- **gVisor.** Linux only, so on a Mac it needs Docker Desktop's VM, and Docker's privileged
  daemon is itself a known way out.
- **Firecracker.** It needs KVM and has no macOS port.

**Opt-in for now.** Seatbelt stays the macOS default; `ELEFANTE_SANDBOX=microvm` selects the VM,
and the status line shows it. **The microVM should become the macOS default in the near future**,
once M4.2's measurements show its start-up cost and the Linux toolchain are acceptable for
everyday use.

**What moves into the VM.** Only a bash call and everything it starts. The daemon, the key, the
TUI, the file tools, harness git, the indexer and `web_search` stay on the host. Inside the guest,
commands build and run Linux tools, so `cargo build` there produces Linux binaries.

**Baseline.** Design pass first, with a spike. Its results go into the milestone file before any
phase begins.
- **A `Backend` seam** in `core` with Seatbelt, bubblewrap and the microVM behind it, selected by
  the daemon's configuration only, never by the workspace.
- **libkrun never runs in the daemon.** It runs in the process that links it, so a VM escape
  inside the daemon would land beside the key. A separate VM-runner child links it, receives no
  key, and itself runs under Seatbelt with only the shares it serves.
- **A pinned stable libkrun branch.** `main` is an unstable 2.0. The runner is signed with the
  hypervisor entitlement that Hypervisor.framework requires.
- **We own the guest:** kernel, root filesystem with the Linux toolchain, and a minimal init.
- **The same `Policy`.** Network off attaches no network device at all; libkrun's socket
  forwarding (TSI) uses the runner's host network, so it is never enabled for an off call.
  Approved network gives the guest outbound access for that one call. The key never enters the
  guest; the environment is the same allowlist. Timeouts stop the command and the VM.
- **Bubblewrap and seccomp inside the guest**, as the second layer, so the M4 rules still hold
  when the VM boundary is not the one under test.
- **Fail closed.** If the microVM is selected but unavailable (unsupported macOS, Intel Mac,
  libkrun or the guest image missing), bash does not run and the error names why. It never
  silently falls back to Seatbelt.

**Open for the design pass (spike questions):**
- libkrun's minimum macOS version on the pinned branch.
- Cold and warm start time per call and per session; whether one VM per session is needed to
  keep calls fast.
- Can the virtio-fs shares keep `.git` read-only and secrets and `.elefante` absent at the host
  side, not only inside the guest?
- How the guest kernel and image are built, versioned and kept small.
- Shared-folder speed for `cargo test` and `bun test` on this repository.
- Linux build output must not clobber the host's: `target/` and native `node_modules` need a
  guest-private location.
- Does a GitHub-hosted macOS runner, itself a VM, support Hypervisor.framework guests? If not,
  where the gate runs.

**Gate.**
- Inside the VM, `uname -s` is Linux and host processes are invisible.
- The M4 negative controls pass with the microVM backend: outside paths and secrets hidden, `.git`
  read-only, network off and approved differ, the daemon socket refuses without the token, the
  timeout kills the command.
- Selecting the microVM where it is unavailable fails closed with a setup reason.
- With the microVM not selected, Seatbelt behaves exactly as in M4.
- **M0–M4.1 gates still green.**

### M4.3 — MicroVM backend on Linux

**Baseline.** The M4.2 libkrun backend on Linux over KVM, opt-in on the same setting, behind the
same `Backend` seam, with the same runner process, fail-closed rule and gate. The runner process
is confined by bubblewrap and seccomp instead of Seatbelt. The design pass confirms `/dev/kvm` on
the CI runner.

**Gate.** M4.2's gate on Linux, and **M0–M4.2 gates still green.**

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
- M7's pull request states the measurement's results.
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
