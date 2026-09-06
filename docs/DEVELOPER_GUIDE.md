# Elefante developer guide

This guide explains the code that exists today, the contracts between crates, and the
invariants a contributor must preserve. Read [`PLAN.md`](./PLAN.md) for the longer-term
architecture and [`MILESTONES.md`](./MILESTONES.md) for delivery status.

## 1. Current scope

The repository is at M1: a provider-backed coding loop behind a daemon, driven by either a
headless client or an OpenTUI terminal client. The implemented vertical slice can:

- send a prompt and stable tool catalog to OpenRouter;
- stream reasoning and text, separated end to end;
- assemble provider events into replayable conversation history that persists across turns;
- execute model-requested tools concurrently, and cancel them mid-flight;
- enforce path, shell, and output-scrubbing policy, suspending for approval when the shell
  classifier escalates;
- serve many clients per session and many sessions per client, with resume after disconnect;
  and
- continue the conversation until the provider ends the turn, refuses, is interrupted, or
  reaches a configured round limit.

Crates that remain compile-safe scaffolds, because their dependency boundaries are expensive
to retrofit. These do not imply that later milestones have shipped:

- `elefante-mem` contains only an M2 availability marker; there is no database or graph.

Both milestone gates are green, including the credentialed OpenRouter cache-hit test and the
live repository edit. CI runs the hermetic gates on every push and the credentialed pair
behind `workflow_dispatch`; see `.github/workflows/`.

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
└── crates/
    ├── cli/                       `elefante` terminal binary
    ├── core/                      provider/tool conversation loop
    ├── provider/                  normalized model API + OpenRouter transport
    ├── guard/                     path, shell, and secret-output policy
    ├── tools/                     schemas and syscall-facing tool executor
    ├── rpc/                       wire envelopes and framing
    ├── srv/                       daemon, session registry, ring
    └── mem/                       M2 memory scaffold
```

All crates inherit Rust 2024 edition, the minimum Rust version, dependency versions, and
workspace lints from the root manifest. `unsafe` code is forbidden. Clippy's `all` and
`pedantic` groups are warnings locally and are promoted to errors by the documented check.

## 4. Crate responsibilities and dependency direction

The current internal dependency graph is:

```text
elefante-cli ──► elefante-core ──► elefante-provider
      │                 │
      │                 └────────► elefante-tools ──► elefante-guard
      ├────────► elefante-provider
      └────────► elefante-tools

elefante-srv ──► elefante-core
      └────────► elefante-rpc

elefante-mem ──► elefante-guard
elefante-rpc ──► no internal crate
```

### `elefante-cli`

The binary is composition code. It parses arguments, reads environment configuration,
constructs `OpenRouterProvider` and `ToolExecutor`, builds `RunConfig`, and calls
the daemon. It is a client: it spawns `elefante-srv` if none is listening, then streams a
session over the socket. The answer goes to stdout and reasoning plus tool activity to
stderr, which is what resolves issue #4 — the two can no longer render as one undelimited
run, by construction rather than by formatting.

Keep provider protocol parsing, policy, and orchestration out of this crate. Prefer
this direct composition path with daemon/client startup behavior.

### `elefante-core`

Core owns conversation semantics:

- building provider-neutral requests;
- consuming normalized streaming events;
- retaining assistant content in provider order;
- validating tool-use lifecycle and IDs;
- executing a batch of tools concurrently while retaining deterministic result order;
- returning every tool result together as one batch; and
- enforcing refusal and round-limit behavior.

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

Do not add an alternate read, write, process, glob, or search path that bypasses this crate's
policy calls.

### Scaffold crates

`elefante-rpc`, `elefante-srv`, and `elefante-mem` preserve the intended workspace topology
without implementing M1/M2 early. Their APIs can change freely until their milestone design
passes are complete.

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
- Events outside a turn carry `turn_id: null`.

A gap in `seq` means the client missed something and must resynchronise. It must never be read
as "nothing happened."

### Method set

Client → server:

| Method | Params | Result |
|---|---|---|
| `session.create` | `{ cwd, model?, effort?, max_rounds?, max_tokens? }` | `{ session_id, seq }` |
| `session.prompt` | `{ session_id, text }` | `{ turn_id }` — notifications follow |
| `session.interrupt` | `{ session_id }` | `{}` |
| `session.attach` | `{ session_id, since?: number }` | see **Attach and resume** |
| `session.close` | `{ session_id }` | `{}` |
| `session.resolve_approval` | `{ session_id, approval_id, decision: "allow" \| "deny" }` | `{}` |
| `key.set` | `{ api_key }` | `{}` — first-run setup |

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

`approval.requested` carries **no `reason`**: `BashDecision` has `verdict` and `rule` only, and
a prose reason is the M4 triage-critic verdict.

`approval.resolved` is required by fanout — with several clients attached, one resolves the
prompt and the rest must dismiss their modal.

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
removed span replaced by exactly `\n… [truncated]`.

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
`turn.finished` — are never merged or dropped.

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

Transport and provider failures *inside* a turn are not RPC errors — the request already
succeeded. They arrive as `session.error` followed by `turn.finished`.

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
| `mid_conversation_system` | `true` for OpenRouter — an OpenAI-shaped `messages` array accepts a `system` role at any position. Gains its consumer in M2/M3, when memory blocks and critic findings are placed mid-conversation. |
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
| `memory_search` | `query` | Deterministic M2-not-available error. |

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
- this developer guide if a public model, interaction, invariant, or workflow changed;
- the README if setup, user-visible behavior, or project status changed; and
- the design/milestone documents when their respective contracts require it.

## 14. Known limitations and next boundaries

- `memory_search` has no backing graph until M2.
- Usage is parsed but not accumulated into cost/staleness reporting until M5.
- There is no model-backed critic.
- `session.list` waits for the tmux fanout work, so a client cannot enumerate sessions it did
  not create.
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

### `memory_search is unavailable until milestone M2`

This is expected. Repository inspection currently uses `read`, `glob`, and `grep`.

### Provider stream ends with a protocol error

Start with the provider fixture tests. Check SSE framing, `: OPENROUTER PROCESSING`
keep-alives being mistaken for frames, `tool_calls` fragments accumulated by the wrong key, the
`[DONE]` sentinel, terminal stop events, and verbatim `reasoning_details` replay before changing
core validation.
