# M1 — core types

**Read this before [`M1-PROTOCOL.md`](./M1-PROTOCOL.md). The protocol is derived from these
types and cannot be implemented without them.**

## Why this is a separate document

An adversarial review of the first M1 contract found four blocking defects sharing one root
cause: the wire was frozen before the core types the wire must be derived from. Every
notification in the protocol's table was unproducible from the shipped `OutputEvent`; the
session type it presupposed did not exist; the interrupt guarantee it made was unachievable
against `join_all`. `PLAN.md` had predicted exactly this hazard from the other direction and
the first draft made the inverted version of the mistake.

Splitting the contract in two is what prevents a repeat, and the split is **for parallel
work**:

| Stream | Works from | May start |
|---|---|---|
| Rust core — types, session, cancellation, guard relocation | this document | once this document is frozen |
| Rust daemon — socket, registry, wire mapping | `M1-PROTOCOL.md` | once this document is frozen |
| TypeScript client — protocol, reducer, UI | `M1-PROTOCOL.md` | once this document is frozen |

**Freezing is not the same as implementing.** The daemon and client streams depend on these
types being *decided*, not on them being *written*. Once this document is settled, all three
proceed concurrently against fixed shapes.

**Neither protocol stream may start before this document is frozen.** That is the whole
reason it is separate: a wire change discovered mid-implementation invalidates work in two
languages at once, and the TypeScript side has no compiler to catch it.

Scope: `elefante-core`, `elefante-tools`, `elefante-guard`. The daemon and client are out of
scope here and specified in `M1-PROTOCOL.md`.

---

## 1. `OutputEvent`

The shipped enum has four variants (`crates/core/src/lib.rs:47-61`) and cannot produce most
of the protocol's notifications. M1 shape:

```rust
pub enum OutputEvent {
    TurnStarted,
    ReasoningDelta(String),
    TextDelta(String),
    ToolStarted { id: String, name: String, input_preview: String },
    ToolFinished {
        id: String, name: String, is_error: bool,
        output_preview: String, duration_ms: u64,
    },
    ApprovalRequested { approval_id: String, command: String, rule: &'static str },
    ApprovalResolved { approval_id: String, approved: bool },
    TurnFinished { stop_reason: StopReason, usage: Usage },
    Error { message: String },
}
```

Notes that are decisions, not description:

**`ToolStarted` moves to dispatch time.** Today it is emitted when the provider sees the
first streamed fragment of a tool call (`crates/provider/src/openrouter.rs:534-537` →
`crates/core/src/lib.rs:330-336`), which is *before any arguments have arrived* — the input
is not known until `ToolUseComplete` at stream end. Carrying a complete `input_preview`
therefore requires emitting it when the call is dispatched instead. This changes its
ordering relative to `text.delta` and `reasoning.delta`, which is observable in the
transcript. That is acceptable; a tool row appearing without its arguments is not.

**`ApprovalResolved` exists for fanout.** With several clients attached, one resolves the
prompt and the others must dismiss their modal. Without this event the losing client's
modal sits there until the user clicks it and gets an error.

**No `reason` field on `ApprovalRequested`.** `BashDecision` carries `verdict` and `rule`
only (`crates/guard/src/bash.rs:34-38`). A prose reason is the triage-critic verdict, which
is M4. A field that is always null is worse than an absent one.

**`Error` is a turn-level abnormal end**, always followed by `TurnFinished`. It is not the
channel for daemon-to-client transport problems; the protocol handles those separately.

---

## 2. `Session` replaces `run_agent_loop`

`run_agent_loop` (`crates/core/src/lib.rs:220-229`) takes a prompt, builds `messages`
locally, and drops them on return; `RunOutcome` carries no history. A second prompt would
start from zero, so multi-turn sessions — the entire premise of a session registry — are not
implementable against it.

```rust
pub struct Session {
    messages: Vec<Message>,
    config: RunConfig,
    usage: Usage,          // cumulative across turns
}

impl Session {
    pub fn new(config: RunConfig) -> Self;

    /// Runs one turn, appending to the session's history.
    pub async fn prompt<F>(
        &mut self,
        provider: &dyn Provider,
        tools: &ToolExecutor,
        gate: &ToolGate,
        prompt: impl Into<String>,
        cancel: CancellationToken,
        emit: F,
    ) -> Result<TurnOutcome, LoopError>
    where F: FnMut(OutputEvent);

    pub fn messages(&self) -> &[Message];
    pub fn usage(&self) -> Usage;
}

pub struct TurnOutcome {
    pub stop_reason: StopReason,
    pub rounds: usize,
    pub text: String,
    pub usage: Usage,      // this turn only; Session::usage is cumulative
}
```

History lives in the `Session` and survives the call. `RunOutcome` becomes `TurnOutcome` and
keeps its current fields — per-turn totals stay per-turn, and the cumulative figure the
status line needs comes from `Session::usage`.

`emit` stays a synchronous `FnMut`. The daemon's callback pushes into the session ring; it
must not block, and making it async would force the whole loop to carry a lifetime it does
not need.

---

## 3. Cancellation

`session.interrupt` promises that already-completed tool results still return, so the
assistant message and its `tool_result` blocks stay paired. That is unachievable against
`join_all` (`crates/core/src/lib.rs:389-396`): it is one composite future with no per-task
handles, and dropping it discards completed results along with pending ones.

Dispatch becomes a `JoinSet`, which `PLAN.md:335` already specifies and the code never
implemented:

```rust
let mut set = JoinSet::new();
for call in &calls { set.spawn(execute_one(call.clone())); }

let mut results = HashMap::new();
loop {
    tokio::select! {
        Some(joined) = set.join_next() => { /* record */ }
        () = cancel.cancelled() => { set.shutdown().await; break; }
        else => break,
    }
}
```

**Every `tool_use` must receive a `tool_result`, including aborted ones.** A cancelled call
gets a synthesized `is_error: true` result reading `interrupted`, never omission. Pairing is
the invariant — an unpaired `tool_use_id` makes the next provider request 400, and an
interrupted session that cannot be resumed is worse than one that stops cleanly.

`CancellationToken` comes from `tokio-util`, a new workspace dependency.

---

## 4. The gate moves from `tools` to `core`

`ToolExecutor::bash` currently classifies *and* self-denies
(`crates/tools/src/lib.rs:227-239`), with a comment noting M0 has no approval transport.
That cannot work for M1: approval must suspend and await a client decision, and `tools` has
no route upward — the dependency direction is `tools → guard` with nothing above it.

`PLAN.md:333` already places `guard::check` in core before dispatch. The code diverged; M1
brings it back.

```rust
pub enum GateDecision {
    Allow,
    Deny { rule: &'static str },
    Escalate { rule: &'static str, command: String },
}

pub struct ToolGate { classifier: BashClassifier }

impl ToolGate {
    pub fn check(&self, name: &str, input: &Value) -> GateDecision;
}
```

Core's dispatch:

| Decision | Action |
|---|---|
| `Allow` | `tools::execute` |
| `Deny` | synthesize an error result; the filesystem is never touched |
| `Escalate` | emit `ApprovalRequested`, await the decision, then execute or synthesize |

`PathPolicy` **stays in `tools`**, at the syscall boundary. Only the bash classifier's call
site moves. The distinction: path policy is a property of every filesystem operation and
belongs where the operation happens; bash escalation is a question for a human and belongs
where a human can be reached.

### Why `Verdict` keeps three states

`Escalate` and `Deny` behave identically in M0 — both return a non-executing error — which
makes the third state look redundant. It is not; it is uncashed. The two are different
modalities, not different severities:

- **`Deny` is terminal.** No human, no critic, no flag clears it.
- **`Escalate` is a question.** It has an answer, and the answer may be yes.

Collapsing them loses the ability to say *which denials are negotiable*. Once the TUI has an
approve affordance it attaches to whatever "denied" means, so a bool would put an approve
button on `rm -rf /`. The M4 triage critic routes escalations to a cheap model and must
never see denials. Auto mode is only safe because `Deny` is a state the critic cannot clear.
And the nearest bool encoding, `{allowed, approvable}`, admits the nonsense
`{allowed: true, approvable: true}` — the enum makes that unrepresentable.

**M1 is when this stops being theoretical.** If approval slips again, collapse the enum
rather than carry a third state through another milestone pretending to mean something.

### Awaiting a decision

`Session::prompt` takes approvals through a channel the daemon owns:

```rust
pub trait ApprovalSink: Send + Sync {
    /// Resolves when a client answers, or when the turn is cancelled.
    async fn request(&self, id: &str, cancel: &CancellationToken) -> ApprovalOutcome;
}

pub enum ApprovalOutcome { Approved, Denied, Cancelled }
```

Injected, never constructed inside `core` — the same rule `PLAN.md:70` applies to the critic
and its `Provider`. A headless client with no way to prompt supplies a sink that returns
`Denied` immediately, which keeps escalated commands non-executing in scripts without a
special case in the loop.

---

## 5. What this unblocks

| Protocol element | Depends on |
|---|---|
| `turn.started`, `turn.finished` | §1 `TurnStarted` / `TurnFinished` |
| `tool.started { input_preview }` | §1, and the dispatch-time move |
| `tool.finished { output_preview, duration_ms }` | §1 |
| `session.error` | §1 `Error` |
| multi-turn sessions, snapshot `turns[]` | §2 |
| `session.interrupt` guarantees | §3 |
| `approval.requested` / `resolve` / `resolved` | §4 |

---

## Gate

Core-layer only. The protocol's gate is in `M1-PROTOCOL.md` and assumes these hold.

- A `Session` given two prompts in sequence sends the second request with the first turn's
  messages in history.
- `Session::usage` accumulates across turns while `TurnOutcome::usage` stays per-turn.
- Interrupt during dispatch: tools that completed return their real results, tools that were
  aborted return synthesized `is_error` results, and **every `tool_use` has exactly one
  matching `tool_result`**.
- A denied bash command is refused by `core`'s gate, and `ToolExecutor::bash` no longer
  classifies — asserted by calling the executor directly and observing that it runs what it
  is given.
- An escalated command suspends, and resolving the approval either executes it or
  synthesizes a denial.
- An `ApprovalSink` that returns `Denied` immediately keeps escalated commands
  non-executing, with no branch in the loop.
- `ToolStarted` carries a complete `input_preview`, asserted against a scripted provider that
  streams tool arguments across several fragments.
- **M0 gate still green**, with one relocation: `bash_gate_denies_and_escalates_without_execution`
  moves from `crates/tools/tests/` to `crates/core/tests/`, because the behaviour moves.
