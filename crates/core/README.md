# elefante-core

The provider-neutral agent loop. Core owns conversation semantics — building requests, consuming
normalized events, validating the tool-use lifecycle, dispatching tool batches, and deciding
when a turn ends. It knows no OpenRouter field names and performs no syscalls.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 907 | `Session`, `ToolGate`, `RoundAccumulator`, `ContextSource`, `run_agent_loop`, and every public type. |
| `src/memory.rs` | 89 | `MemContext` and `MemSearchTool` — the repository index as a source of context and as the backing for `memory_search`. |

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
cargo test -p elefante-core     # 28 tests, no network
```

`tests/session.rs` (13) covers multi-turn history, cancellation, and pairing under interrupt.
`tests/loop.rs` (6) covers the single-turn path. `tests/context.rs` (8) covers where context goes in
a request and that it never enters history. `tests/prompt.rs` (1) asserts the default system prompt. Nothing is `#[ignore]`d; every test drives a scripted in-process provider, so the suite
needs no credentials.

## Boundaries

No provider wire formats, no filesystem access, no socket handling. Core takes a `&dyn Provider`
and a `&ToolExecutor` and calls them; if a change here needs to know how a tool result is shaped
on the wire, it belongs in `provider` instead.

Core depends on `elefante-mem` only through `memory.rs`; the loop sees a `ContextSource` trait
object and never a store.

Core does not know it is running under a daemon. `srv` drives it; `srv` is not visible from
here, and that direction must stay.
