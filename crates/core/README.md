# elefante-core

The provider-neutral agent loop. Core owns conversation semantics — building requests, consuming
normalized events, validating the tool-use lifecycle, dispatching tool batches, and deciding
when a turn ends — and, in `tools.rs`, the syscall boundary every tool goes through. It knows no
OpenRouter field names, and nothing outside `tools.rs` touches the filesystem or spawns a process.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 1038 | `Session`, `ToolGate`, `RoundAccumulator`, `ContextSource`, `InjectedContext`, `run_agent_loop`, and every public type. |
| `src/learn.rs` | 1017 | `Learner`, `LearnConfig`, and the `LearnSink` it reports to. |
| `src/memory.rs` | 138 | `MemContext` and `MemSearchTool` — the repository index as a source of context and as the backing for `memory_search`. |
| `src/tools.rs` | 599 | `ToolExecutor`, the seven tool implementations, `definitions()`, and the input structs each tool decodes. Formerly the `elefante-tools` crate. |

`lib.rs` is still one large file and still the obvious first split — `ToolGate` and
`RoundAccumulator` are both self-contained. `memory.rs` is the only part that knows `elefante-mem`
exists, so the loop itself would compile unchanged without it.

## Entry points

```rust
use elefante_core::{CancellationToken, RunConfig, Session, ToolGate};

let mut session = Session::new(RunConfig::new("anthropic/claude-sonnet-5"));
let gate = ToolGate::new(approvals);          // Arc<dyn ApprovalSink>

let outcome = session.prompt(
    &provider, &tools, &gate,
    "Summarize this repository.",
    CancellationToken::new(),
    |event| render(event),                    // FnMut(OutputEvent)
).await?;

session.messages();   // full history, retained across prompts
session.usage();      // accumulated across every turn in this session
```

**`Session` is the entry point; `run_agent_loop` is the lower-level one.** `Session` owns
history across turns — call `prompt` repeatedly and each turn sees everything before it.
`run_agent_loop` runs a single turn from a fresh history and exists for callers that manage
their own state.

`CancellationToken` is re-exported from `tokio-util` so callers do not need that dependency to
interrupt a turn.

### Per-turn context

```rust
let session = Session::new(config).with_context(Arc::new(MemContext::new(store, retriever, scope)));
```

A `ContextSource` supplies two things: a **block** relevant to this turn's prompt, and a standing
**preamble** (the repository brief) once there is one. Both are *repository-derived data*, so:

- They are built into each request, never stored in `Session::messages()`. History is
  byte-identical with and without a source, which is asserted.
- They are `user`-role messages, **never `system`**. A doc comment in a cloned repository is
  attacker-controllable; giving it system authority would be an escalation.
- The block goes **immediately after** the turn's user message. Across consecutive turns the
  request prefix is then byte-identical through that message and only the block and what follows
  differ — placing it before would shift the user message and everything after it. It is fetched
  once per turn and re-sent at the same index every round, so the cache holds across tool rounds.
- The preamble is adopted at most once and then frozen: the same bytes on every later request.
  A session that starts before the first index finishes simply adopts it on the first turn where
  one exists, costing one cache miss.
- A source that returns `None` adds nothing — no empty envelope.
- `turn_block` returns `Option<InjectedContext { text, notes }>`. When learned notes were injected,
  `Session::prompt` emits `OutputEvent::Context { notes }` **after** `TurnStarted` (before it, the
  daemon would have no active turn to attach it to). It carries metadata only — the rendered text is
  never emitted and never stored — so a client can say which notes shaped a turn.

### Learning (`learn.rs`)

```rust
let learner = Learner::new(store, provider, LearnConfig::from_env(&author_model, &|n| env::var(n).ok()));
learner.observe(session.messages());   // after a turn; returns at once
learner.flush().await;                 // on close, after a quiet spell, and as the daemon exits
```

A `Learner` is one session's loop for asking a model what is worth remembering and handing the
answer to `elefante-mem`, which decides what is kept. The model's reply is a *proposal*.

- **What it reads.** Only the `Text` blocks of user and assistant messages, scrubbed. A tool result
  lives inside a `User` message but is not something the user said; tool calls and reasoning are not
  conversation. Repository text therefore reaches the extractor only if one of them restated it.
- **When it asks.** A *gate* decides when, never whether: an explicit cue ("remember", "from now
  on", …) in unprocessed user text, or ~600 characters of it, or 6 user messages. Skipped turns stay
  pending and are read by the next run, so nothing is lost by waiting. `flush` asks regardless.
  One call at a time per session, at most 20 calls, and three consecutive failures give a batch up
  (`skipped`), so a poisoned slice is not retried for ever.
- **The watermark** is what is still pending: it shrinks only after a batch has been committed.
  Anything a future compactor drops from the history has therefore already been read, provided it
  calls `flush` first.
- **Offering the index.** `memory_search` is listed for a session exactly when its `ToolExecutor`
  has an index behind it (`has_memory`), so the model is never offered a tool that can only refuse.
  `Session::with_context` is how a session gets an index; it also appends `MEMORY_USE_HINT` to the
  system prompt. `Session::with_system_note` appends a further fixed sentence (the daemon adds
  `LEARNING_HINT` only where learning can run). The prompt stays one string, fixed for the session,
  so the cached prefix holds; `DEFAULT_SYSTEM_PROMPT` itself is true of every session.
- **The log.** `ELEFANTE_LEARN_LOG=<file>` makes the daemon append one JSON line per attempt: the
  outcome (`provider_error`, `unreadable_reply`, `no_notes`, `ingested`, `store_error`), the stop
  reason, whether the schema was sent, counts, the verifier's rejections (`QuoteNotFound` and so on),
  and the first 400 characters of the reply. That is the only place a silent "nothing was learned"
  is explained. The file holds fragments of the conversation, so it is created `0600`, only on
  request, and a path that cannot be written is ignored.
- **The role.** `ELEFANTE_EXTRACT_MODEL` and `ELEFANTE_EXTRACT_EFFORT` (default low); with no model
  set, the session's own. The request carries no tools and a response schema, which the provider
  sends only to models that declare structured output; the reply is validated either way. If a
  provider refuses a request *because of* the schema (HTTP 400, 404 or 422), the learner drops the
  schema for the rest of the session and asks again without it, which is not counted as a failure.
  `Learner::diagnostics()` says why the last call failed, what an unreadable reply began with, and what
  the store refused; a success clears it.
- **The sink.** `Learner::with_sink` takes an `Arc<dyn LearnSink>` (`learned`, `usage`, `health`; every
  method defaults to a no-op, so `Learner::new` is unchanged). `learned` gets only `Added`, `Promoted`,
  `Superseded` (with the note it replaced) and `Revived` notes, resolved with one read after the
  commit; a reinforced note is not reported. `usage` is the `Usage` of each extractor call that
  reported one, including a call whose reply turned out to be unreadable. `health` is `LearnHealth { consecutive_failures, failure }` with a *class*
  (`ProviderStatus(code)`, `NoReply`, `UnreadableReply`, `Store`), reset on success; it is what the
  daemon turns into the wire's fixed-vocabulary `problem`, never the provider's text. Callbacks run
  on the learner task and must return quickly.
- **Consent is not here.** The learner has no idea whether it may run; the daemon simply does not
  call `observe` until a person has agreed.

### Tools (`tools.rs`)

```rust
use elefante_core::tools::{ToolExecutor, definitions};

let tools = ToolExecutor::new("/path/to/repo")?;   // roots PathPolicy here
let result = tools.execute("read", json!({ "path": "src/main.rs" })).await;

let catalog = definitions();   // 7 schemas, alphabetical, for the provider request
```

`ToolExecutor` is cloneable and holds the policy and scrubber, so cloning it cannot produce an
executor rooted somewhere else. `definitions()` is sorted alphabetically because a stable catalog
order keeps the prompt cacheable. The seven tools are `bash`, `edit`, `glob`, `grep`,
`memory_search`, `read` and `write`; `DEVELOPER_GUIDE.md` §8 documents each one.

- **Scrubbing happens at the single exit point, not per tool.** `execute` matches on the tool name,
  then rebuilds the result with `self.scrubber.scrub(&result.content)` before returning. A new tool
  cannot forget to scrub; preserve that shape when adding one.
- **Every path argument goes through `self.resolve`, which delegates to `PathPolicy`.** `glob` and
  `grep` also filter their *results* through the policy, so a traversal or a symlink into a secret
  cannot leak through a listing even when the caller never named the path.
- **`bash` does not classify its own command.** Classification is `ToolGate`'s, which runs before
  dispatch and can suspend for approval; a gate inside the executor would have nowhere to wait. The
  executor spawns `bash -lc`, rooted at the policy root, `kill_on_drop`, in its own process group on
  Unix so an interrupt takes the children with it.
- **`memory_search` is a trait.** The tool is validated in `tools.rs` (a non-empty query, `limit`
  1–20, `kind` of `file`, `symbol` or `note`, no other fields) and handed to a `MemorySearch`
  attached with `ToolExecutor::with_memory`; `memory.rs` implements it over `elefante-mem`. With
  nothing attached the tool returns one fixed refusal, so a session with memory off still gets a
  deterministic answer.
- **One syscall boundary.** Do not add a read, write, process, glob or search path elsewhere that
  bypasses these policy calls: auditing the boundary means auditing one file.

## Invariants

**Every `tool_use` must keep exactly one matching `tool_result`.** An unpaired id makes the next
provider request 400, so pairing is validated *before* an assistant message is published to
persistent history (`src/lib.rs:289`) — publishing first and repairing later would leave the
session permanently unable to continue. This is the single invariant most likely to be broken by
a well-meaning change to interrupt or error handling.

**Tool dispatch is a `JoinSet`, not `join_all`.** The difference only shows on interrupt: a
`JoinSet` can abort pending tasks while completed ones keep their real results, which is exactly
what pairing requires. `join_all` gives no such handle, and the original M1 protocol draft
promised an interrupt guarantee that `join_all` could not keep.

**Every tool result in a batch is appended together, before the next assistant turn.** This is a
protocol invariant rather than a formatting preference — splitting a result batch across turns
trains the model out of parallel tool calls.

**`ToolGate` maps a `Verdict` onto a `GateDecision`, and the three cases stay distinct.**
`Deny` is terminal and never reaches the approval sink; `Escalate` calls `ApprovalSink::request`
and waits. `DenyApprovalSink` is the default, so a caller that wires nothing gets fail-closed
behavior rather than silent approval.

Refusals end the invocation without dispatching pending tools, and a partial tool block is
discarded before validation so a refusal mid-block cannot desynchronize ids.

## Testing

```sh
cargo test -p elefante-core     # 97 tests, no network
```

`tests/session.rs` (20) covers multi-turn history, cancellation, and pairing under interrupt.
`tests/loop.rs` (6) covers the single-turn path. `tests/tools.rs` (19) covers all seven tools
against `tempfile` workspaces — real files, real `bash`, no network. `tests/context.rs` (12) covers where context goes in
a request, that it never enters history, and that the audit event follows `turn.started` without the rendered text. `tests/learn.rs` covers the learner, including the sink. `tests/prompt.rs` (1) asserts the default system prompt. Nothing is `#[ignore]`d; every test drives a scripted in-process provider, so the suite
needs no credentials.

## Boundaries

No provider wire formats and no socket handling, and no filesystem access outside `tools.rs`. The
loop takes a `&dyn Provider` and a `&ToolExecutor` and calls them; if a change here needs to know how
a tool result is shaped on the wire, it belongs in `provider` instead.

Core depends on `elefante-mem` only through `memory.rs`; the loop sees a `ContextSource` trait
object and never a store.

Core does not know it is running under a daemon. `srv` drives it; `srv` is not visible from
here, and that direction must stay.
