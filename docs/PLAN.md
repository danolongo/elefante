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
| UI | Rust daemon + Bun/OpenTUI client over JSON-RPC on a unix socket | Real OpenTUI (widgets, flexbox, mouse) has no Rust equivalent — `opentui_rust` is a 35-star rendering-only port with no widgets, layout, or event loop. The split is opencode's own architecture, and a headless core is what makes tmux subagents work later. |
| Retrieval | SQLite FTS5 + graph hop expansion | No embedding model, no API cost, deterministic and debuggable. Graph edges do the semantic work. Pluggable vector slot left open. |
| Storage | User graph in `~/.elefante`, project graph in `<repo>/.elefante/`, all gitignored | Project memory sits next to what it describes; nothing is ever committed, so no session noise reaches a diff. |
| Cache | In-process `moka` LRU inside the daemon + SQLite WAL | Redis considered and rejected: subagents all attach to the same daemon, so they already share its cache. Redis would add a required daemon and an IPC hop to beat microsecond page-cache reads. Kept behind a trait for possible cross-machine use later. |
| Providers | Anthropic and OpenRouter, as peers | Neither is privileged, neither is a fallback for the other. The trait reports `Capabilities` and the core degrades features rather than erroring. |
| Staleness detection | Deterministic metrics, **not** a `"working"` sentinel token | See "Rejected: response sentinel". |

---

## Crate topology and dependency direction

```
elefante/
  Cargo.toml                 # workspace
  docs/PLAN.md               # this document
  docs/MILESTONES.md         # per-milestone completion record
  crates/
    elefante-rpc/            # wire types shared by daemon and client (no deps on others)
    elefante-provider/       # Provider trait + Capabilities; anthropic, openrouter
    elefante-guard/          # PathPolicy, BashClassifier, Scrubber, Critic
    elefante-tools/          # read, write, edit, glob, grep, bash, memory_search
    elefante-mem/            # graph store, indexer, retrieval, cache
    elefante-core/           # session, turn loop, context assembly, staleness, compaction
    elefante-srv/            # daemon binary: socket, session registry
    elefante-cli/            # `elefante` binary: spawns daemon, launches client
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

## Providers

Two peers. Neither is the "real" one. The user selects per role. Feature surfaces differ, so the trait reports capabilities and the core adapts:

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

Capability differences are **descriptive, not a ranking**. The core degrades on every axis: no cache breakpoints when `prompt_caching` is false; memory injected as a normal user-turn block when `mid_conversation_system` is false; findings parsed leniently when `structured_output` is false. A provider is never sent a field it does not declare.

| | Anthropic | OpenRouter |
|---|---|---|
| Transport | `POST /v1/messages`, SSE | OpenAI-compatible `/chat/completions`, SSE |
| Model reach | Claude family | 100s across vendors |
| Credentials | `x-api-key` | single key |
| Thinking / effort | full | varies by upstream model |
| Caching + metrics | full | partial, model-dependent |

### Anthropic

Rust has no official Anthropic SDK, so this is raw HTTP (`reqwest` + SSE) against `POST /v1/messages`, headers `x-api-key` + `anthropic-version: 2023-06-01`. API facts that constrain the design:

- **Thinking budgets are removed.** `{type: "enabled", budget_tokens: N}` returns **400** on Opus 5 / Sonnet 5 / Opus 4.8 / 4.7. Use `thinking: {type: "adaptive"}` + `output_config: {effort: "low"|"medium"|"high"|"xhigh"|"max"}`. Effort is the per-turn depth knob.
- **`thinking.display` defaults to `"omitted"` on Opus 5.** Send `display: "summarized"` or reasoning never streams and the TUI shows a long silent pause.
- **Mid-conversation system messages** — append `{"role": "system", ...}` to `messages[]` (Opus 5 / Opus 4.8; **not** Sonnet 5). The correct channel for injecting memory and critic findings **without invalidating the cached prefix**, and prompt-injection-safe. Must follow a `user` message and be last or followed by an `assistant` turn.
- **Prompt caching** renders `tools` → `system` → `messages`; any byte change invalidates everything after. Max 4 breakpoints. Verify with `usage.cache_read_input_tokens`.
- **Structured outputs** via `output_config: {format: {...}}`; `strict: true` on tools requires `additionalProperties: false` + `required`.
- **No assistant prefill** on Opus 5 (400). **`stop_reason: "refusal"`** must be checked before reading `content`.
- Streaming required for large `max_tokens`. Parse tool inputs with a real JSON parser — escaping varies.

### OpenRouter

OpenAI-compatible `/chat/completions`, SSE, single key. Broadest reach and the simpler impl. Caveats are honest rather than disqualifying: no Anthropic-native cache breakpoints or hit metrics, and thinking/effort varies by upstream model — so `Capabilities` is resolved **per model** here, not per provider.

Tool-call semantics differ (`tool_calls` with stringified JSON arguments vs. `tool_use` blocks with parsed input). The provider crate normalizes both onto one internal message/tool type so neither wire format leaks into `elefante-core`.

### Role configuration

Roles are provider-agnostic and configured, not hardcoded — **author**, **critic**, **bash triage** each name a `(provider, model, effort)` triple in `~/.elefante/config.toml`, overridable per project. Starting config: author = Opus 5 @ `xhigh`; critic = a *different vendor's* model (this is what makes review adversarial rather than self-agreeing); triage = anything cheap and fast. Anthropic reference pricing per 1M in/out: `claude-opus-5` $5/$25 (1M ctx) · `claude-sonnet-5` $2/$10 (1M) · `claude-haiku-4-5` $1/$5 (200K).

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

---

## The RPC protocol (`elefante-rpc`)

Newline-delimited JSON, JSON-RPC 2.0 shaped, bidirectional. Requests carry ids; streaming uses server→client notifications because the TUI needs sub-frame latency and polling would jitter.

**Client → server (requests):**

| Method | Params | Returns |
|---|---|---|
| `session.create` | `{ cwd, roles? }` | `{ session_id }` |
| `session.prompt` | `{ session_id, parts }` | `{ turn_id }` (streams follow) |
| `session.interrupt` | `{ session_id }` | `{}` |
| `session.resolve_approval` | `{ approval_id, decision }` | `{}` |
| `session.list` / `session.attach` | `{}` / `{ session_id }` | registry view / replay of state |
| `memory.search` | `{ session_id, query }` | `{ nodes }` — for UI inspection |
| `stats.session` | `{ session_id }` | token/cost accounting |

**Server → client (notifications):**

`turn.started` · `thinking.delta` · `text.delta` · `tool.started` · `tool.finished` · `approval.requested` · `critic.findings` · `turn.finished { stop_reason, usage }` · `session.stale { signals }`

`session.attach` returning a **replay of current state** (not just an ack) is what allows a second client — or a reconnecting one after a crash — to render a session already in flight. Building that in from the start is much cheaper than adding it once the TUI assumes it owns the session.

---

## The turn lifecycle

This is the central interaction, and the place where every crate meets. One numbered trace, with the owner of each step:

**1. Prompt received** — `srv` routes `session.prompt` to `core::Session`, which takes a turn lock. A second prompt on the same session while a turn is live is rejected, not queued: silent queueing makes an interrupted session behave unpredictably.

**2. Context assembly** (`core` → `mem`)

```
core::assemble_context(user_msg, open_files, budget) →
    mem::retrieve(Query { text, files, budget_tokens: 1500 })
        → moka lookup on (query_hash, repo_generation)
        → miss: FTS5 seed → graph expand (1–2 hops, weighted) → pack to budget
    → MemoryBlock { nodes, tokens, partial: bool }
```

`partial: true` when the indexer hasn't finished its first pass — retrieval never blocks on indexing.

**3. Placement decision** (`core`) — capability-dependent, and the reason `Capabilities` exists:

- `mid_conversation_system: true` → memory appended as `{"role": "system"}` in `messages[]`. **The cached prefix survives.**
- `false` → memory prepended to the user turn's content. Correct, just less cache-efficient.

**4. Request construction** (`core` → `provider`) — `core` builds a provider-agnostic `Request { system, tools, messages, role }`. The provider maps it to wire format, applying only capabilities it declares, and places cache breakpoints itself (Anthropic: end of system, end of tool list). `core` never writes provider-specific fields.

**5. Stream** — provider yields a normalized `Event`:

```rust
enum Event {
    ThinkingDelta(String), TextDelta(String),
    ToolUseStart { id, name }, ToolUseInputDelta(String), ToolUseComplete { id, input: Value },
    Usage(Usage), Stop(StopReason),
}
```

`core` forwards each as an RPC notification *and* accumulates the assistant message. Two consumers, one pass.

**6. Tool dispatch** on `stop_reason == tool_use` — the most interaction-dense step:

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

- **All results must return in a single user message.** Splitting them across messages trains the model out of parallel tool calls.
- **A failed tool still returns a result** with `is_error: true`. Dropping it desynchronizes `tool_use_id` pairing and the next request 400s.
- **`guard::scrub` runs on every result**, including denials and errors — it is the layer that catches what the path policy cannot.

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
  user.db                    # cross-project: preferences, corrections, workflows

<repo>/.elefante/
  memory.db                  # project graph
  cache.db                   # derived: file hashes, symbols, briefs
  .gitignore                 # generated, contains `*` — self-ignoring
```

The generated `.gitignore` containing a single `*` means the directory ignores itself and the host repo's own ignore rules are never touched. Repo identity is the project path, so no identity keying is needed.

### Concurrency: the memory actor

`rusqlite::Connection` is not `Sync` and SQLite calls block. Rather than scattering `spawn_blocking` across the codebase, `mem` runs **one actor thread owning both connections**, receiving `MemRequest` over an `mpsc` channel and replying via `oneshot`. This serializes writes naturally, keeps `Connection` off the async runtime, and gives one place to instrument query timing. The `moka` cache sits in front of the actor, so hits never cross the channel.

### Schema

Deliberately small. **Full schema and relation semantics get a dedicated design pass before M2 ships** — this is the starting point, not the final word:

```sql
node(id, kind, key, body, confidence, source, created_at, updated_at)
edge(src, dst, rel, weight, created_at)
node_fts(key, body)                            -- FTS5 virtual table
meta(repo_generation, last_indexed_at, ...)
```

Kinds: `fact`, `preference`, `decision`, `convention`, `file`, `symbol`. Relations: `about`, `depends_on`, `co_edited_with`, `contradicts`, `supersedes`, `derived_from`.

### Two ingestion layers

The split is what keeps this cheap:

**Mechanical** — no LLM, fully cacheable, runs in the indexer:

```
ignore::WalkBuilder (respects .gitignore)
  → blake3 per file → diff against cache.db → changed set only
  → tree-sitter parse → symbol nodes, import edges
  → git log --name-only -n 500 → files co-occurring in a commit
                                → co_edited_with edge weight++
  → single transaction → bump repo_generation (invalidates moka)
```

Git co-change is the highest-value-per-cost signal here: it captures "these files change together" — real coupling that imports alone miss — for the price of parsing one `git log`.

**Learned** — LLM-extracted decisions, conventions, corrections, written at session end and on explicit signal.

### Indexer scheduling

`session.create` spawns `mem::Indexer::refresh` as a background task. It **never blocks the first prompt**. Retrieval against a partially-built index is valid and flagged. The repo brief (~300 tokens, injected each session) is keyed by the merkle root of tracked file hashes, so it regenerates only on material change.

### Anti-bloat

This decides whether the whole subsystem works. LLM memory fails by accumulating contradictory sludge:

- Dedupe against FTS before insert.
- `supersedes` edges rather than deletion — history stays inspectable.
- Confidence decays with age unless reinforced.
- Periodic compaction pass collapses superseded chains.

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
| `cache_read_input_tokens / input_tokens` | a collapsing hit rate means a silent invalidator |
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

**Baseline.** Workspace skeleton. `Provider` trait + `Capabilities` + Anthropic impl (SSE, adaptive thinking, effort, caching, refusal handling). Agent loop with the seven tools, plain stdout, no TUI. **`PathPolicy` and the deterministic half of the bash classifier ship here** — an ungated bash tool should never exist, even in a prototype.

**Gate.**
- Bash classifier fixture table: commands → expected `Allow`/`Deny`/`Escalate`, including compound commands (`ls && rm -rf /` must classify as the `rm`), pipes, subshells, and command substitution. Includes obfuscation cases asserted as **known-not-caught**, so the test documents the limit rather than implying coverage.
- `PathPolicy` canonicalization: `./x/../.env`, a symlink pointing at `.env`, `.env.local`, and a grep whose *pattern* would match secret content — all denied.
- Secret fixture: an `.env` containing an `sk-ant-`-shaped string. Read, glob, and grep all deny; `cat .env` is gated; the scrubber redacts it from a bash result that reaches it another way.
- Provider: `usage.cache_read_input_tokens > 0` on the second request of a session (zero means a silent invalidator). No request ever constructs `budget_tokens`. `display: "summarized"` yields non-empty thinking deltas.
- End-to-end: run against this repo, request a trivial edit, confirm the tool loop completes and the edit lands.

### M1 — Daemon + TUI

**Baseline.** `elefante-rpc` protocol; daemon over unix socket with session registry; Bun/OpenTUI client rendering streamed text, summarized thinking, tool calls, and approval prompts.

**Gate.**
- Kill and reconnect a client mid-turn: `session.attach` replays state and the turn completes with output intact.
- Two clients attached to one session both render the same stream.
- Interrupt cancels in-flight tools, and already-completed tool results still appear in history (no `tool_use_id` desync — assert the next request does not 400).
- Socket is mode `0600`; daemon exits after idle timeout with zero clients.
- **M0 gate still green.**

### M2 — Repo cache + mechanical graph

**Baseline.** Schema design pass first. Walk + blake3 cache, tree-sitter symbols, import edges, git co-change edges, repo brief, FTS5 + graph-hop retrieval injected per capability.

**Gate.**
- Warm start: second launch on an unchanged repo performs **zero** tree-sitter invocations (asserted by counter, not by timing).
- Touching one file re-parses exactly one file and bumps `repo_generation` once.
- `co_edited_with` weights match a hand-computed expectation on a fixture repo with known commit history.
- A prompt issued before indexing completes returns a `partial: true` memory block and does not block.
- Retrieval respects the 1500-token budget under an adversarially large graph.
- **M0–M1 gates still green.**

### M3 — Learned memory

**Baseline.** User graph, session-end extraction, dedupe/supersede/decay/compaction. Scrubber on the write path. Compaction writes facts to the graph.

**Gate.**
- `git status` is clean after a full session in a test repo — the self-ignoring `.gitignore` means nothing is untracked and nothing is stageable.
- Insert a fact twice with different wording: one node, confidence reinforced, not two nodes.
- A contradicting fact creates a `supersedes` edge; retrieval returns the survivor.
- No memory row contains the `sk-ant-` fixture string after a session that touched it.
- Compact a long transcript, then assert a fact from the dropped region is still retrievable from the graph.
- **M0–M2 gates still green.**

### M4 — Critics + OpenRouter

**Baseline.** `Critic` trait; bash escalation; plan and diff critics with structured findings; bounded revision loop; findings → memory. OpenRouter impl lands here, since running a critic on a different vendor's model is its first real consumer.

**Gate.**
- Seed a diff with a known defect: the diff critic returns a `Finding` citing the correct file, and the revision loop terminates at the round cap rather than spiraling.
- The *same* critic request through both providers returns schema-valid findings — the real test that capability degradation works rather than that one provider is privileged.
- No provider receives a field it does not declare (nothing Anthropic-only reaches OpenRouter unless the resolved model declares support).
- Absent provider key produces a named, graceful error — either provider is optional.
- **M0–M3 gates still green.**

### M5 — Token analysis

**Baseline.** Per-turn accounting from `usage`, cache-hit reporting, standing prompt overhead, staleness signals, cost attribution by role.

**Gate.**
- Reported cost matches a hand-computed figure from recorded `usage` for a scripted session.
- Standing system-prompt overhead is reported and is **under the ~400-token target** — the number that makes conviction #1 enforced rather than aspirational.
- Staleness signals fire on a synthetic session built to trigger each one.
- **M0–M4 gates still green.**

---

## Deferred

Noted now so the architecture accommodates them, not built yet:

- **tmux subagent execution and nested sessions** — the daemon is already the shared backend; subagents become additional registry sessions, each rendered in its own pane.
- **Multi-model routing policy** — automatic selection by specialization / intelligence / cost. Both providers ship by M4, so the reach exists; the deferred part is the policy that picks per task rather than per configured role.
- **Self-authored skills** (user- and project-level) and self-improving workflow — builds on the M3 learned-memory write path.
- **Hooks and plugins.**
- **Full graph schema and relation semantics** — dedicated design pass before M2 ships.
- **Server-side compaction** (`compact-2026-01-12`) and **context editing** (`clear_tool_uses_20250919`) as alternatives to client-side compaction.
- **Task budgets** (`task-budgets-2026-03-13`) for agentic-loop spend ceilings, feeding M5.
- **Embedding-based retrieval** — the vector slot is left pluggable; FTS + graph hops ship first.
