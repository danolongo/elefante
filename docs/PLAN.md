# elefante — lightweight coding harness (Rust core + OpenTUI client)

> **Living document.** This is the design of record. When a decision changes during implementation, amend this file in the same change that implements it — a plan that drifts from the code is worse than no plan. Milestone outcomes are recorded in [`MILESTONES.md`](./MILESTONES.md).

## Context

The repo began empty (one README: "a quick modifiable harness for coding"). The goal is a coding agent harness that can be modified quickly, built around four convictions:

1. **Minimal standing prompt.** Behavioral rules that can be enforced mechanically should be code — hooks, gates, validators — not sentences in a system prompt. Target: base system prompt under ~400 tokens.
2. **Graph memory as first-class substrate**, separately for the user and for each project, with a content-addressed cache so repo knowledge is warm in milliseconds.
3. **Adversarial review built into the loop**: a gate before sensitive bash, a critic on plans, a critic on diffs.
4. **Incremental**, with real gates between increments.

This document covers the **core**. Deferred features are listed at the end so they inform architecture without being built yet.

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
| UI | Rust daemon + Bun/OpenTUI client over JSON-RPC on a unix socket | The same split opencode runs in production, so it is a proven shape rather than one invented here. A headless core is also what makes tmux subagents work later. |
| Retrieval | SQLite FTS5 + graph hop expansion | No embedding model, no API cost, deterministic and debuggable. Graph edges do the semantic work. Pluggable vector slot left open. |
| Storage | User graph in `~/.elefante`, project graph in `<repo>/.elefante/`, all gitignored | Project memory sits next to what it describes; nothing is ever committed, so no session noise reaches a diff. |
| Cache | In-process `moka` LRU inside the daemon + SQLite WAL | Redis considered and rejected: subagents all attach to the same daemon, so they already share its cache. Redis would add a required daemon and an IPC hop to beat microsecond page-cache reads. Kept behind a trait for possible cross-machine use later. |
| Provider | OpenRouter only | One transport, one auth, one code path. Reaching Claude via `anthropic/claude-opus-5` means this narrows the *code*, not the *models*. `Capabilities` survives and matters more, now resolved per model. |
| Staleness detection | Deterministic metrics, **not** a `"working"` sentinel token | See "Rejected: response sentinel". |

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
    rpc/                     # wire types shared by daemon and client (no deps on others)
    provider/                # Provider trait + Capabilities; sse decoder, openrouter
    guard/                   # PathPolicy, BashClassifier, Scrubber, Critic
    tools/                   # read, write, edit, glob, grep, bash, memory_search
    mem/                     # graph store, indexer, retrieval, cache
    core/                    # session, turn loop, context assembly, staleness, compaction
    srv/                     # daemon binary: socket, session registry
    cli/                     # `elefante` binary: spawns daemon, launches client
  tui/                       # Bun + @opentui/core client (TypeScript)
```

**Dependency direction is a constraint, not an accident:**

```
core  ──► provider, tools, mem, guard, rpc
tools ──► guard            (PathPolicy at the syscall boundary)
mem   ──► guard            (Scrubber on the write path)
guard ──► provider         (Critic needs a model; injected, never constructed internally)
rpc   ──► (nothing)
```

`guard` must never depend on `tools` or `mem`, or the graph cycles. The practical consequence: `guard` is pure policy — it decides, it never performs. This is what makes the whole gate layer unit-testable without a filesystem or a network.

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
provider shape. `Capabilities` gains core-side callers later, where the branch is genuinely
core's: `mid_conversation_system` and `structured_output`, both for critic findings (M4). (Repository
context does *not* use `mid_conversation_system`: it is a `user` message on purpose — see step 3.)

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

Roles are provider-agnostic and configured, not hardcoded — **author**, **critic**, **bash triage** each name a `(provider, model, effort)` triple in `~/.elefante/config.toml`, overridable per project.

With one provider a role is just `(model, effort)`. M0 ships only `OPENROUTER_MODEL` + `ELEFANTE_EFFORT`; the `config.toml` role table arrives when a second role actually exists — the bash triage critic in M4. Building it in M0 would be configuration with one consumer.

Starting config: author = `anthropic/claude-sonnet-5` @ `xhigh`; critic = a *different vendor's* model, which is what makes review adversarial rather than self-agreeing; triage = anything cheap and fast. Per-model pricing no longer needs a local table — `usage.cost` comes back on every response.

### First-run key setup

**Planned, not built.** Today the harness requires `OPENROUTER_API_KEY` to be exported
before launch, and `.env.example` in the repo root is an interim convenience for that.

The intended behaviour: on first launch with no key resolvable, the harness **prompts the
user for an OpenRouter key** and stores it under `~/.elefante/`, outside any working tree.
Three reasons this is the right shape rather than a nicety:

- A key in `<repo>/.env` sits inside the tree the agent edits. Elefante's own path policy
  already refuses to read it, which is correct but means the file exists purely to be
  stepped around. Storing the key in `~/.elefante/` removes it from the blast radius
  instead of guarding it there.
- It removes the only setup step between cloning and a working harness. Everything else has
  a default.
- The key never enters a repo, so it cannot be committed, and `.env.example` can be deleted.

**Lands in M1**, because it needs an interactive transport — the same daemon/TUI channel
that carries bash-approval prompts. A prompt has nowhere to render before then. Until it
ships, a missing key must keep producing the named, graceful error M0's gate requires,
never a bare transport failure.

Open questions for the M1 design pass: file permissions and format for the stored key
(`0600`, and whether it belongs in `config.toml` or a separate credentials file); whether
to offer the OS keychain; and whether an already-set `OPENROUTER_API_KEY` environment
variable should win over the stored key (it should — env beats config everywhere else here).

### First-run setup flow

**Planned, not built.** The first-run key prompt grows into a short guided setup that runs once,
when the harness is launched interactively with nothing configured. Three steps, in this order,
because each needs the one before it:

1. **API key.** The prompt described above (`key.set` already carries it over the wire). It comes
   first because the next two steps list models, and the catalogue is fetched with the user's key.
   A successful catalogue fetch doubles as proof the key works.
2. **Default agent model.** One sentence beside the picker: *"The model that does your coding work
   in each session — it reads, edits and runs commands, so it drives quality and cost."*
3. **Default memory extractor model.** One sentence beside the picker: *"A background model that
   reads excerpts of your conversations to note lasting preferences and project conventions; a
   cheaper, faster model is usually enough."* The choices include **"same as the agent model"**,
   which is also what an unset extractor falls back to. Because excerpts of the conversation are
   sent to whichever model is chosen, the sentence must say so; the user should never learn that
   from the bill.

Both pickers are the model picker described below (user-scoped catalogue, pricing shown before
the choice), not free text. Pressing Enter accepts a default at every step, so the whole flow is
skippable, and none of it runs headless: a one-shot prompt keeps reading the environment only.

Where it lands: the answers become the first rows of the `config.toml` role table
(`[roles.author]`, `[roles.extractor]`), which is exactly the table § Role configuration defers
until a second role exists. The extractor is that second role. **Precedence is environment, then
`config.toml`, then fallback to the author model**, matching "env beats config everywhere else
here". The learned-memory milestone ships the extractor role as environment variables only
(`ELEFANTE_EXTRACT_MODEL`, `ELEFANTE_EXTRACT_EFFORT`); this flow later becomes their home, and the
variables stay as overrides.

M3.1's launch-time consent question is the stopgap for step 3 until this flow exists; when it lands,
step 3's sentence *is* the consent question, and answering the picker records the same stamped consent
bound to the chosen model. **The stopgap has shipped** (M3.1, below): the TUI asks once, the answer is
stored in `~/.elefante/memory-consent` bound to the extractor, and `elefante memory learn on|off` answers
headlessly. Steps 1 and 2, the pickers, and the whole flow remain unbuilt.

Open for its design pass: whether the flow is re-runnable (a `setup` subcommand or TUI command)
so a changed mind does not mean editing a file by hand, and whether the critic and triage roles
join the flow when they exist or stay in `config.toml` as advanced settings.

### Model selection

**Planned, not built.** A model is currently chosen by typing a slug into `OPENROUTER_MODEL`,
which requires knowing the catalogue by heart and gives no view of cost. The intended
behaviour is an interactive picker listing the models the user can actually reach, with
pricing shown before the choice is made.

**The catalogue must come from `GET /api/v1/models/user`, authenticated with the user's key
— not the public `GET /api/v1/models`.** The user-scoped endpoint filters by that account's
provider preferences, privacy settings, and guardrails. The public list would happily offer
models the user's own account then refuses, which turns a configuration choice into a runtime
failure. Paginate via `links.next`; the catalogue runs to hundreds of entries.

**Default is `openrouter/auto`**, with a `?` affordance explaining the cost, because auto is
the one row whose price cannot be printed: it routes each prompt among a curated set and
**bills at the rate of whichever model it selects**, so its cost is variable by request rather
than a number. The explanation should say that plainly, and should also head off the common
misreading that the `:free` variant is free — it is not.

**Pricing display.** `pricing.prompt` and `pricing.completion` are strings in **USD per
token**; multiply by 1e6 for the conventional $/1M figures. Two rules follow:

- This table is for *choosing*, never for accounting. Actual spend is read from `usage.cost`
  on each response, which already accounts for cache discounts and the model auto actually
  picked. Computing cost from the price table would drift from the bill.
- `pricing` also carries `input_cache_read` and `input_cache_write`, which is the honest way
  to show that a cheap-looking model without caching can cost more in a long agent session
  than a pricier one with it.

**Show `supported_parameters`, filter by default, but do not hide.** Supplying tool
definitions is necessary and not sufficient: the model still has to *emit* `tool_calls`. One
never trained for function calling describes its intent in prose instead, the loop sees no
tool calls, terminates after a round, and the agent quietly does nothing. The infrastructure
is fine — the model cannot drive it. So a model not listing `tools` is worth flagging.

It is not worth *hiding*, though. `supported_parameters` is advisory metadata rather than a
guarantee, OpenRouter's own routing already biases toward tool-capable endpoints, and a
picker that silently omits rows misrepresents the catalogue the user is paying for. Default
the filter on, make it a toggle, and say why a row is excluded.

`reasoning` support belongs in the row too, since it decides whether the effort setting does
anything. This is also where an unrecognised slug's conservative all-false capability
resolution becomes visible to the user rather than silently degrading.

Cache the fetched catalogue under `~/.elefante/` with a TTL, keyed by the API key: it is
large, it changes slowly, and refetching hundreds of entries to redraw a menu is waste.

### Capabilities from metadata, not prefixes

The current resolver matches hardcoded slug prefixes and falls back to all-false. That was
the right shape when nothing could be looked up, but the catalogue the picker fetches
already carries every axis, so the prefix table should become the **offline fallback** and
metadata the primary source:

| Capability | Derived from |
|---|---|
| `adaptive_thinking` | `supported_parameters` contains `reasoning` |
| `effort` | `supported_parameters` contains `reasoning_effort` |
| `structured_output` | `supported_parameters` contains `structured_outputs` |
| `prompt_caching`, `cache_metrics` | `pricing.input_cache_read` is present — no `supported_parameters` entry exists for caching, so pricing is the only signal |
| `mid_conversation_system` | transport property; always true. No core caller until M4 |

This removes the maintenance treadmill of tracking new slugs by hand, and it is accurate per
model rather than per family. The prefix table stays for the cold-start and offline cases,
where guessing conservatively still beats failing.

`pricing.prompt == "-1"` is the sentinel for **variable pricing** — how the picker detects
that a row has no fixed price, generically, rather than special-casing a slug by name.

**Depends on the interactive channel**, like first-run key setup, and naturally follows it —
key first, then model. Until it exists, `OPENROUTER_MODEL` remains the way to choose.

Open questions for the design pass: whether to show all reachable models or a curated
shortlist with a "show everything" toggle; whether the picker sets a project default, a user
default, or both; and whether to surface `top_provider.is_moderated`, which changes behaviour
in ways a coding user would want to know about before choosing.

---

## Process topology

```
  ┌──────────────┐                     ┌──────────────────────────────┐
  │ elefante-cli │──spawns if absent──►│        elefante-srv          │
  └──────┬───────┘                     │  ┌────────────────────────┐  │
         │ spawns                      │  │   Session registry     │  │
         ▼                             │  │  (many sessions, 1..n  │  │
  ┌──────────────┐   JSON-RPC over     │  │   clients per session) │  │
  │ Bun + OpenTUI│◄──unix socket ─────►│  └────────────────────────┘  │
  │   (tui/)     │   framed NDJSON     │   provider clients (shared)  │
  └──────────────┘                     │   mem stores (lazy per repo) │
                                       │   moka cache · tool executor │
  ┌──────────────┐                     │                              │
  │ tmux pane N  │◄────────────────────┤   (later: subagent sessions) │
  └──────────────┘                     └──────────────────────────────┘
```

Socket at `$XDG_RUNTIME_DIR/elefante.sock`, falling back to `~/.elefante/run/elefante.sock`, mode `0600`. The CLI connects, and spawns the daemon only if the connect fails. The daemon exits after a configurable idle period with zero attached clients.

**Why the daemon exists even for single-user CLI use:** it is what later lets N tmux panes attach to one warm memory store and one set of provider connections. Retrofitting that onto an in-process design is the expensive path, so the split is paid for up front and is cheap now.

### Session picker

**Planned, not built.** Launching the TUI always creates a fresh session. The intended behaviour
is a menu, scoped to the current repository, listing the conversations that have happened there
with "new session" first, so a user can resume earlier work instead of starting cold.

**What the current structure does and does not allow** (verified against `srv/src/server.rs`):

- **One daemon per socket, by construction.** `bind()` takes an exclusive lock on the socket's
  parent directory, probes the socket, and a second `elefante-srv` that finds a live daemon
  returns without serving. A second daemon needs a different `ELEFANTE_SOCKET`, which is a test
  isolation device — it would share nothing with the first but the credentials file. The picker
  must not depend on running several daemons.
- **Many sessions per daemon already works.** The registry is a `HashMap` of sessions; any
  number can be live, each with its own turn lock, and one client may attach to several. This is
  the capability the picker needs and it exists.
- **There is no way to enumerate them.** No `session.list` method exists, so a client can attach
  only to an id it already holds (`--session ID`).
- **Sessions do not outlive the daemon.** The registry and every history live in memory. The
  daemon exits after `ELEFANTE_IDLE_SECS` (15 min) with no clients and no turns, taking every
  session with it; the TUI's exit is a detach and never sends `session.close`, so an abandoned
  session lingers only until then. "Every conversation that has happened in this repo" is
  therefore not answerable today, only "every session the running daemon still holds".

**This is milestone M6 — Conversation history** (below). It is sequenced after M5 because it
reuses what M2 and M3 build: the repo identity and self-ignoring `.elefante/` directory, and the
`Scrubber` on a durable write path. Nothing in M0–M5 should have to change to make room for it;
the architecture notes that follow record why, so that stays true.

#### Architecture it builds on

The daemon already separates the three things a durable history needs to keep apart:

| Concern | Today | Where | Survives daemon exit |
|---|---|---|---|
| What the **model** sees | `core::Session.messages` — provider-neutral `Message`s, `reasoning_details` verbatim | private field of `core::Session` | no |
| What the **user** sees | `View.turns` — one record per completed turn: prompt, answer, tool previews, `stop_reason`, `usage` | `srv::View` | no |
| What a client **missed** | `View.ring` — 4,096 notification envelopes | `srv::View` | no, and not meant to |

The ring is a delivery buffer and should stay one; it is not the history. The history is the first
two, and they cannot be derived from each other: `View.turns` carries truncated previews and usage
that the messages do not, and the messages carry replay blocks that `View.turns` deliberately
omits. **A durable record therefore stores both, written together at `turn.finished`.** A turn
boundary is the natural commit point — an interrupt still returns a result for every `tool_use`
(DEVELOPER_GUIDE §5) — so a crash mid-turn loses at most the turn in flight rather than leaving
half of one on disk.

Constraints that follow from the existing design, to be honoured when the design pass happens:

- **Live session and stored conversation are different objects.** A `session_id` today is
  `s{instance_id}-{n}`, where `instance_id` is random per daemon run — unique, but a handle to a
  process's memory, not a name for a conversation. A stored conversation needs its own stable
  `conversation_id`, and *resume* creates a new live session loaded from it. That leaves
  `session.attach`, `seq`, the ring, and the snapshot contract (DEVELOPER_GUIDE §5) exactly as
  they are: a resumed session starts at `seq: 0` with its completed turns delivered by the first
  attach, which is already a snapshot.
- **`close` must stop meaning "gone".** `session.close` currently drops the session and its
  history. With durable history it ends the *live* session only; deleting a conversation is a
  separate, explicit method.
- **The daemon stays the sole writer.** Clients never touch the files, which keeps one process
  ordering writes. Two daemons on different sockets are possible only as a test device, but both
  could resume the same conversation, so resume takes a per-conversation lock and a second
  resumer gets a named error rather than interleaved appends.
- **Group by repo root, sandbox by `cwd`.** `View.cwd` is the canonicalised `session.create`
  path and is also the tool policy root, so a session started in `repo/subdir` is deliberately
  confined to `subdir`. Listing "this repo's conversations" needs a second field, `repo_root`,
  resolved once at creation, used only for grouping and never for policy.
- **List headers, not transcripts.** A listing row is `{ conversation_id, repo_root, cwd, model,
  created_at, updated_at, turn_count, first_prompt_preview, state }` where `state` is
  `live_active | live_idle | stored`. It lives apart from the transcript so listing a hundred
  conversations reads a hundred small records, never a hundred histories. Live sessions and
  stored ones share the row shape so a client renders one list.
- **Resume needs a constructor core does not have.** `Session::new(config)` starts empty and
  `messages` is private. M6 adds `Session::from_history`, and resume may change model or effort,
  since both live in `RunConfig` rather than the history. Resumed prefixes will usually miss the
  prompt cache once its TTL has lapsed; that is a cost, not a defect.
- **Transcripts are secrets-adjacent.** User prompts are not scrubbed today and can contain a
  pasted key. Every durable write goes through `Scrubber`, the same call site M3 uses for memory
  rows. The path policy also denies only `.elefante/credentials.toml`; once transcripts live in
  `<repo>/.elefante/`, whether the agent may read its own history is a decision, not a default.
- **Snapshot growth gets decided here.** Developer guide §14 open question 1 — snapshots grow
  without bound per session — becomes real once conversations live for weeks. Paging completed
  turns, or capping the snapshot to the last N with the rest fetched on demand, is part of M6.

Open for the design pass, deliberately not decided now: the on-disk form (an append-only JSONL
file per conversation plus a small header record is the leading option — crash-tolerant,
greppable, no actor thread — against a `sessions.db` beside `memory.db`); and whether transcripts
live under the repo or under `~/.elefante/`. The repo is preferred, because the picker is
per-repo and the directory already ignores itself.

---

## The RPC protocol (`elefante-rpc`)

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

**`Escalate` is inert until M1.** It and `Deny` both return a non-executing error today, so the third verdict does not yet mean anything distinct. M1 is when it is cashed — if approval slips again, collapse the enum rather than carry a state that pretends to mean something. The two are different *modalities*, not severities: `Deny` is terminal and no critic or flag clears it, while `Escalate` is a question whose answer may be yes. That is what stops an approve button appearing on `rm -rf /`.

**Approval flow (Escalate):** `core` emits `approval.requested` with the command, the rule that fired, and — in auto mode — a `guard::Critic` verdict from the triage model. The turn suspends on a `oneshot` channel. `session.resolve_approval` or the critic's verdict resolves it. An interrupt cancels the whole `JoinSet`, and already-completed tool results are still returned so history stays consistent.

**7. Loop** back to step 4 until `stop_reason != tool_use`.

**8. Post-turn observation** (`core` → `mem`, `core` → staleness) — records files read and edited, feeds `co_edited_with` candidates, updates staleness counters, and accumulates usage into `stats`.

**9. Review gate** — when the model signals completion after edits, `core` calls `guard::diff_critic` against the working-tree diff. Findings are injected as a mid-conversation system message (or a user turn, per capability) and the loop resumes, bounded to N rounds so it cannot spiral. Findings the user accepts or rejects become memory nodes — the system learns which critiques matter in this repo.

---

## Memory subsystem

### Storage

```
~/.elefante/
  config.toml                # roles, provider keys, thresholds
  memory.key                 # 32 random bytes, 0600: authenticates every memory store this user owns
  user.db                    # cross-project: preferences, corrections, workflows (not yet built)

<repo>/.elefante/
  memory.db                  # the graph: node, edge, import_ref, node_fts, meta
  cache.db                   # derived: file_state, content-addressed extractions, briefs
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

```rust
trait Critic { fn review(&self, artifact: Artifact, ctx: &Context) -> Verdict; }
enum Verdict { Pass, Findings(Vec<Finding>) }
struct Finding { severity, location, claim, evidence }
```

One abstraction, three call sites: bash escalation, plan review, diff review. Findings come back as structured output under `strict: true` and must cite file evidence. `guard` takes a `Provider` handle by injection, never constructing one — which is what makes the critics testable against a stub.

---

## Agent loop details

Seven tools: `read`, `write`, `edit`, `glob`, `grep`, `bash`, `memory_search`. Every tool is permanent prompt cost, so the bar for an eighth is high.

- Edits are exact-string-replace. Highest reliability, lowest tokens, no whole-file rewrites.
- Per-turn `effort` routing: `xhigh` for planning turns, `low` for mechanical edits.
- The system prompt stays under ~400 tokens because rules live in code: the bash classifier *is* "don't run destructive commands"; the path policy *is* "never read .env"; the diff critic *is* "review before done".
- Cache stability: frozen system text first (with `cache_control`), then a deterministic tool list — sorted, stable JSON key order — then messages. An unsorted tool list silently invalidates the cache every turn, which is exactly the class of bug the M0 gate checks for.

### Staleness detection

Lives in `core`, fed by turn observations, emitted as `session.stale`:

| Signal | Why it matters |
|---|---|
| `input_tokens / context_window` | the blunt one |
| `cached_input_tokens / input_tokens` | a collapsing hit rate means a silent invalidator |
| turns since compaction | |
| **repeated reads of a file already in context** | the earliest real signal — the model has lost track of what it knows |
| tool error rate, edit retry rate | degradation shows here before it shows in prose |

### Rejected: response sentinel

A `"working"` first-token sentinel (stripped from display; absence signals staleness) was considered and rejected for core:

- **Lagging.** First-token compliance is the easiest possible instruction to follow and among the last to fail — degradation in edit discipline and review adherence precedes it by many turns.
- **Mostly unmeasurable here.** Most agent-loop turns are pure `tool_use` with no text block, so there is no first word to check; treating that as a miss produces constant false positives.
- **Costs what it is meant to save** — prompt tokens, output tokens every turn, and a parse rule in the stream handler.
- **Larger context windows make it worse, not better** — stronger models hold the trivial instruction longer while still degrading on hard ones.

Replaced by the deterministic signals above, which are already on the wire. Instruction adherence is measured for free via critic structured-output schema-violation rate. Kept as an opt-in `--sentinel` diagnostic, evaluated only on turns containing a text block.

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
Deferred from the original baseline: the user-level graph (`user.db`), and transcript compaction —
M3 ships the seam (`Learner::flush`, and a watermark that only shrinks after a commit), so whatever a
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

**Shipped** (record in [`MILESTONES.md`](./MILESTONES.md)). Follow-up to M3, designed in [`MEMORY-UI-DESIGN.md`](./MEMORY-UI-DESIGN.md) (§ 7, § 8). It fixes two
defects found in the merged code and changes one default; the rest of that document is assigned to M5,
M6 and M7 below.

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

### M4 — Critics

**Baseline.** `Critic` trait; bash escalation; plan and diff critics with structured findings; bounded revision loop; findings → memory. A critic runs on a different vendor's model by changing a slug — no second integration — so this milestone is single-concern.

**Gate.**
- Seed a diff with a known defect: the diff critic returns a `Finding` citing the correct file, and the revision loop terminates at the round cap rather than spiraling.
- The *same* critic request returns schema-valid findings on a model that declares `structured_output` and on one that does not — proving lenient parsing actually degrades rather than erroring.
- Findings the user accepts or rejects reach the graph as nodes.
- **M0–M3 gates still green.**

### M5 — Token analysis

**Baseline.** Per-turn accounting from `usage`, cache-hit reporting, standing prompt overhead, staleness signals, cost attribution by role. Cost comes from `usage.cost` on each response rather than a local price table — the provider reports what the generation actually cost, including cache discounts, so there is no table to keep current.

**Memory is the first background role to be accounted for.** *The memory parts of this milestone have
shipped — `MemoryStatus`, `memory.status`, lenient clients, `View::emit_session`, the consecutive-failure
count, `Store::reset_reason()`, the single cost total and the status suffix. `session.stale` and the rest
of token analysis (the baseline above and every gate item that is not about memory or unknown events) are
unbuilt, so M5 stays open.* The learned-memory extractor spends money
the turn totals never see, so this milestone adds the full `MemoryStatus` from
[`MEMORY-UI-DESIGN.md`](./MEMORY-UI-DESIGN.md) § 4.1 (grown in place from M3.1's `memory` field):
consent, learning working/paused/failing, cumulative memory spend, and a fixed-vocabulary problem
reason, carried in create/attach results and snapshots and refreshed by a `memory.status` event. The
displayed cost becomes one total including memory. It is also the first milestone with a new
notification event (`session.stale`, below), so **lenient clients** ship here: both parsers accept an
unknown `method`, the Rust one through a hand-written `Deserialize` with `Unknown`, and new events share
the existing `seq` space. Needs `View::emit_session` (events that belong to no turn), the learner's
consecutive-failure count, and `Store::reset_reason()`.

**Gate.**
- Reported session cost equals the sum of per-response `usage.cost` for a scripted session against recorded fixtures.
- Cache reporting distinguishes `cached_input_tokens` (reads) from `cache_write_tokens` (writes); a session with zero reads across repeated identical prefixes is surfaced as a warning, not silently.
- `reasoning_tokens` are attributed separately from `output_tokens`, so effort changes are visible in the accounting.
- Standing system-prompt overhead is reported and is **under the ~400-token target** — the number that makes conviction #1 enforced rather than aspirational.
- Staleness signals fire on a synthetic session built to trigger each one.
- Both clients ignore an unknown event, including one with params, and `seq` has no holes for any client.
- `MemoryStatus` is correct for every switch and degradation, from a fresh attach and from a snapshot;
  displayed cost equals turn usage plus cumulative memory spend and survives a reattach.
- **M0–M4 gates still green.**

### M6 — Conversation history

**Baseline.** Design pass first: on-disk form, `conversation_id`, resume semantics, and the
secrets and path-policy questions in § Session picker. Then `session.list`, `session.resume`, and
`session.delete`; durable per-conversation records written at each `turn.finished` under
`<repo_root>/.elefante/`, scrubbed on write; `repo_root` resolution; `Session::from_history`; a
per-conversation lock; bounded snapshots; and a TUI picker shown on launch (new session first)
with `--session ID` and `--new` to bypass it.

**Audit trail.** The stored record doubles as the means to audit agent behaviour, so it keeps
what the live view discards. Today `View.turns` omits reasoning and truncates tool input and
output to 2 KiB previews; the durable record instead holds, per turn: the prompt, the visible
reasoning text, the answer, every tool call with its **full** input, output, duration, and
approval decision, `stop_reason`, and `usage` — and the ids of any learned notes injected into the turn
(`InjectedContext` from [`MEMORY-UI-DESIGN.md`](./MEMORY-UI-DESIGN.md) § 5), since what shaped a turn
is part of auditing it. *`InjectedContext`, the `memory.context` event and `memory_note_ids` on snapshots
have shipped (the in-daemon half of this audit); the durable record, and everything else in this
milestone, has not.* Encrypted reasoning blocks are replay state, not
readable text, and are stored only as the history needs them for resume — an audit cannot read
what the provider does not disclose. Everything passes `Scrubber` on write, as above.

**Observability.** The daemon has two choke points that see everything: the connection reader
sees every client → daemon request, and `View::emit` sees every daemon → client event, tool
calls included. Both feed one structured-log stream, shipped to **Datadog** through a sink, so
the backend can change without touching the tap. Constraints, since this sends a user's work to
a third party:
- **Off by default**, enabled by explicit configuration, with the API key supplied the way the
  OpenRouter key is — never from the working tree.
- **Scrubbed before it leaves the process**, by the same `Scrubber`; tool inputs and outputs can
  hold file contents, so log bodies are redacted or truncated by a stated rule rather than sent
  whole.
- Each line carries `session_id`, `conversation_id`, `turn_id`, and `seq`, so a wire event, a
  tool call, and a stored turn can be joined.
- A slow or unreachable Datadog never blocks or fails a turn: the sink is bounded and drops
  rather than back-pressuring the daemon.
- The log is not the audit record. Datadog is for operating the system; the on-disk record is
  the source of truth for what the agent did.

**Gate.**
- `SIGKILL` the daemon between turns and restart it: the conversation is listed with every
  completed turn, and resuming it then prompting does not 400 — against the pairing-validating
  stub, so the assertion cannot pass vacuously.
- A conversation interrupted mid-turn resumes from its last completed turn, with no half-written
  turn in the list or the history.
- `reasoning_details` in the first request after resume are byte-identical to those recorded
  before the restart.
- Listing from repo A never returns repo B's conversations; listing from a subdirectory of A
  returns A's, while the session started there keeps its narrower tool root.
- Listing reads headers only, asserted by a counter on transcript opens rather than by timing.
- A prompt containing an `sk-ant-` fixture string leaves it absent from every file under
  `.elefante/` after the session.
- `git status` is clean after a full session.
- The stored record of a scripted turn contains its reasoning text, answer, and every tool call
  with its complete input and output — a tool result longer than the 2 KiB preview limit is
  stored in full, not truncated.
- With the Datadog sink pointed at a local fake endpoint: a session produces one log line per
  request, event, and tool call, each carrying the join ids; none contains the `sk-ant-` fixture;
  and with the endpoint down or slow the turn still completes and the sink drops rather than
  blocks.
- With no sink configured, nothing is sent anywhere.
- The stored record of a turn that received learned notes names them by id; a turn that received none
  records none.
- A second resume of a live conversation is refused with a named error; `session.close` ends the
  live session and leaves the stored conversation listed; `session.delete` removes it.
- The TUI picker shows live and stored conversations in one list, bypasses on `--session` and
  `--new`, and a headless one-shot is unchanged.
- **M0–M5 gates still green.**

### M7 — Memory in the UI

**Shipped** (record in [`MILESTONES.md`](./MILESTONES.md)), together with M3.1 and the memory parts of M5
and M6. Two departures from the baseline below: the `/memory` panel is a standalone modal, not built on
M6's list overlay (which does not exist), and the panel lists and forgets only — approve, history and
clear stay in `elefante memory`. A tombstone for forgotten notes, per-repository consent and a "recent"
summary on attach remain open (`MEMORY-UI-DESIGN.md` § 13).

**Baseline.** The user-facing half of learned memory, specified in
[`MEMORY-UI-DESIGN.md`](./MEMORY-UI-DESIGN.md) (§ 4.2, 4.3, 6). Depends on M5 (state, events, lenient
clients) and M6 (`repo_root`, the list overlay, injected-note ids).
- `LearnSink` and the `memory.learned` and `memory.context` events; a learned note becomes a persistent
  transcript line (`◇ remembered: … /forget 14`), and a turn that used notes shows
  `◇ used N saved notes`. Nothing is added to the screen while memory is healthy.
- `memory.list | forget | approve | clear`, keyed by `cwd` so they work with no session open; the daemon
  is the preferred writer and `elefante memory` uses it when it is running, falling back to the
  database directly.
- A `/memory` panel (list, forget, close; provenance in words, built on M6's list overlay) and
  `/forget <id>`; a slash command matches only exactly `/memory` or `/forget <integer>`.
- The headless client's stderr lines.

**Gate.**
- `memory.learned` and `memory.context` carry the right `turn_id` and ordering; a reinforced note emits
  nothing; bursts are bounded.
- `forget`, `approve` and `clear` through the daemon take effect on the next retrieval and in
  `MemoryStatus`; the CLI works with and without a daemon; `clear` leaves the code graph alone.
- The panel never lets a key reach the prompt or interrupt a turn; `/usr/bin` and a pasted block are
  prompts; control characters in a note never reach a renderable.
- With learning working, the status line and screen layout are unchanged.
- **M0–M6 gates still green.**

---

## Deferred

Noted now so the architecture accommodates them, not built yet:

- **tmux subagent execution and nested sessions** — the daemon is already the shared backend; subagents become additional registry sessions, each rendered in its own pane.
- **Multi-model routing policy** — automatic selection by specialization / intelligence / cost, building on the interactive model picker (see § Model selection), which is where a user's reachable catalogue and its real prices first become available to the harness. One provider reaching hundreds of models means the reach exists from M0; the deferred part is the policy that picks per task rather than per configured role. `usage.cost` on every response makes the cost input real rather than estimated.
- **Self-authored skills** (user- and project-level) and self-improving workflow — builds on the M3 learned-memory write path.
- **Collapsible reasoning in the TUI.** A key that folds the reasoning block of each turn to its
  heading, so long chains of thought do not push the answer off-screen. An earlier version bound
  this to Ctrl+R with an `App`-level flag; it was removed rather than shipped half-designed.
  Decide first whether collapse is global or per turn, whether it survives reattach (snapshots do
  not carry completed reasoning), and whether it should default to collapsed once a turn ends.
- **Hooks and plugins.**
- **Memory follow-ups after M7**, each needing a decision [`MEMORY-UI-DESIGN.md`](./MEMORY-UI-DESIGN.md)
  § 13 leaves open: a tombstone so a forgotten note is not re-learned (key on the statement's hash or
  its words); consent per repository for repositories marked sensitive; a bounded "learned while you
  were away" summary on attach.
- **Resolution beyond tier 1** — workspace-crate imports (`use elefante_core::…`), `tsconfig` paths and bare package imports; deferred from M2 because each is a second parser for a signal tier-1 edges and co-change already approximate. Also a call/reference graph and `.js`/`.jsx` extraction.
- **Python and C/C++ extraction** — planned, after M3. A language is a grammar crate, an extractor and a
  resolver apiece (plus one registry line in `extract::language_for` and one arm in `resolve::resolve`),
  so these add no schema, no relation and no RPC change. Proposed ids: `python@1`, `c@1`, `cpp@1`; the
  file-state, extraction cache and graph are already language-agnostic. Open points to settle in that
  milestone's design pass, not decided here:
  - **Gates that carry over unchanged:** signatures end before a body or initializer and elide string
    literals (Python: default arguments, `single_quotes = true`; C/C++: never index a `#define` body or
    an initializer); one-sentence docs (Python docstrings, `///` and `/** */`); the `Scrubber` on every
    string; the parse budget; the secret-fixture gate re-run for each language.
  - **Python:** relative imports (`from . import x`, `from ..pkg import y`) and `__init__.py` packages
    resolve against the path set like Rust `mod`; absolute imports need a source-root guess (repo root,
    `src/`) — likely tier 2 with `pyproject.toml`. Index `def`, `class`, methods and top-level
    functions only; module-level assignments are skipped for the same reason TS skips non-function
    `const`.
  - **C/C++:** `#include "x.h"` resolves relative to the including file, then the repo path set;
    `<system>` includes are ignored; include directories need `compile_commands.json` or build files,
    so tier 2. `.h` is ambiguous between C and C++ (decide: parse as C++, which is a superset for
    declarations). Header/source pairing (`foo.h` ↔ `foo.cc`) is a strong coupling signal worth a
    dedicated edge or a resolver rule. Templates, namespaces and macros stress `qname` nesting.
  - **Cost:** every grammar crate must clear the MSRV job (`tree-sitter-rust` and
    `tree-sitter-typescript` did via the S0 spike, which should be repeated for these), and each adds
    binary size and a per-file parse cost against the 1 s budget.
  - Per-language golden-retrieval fixtures, as for Rust and TS.
- **Server-side compaction** (`compact-2026-01-12`) and **context editing** (`clear_tool_uses_20250919`) as alternatives to client-side compaction.
- **Task budgets** (`task-budgets-2026-03-13`) for agentic-loop spend ceilings, feeding M5.
- **Embedding-based retrieval** — the vector slot is left pluggable; FTS + graph hops ship first.
