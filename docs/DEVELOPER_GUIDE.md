# Elefante developer guide

This guide explains the code that exists today, the contracts between crates, and the
invariants a contributor must preserve. Read [`PLAN.md`](./PLAN.md) for the longer-term
architecture and [`MILESTONES.md`](./MILESTONES.md) for delivery status.

## 1. Current scope

The repository is at M0: a provider-backed coding loop running in a plain terminal. The
implemented vertical slice can:

- send a prompt and stable tool catalog to Anthropic;
- stream summarized thinking and text;
- assemble provider events into replayable conversation history;
- execute model-requested tools concurrently;
- enforce path, shell, and output-scrubbing policy; and
- continue the conversation until the provider ends the turn, refuses, or reaches a
  configured round limit.

Several crates already exist as compile-safe scaffolds because their dependency boundaries
are expensive to retrofit. They do not imply that later milestones have shipped:

- `elefante-rpc` contains only a request envelope; the M1 protocol is not implemented.
- `elefante-srv` contains only an M1 marker; there is no daemon or session registry.
- `elefante-mem` contains only an M2 availability marker; there is no database or graph.
- There is no `tui/` directory yet.

M0's hermetic gate is green. The credentialed Anthropic cache-hit test and live repository
edit have not run, so the milestone remains formally in progress.

## 2. Design principles

The implementation follows four project-level convictions:

1. **Keep standing instructions small.** If a behavior can be enforced by a type, policy,
   gate, or test, enforce it there. The core prompt describes intent; it does not pretend to
   be a filesystem or process sandbox.
2. **Normalize at boundaries.** Core code works with provider-neutral requests, content,
   events, usage, and stop reasons. Anthropic JSON and SSE details stay in the provider crate.
3. **Make unsafe paths fail closed.** Filesystem aliases are resolved before access, shell
   syntax is classified across the full parse tree, and unclassified mutation requires an
   approval mechanism that M0 intentionally does not provide.
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
    ├── elefante-cli/              `elefante` terminal binary
    ├── elefante-core/             provider/tool conversation loop
    ├── elefante-provider/         normalized model API + Anthropic transport
    ├── elefante-guard/            path, shell, and secret-output policy
    ├── elefante-tools/            schemas and syscall-facing tool executor
    ├── elefante-rpc/              M1 protocol scaffold
    ├── elefante-srv/              M1 daemon scaffold
    └── elefante-mem/              M2 memory scaffold
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
constructs `AnthropicProvider` and `ToolExecutor`, builds `RunConfig`, and calls
`run_agent_loop`. It renders thinking and text deltas directly to stdout. Tool lifecycle
events are available from core but are not rendered by the M0 terminal frontend.

Keep provider protocol parsing, policy, and orchestration out of this crate. M1 will replace
this direct composition path with daemon/client startup behavior.

### `elefante-core`

Core owns conversation semantics:

- building provider-neutral requests;
- consuming normalized streaming events;
- retaining assistant content in provider order;
- validating tool-use lifecycle and IDs;
- executing a batch of tools concurrently while retaining deterministic result order;
- returning all results in one user message; and
- enforcing refusal and round-limit behavior.

Core does not know Anthropic field names and does not perform filesystem operations.

### `elefante-provider`

The crate has two layers:

- `lib.rs` defines the normalized provider contract and shared data models.
- `anthropic.rs` maps that contract onto `POST /v1/messages`, parses incremental SSE, and
  maps wire events back to normalized events.

Provider capability differences are descriptive. A transport may emit only fields supported
by the selected model. The current Anthropic capability resolver uses model-name matching;
update its tests whenever supported model families change.

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

One CLI invocation follows this path:

```text
prompt / stdin
     │
     ▼
elefante-cli
  ├─ reads flags and environment
  ├─ roots ToolExecutor at current_dir()
  └─ calls core::run_agent_loop
           │
           ▼
      build normalized Request
           │
           ▼
      Provider::stream ──► Anthropic request JSON ──► SSE response
           ▲                                         │
           └──────────── normalized Event stream ◄───┘
           │
           ├─ thinking/text delta ──► frontend callback
           ├─ complete content block ──► assistant history
           └─ stop_reason == tool_use
                          │
                          ▼
                    concurrent tool batch
                          │
             ┌────────────┴─────────────┐
             ▼                          ▼
       guard decision              tool syscall
             │                          │
             └────────────┬─────────────┘
                          ▼
                    scrub every result
                          │
                          ▼
             one user message with all results
                          │
                          └──────────────► next provider round
```

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

Core then appends exactly one user message containing every `ToolResult` block. This is a
protocol invariant, not a formatting preference.

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
| `adaptive_thinking` | The model accepts adaptive thinking configuration. |
| `effort` | The model accepts `output_config.effort`. |
| `prompt_caching` | Stable system/tool cache breakpoints may be sent. |
| `mid_conversation_system` | Memory or findings may later use system-role placement. |
| `structured_output` | Strict schemas may be requested. |
| `cache_metrics` | Cache creation/read usage is observable. |

The core must degrade behavior based on capabilities rather than matching provider names.

### Request and conversation types

`Request` contains a `ModelId`, `max_tokens`, system blocks, sorted tool definitions,
conversation messages, and optional `Effort`.

`Message` has a `Role` and ordered `ContentBlock` values. Current content variants are:

| Variant | Purpose |
|---|---|
| `Text` | Visible assistant or user text. |
| `Thinking` | Complete summarized/omitted thinking plus opaque signature for replay. |
| `RedactedThinking` | Opaque encrypted block that must be replayed unchanged. |
| `ToolUse` | Tool ID, name, and parsed JSON input. |
| `ToolResult` | Matching tool-use ID, content, and error flag. |

Thinking blocks deserve special care. `ThinkingDelta` is display-facing, but complete
`ThinkingBlockComplete` and `RedactedThinkingBlockComplete` events carry replay state. Core
stores those complete blocks in the assistant message without rendering them. Reordering,
editing, or dropping these blocks can invalidate an Anthropic tool continuation.

### Streaming events

`Event` normalizes provider output into:

- visible thinking and text deltas;
- complete thinking/redacted-thinking replay blocks;
- tool start, input delta, and completion;
- usage; and
- a terminal `StopReason`.

`Usage` tracks input, output, cache-creation, and cache-read tokens. M0 parses it but does not
yet aggregate or display cost; that belongs to M5.

### Core result types

- `RunConfig` controls model, effort, token ceiling, round ceiling, and system prompt.
- `OutputEvent` is the frontend-facing subset: thinking/text deltas and tool lifecycle.
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

`RequestEnvelope` currently models only `jsonrpc`, `id`, `method`, and `params`. Do not infer
M1 method or notification stability from this placeholder.

## 7. Anthropic transport details

`AnthropicProvider` uses raw `reqwest` because there is no official Rust SDK assumed by the
project. Production requests use:

- `POST /v1/messages`;
- `x-api-key` from `ANTHROPIC_API_KEY`;
- `anthropic-version: 2023-06-01`; and
- `Accept: text/event-stream`.

The base URL is injectable through `ANTHROPIC_BASE_URL`, which is also how transport tests
use a loopback mock server.

### Stable request construction

Tool definitions are sorted before serialization. The final system block and final tool
definition receive ephemeral cache controls when caching is supported. Adaptive models get
`thinking: { type: "adaptive", display: "summarized" }`, plus configured effort. The client
never constructs the removed `budget_tokens` field.

Changing system bytes, tool order, JSON shape, thinking mode, or messages before a cache
breakpoint can invalidate the prefix. Treat request serialization tests as behavioral tests,
not snapshots that can be casually regenerated.

### SSE parsing

`SseDecoder` handles frames split across arbitrary network chunks, both LF and CRLF frame
boundaries, comments, and multi-line data. `AnthropicEventParser` owns content-block state,
incremental tool JSON, thinking signatures, usage, refusals, and stop reasons. Unknown future
event types are ignored so compatible protocol additions do not terminate a session.

Tool input is parsed with `serde_json`; never reconstruct JSON with string splitting.

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
| `memory_search` | `query` | Deterministic M2-not-available error in M0. |

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

M0 has no approval channel, so `Escalate` returns a deterministic error and does not execute.
M1 may suspend for approval, but it must reuse the same classifier result rather than bypass it.

Static shell analysis cannot understand runtime aliases, generated program names, `eval`, or
every interpreter payload. The classifier deliberately documents known-not-caught cases; do
not describe it as a sandbox.

### Output scrubber

Every `ToolExecutor::execute` result—success or error—is scrubbed before returning to core.
Current patterns cover Anthropic key prefixes, AWS access-key shapes, PEM blocks, and
token/secret/password assignments. The same implementation is intended for future memory
writes so transient leaks cannot become durable graph data.

The scrubber is defense in depth, not permission to weaken path or shell policy.

## 10. Configuration and CLI behavior

Configuration precedence and defaults:

| Concern | Highest precedence | Environment fallback | Default |
|---|---|---|---|
| Model | `--model` | `ANTHROPIC_MODEL` | `claude-opus-5` |
| Effort | `--effort` | `ELEFANTE_EFFORT` | `high` |
| Max rounds | `--max-rounds` | — | `24` |
| Max tokens | `--max-tokens` | — | `16,384` |
| API origin | — | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
| API key | — | `ANTHROPIC_API_KEY` | required |

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
- `elefante-provider` tests deterministic request JSON, capabilities, arbitrary SSE chunking,
  tool JSON, signed/redacted thinking replay, usage, refusals, and forward-compatible events.
- `elefante-core/tests/loop.rs` uses a scripted provider to test a complete edit loop,
  parallel result pairing, refusal, and malformed/incomplete streams without network access.

The provider's always-on transport tests bind a loopback TCP listener. If a sandbox rejects
local sockets, permit loopback binding rather than disabling the test.

### Credentialed provider check

The live cache test is ignored by default because it uses network access and billable API
requests. Run it explicitly with a model known to support the configured behavior:

```sh
export ANTHROPIC_API_KEY="..."
export ANTHROPIC_TEST_MODEL="..."
cargo test -p elefante-provider live_second_identical_request_reads_prompt_cache -- --ignored
```

Do not record M0 complete until this check reports a nonzero cache read on the second request
and a live agent request lands a trivial, reviewed edit in this repository.

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
9. Confirm Anthropic request-order/cache tests remain green.

Do not expose a second tool executor or construct tool results in core to work around this
flow. The single boundary is what makes “every result is scrubbed” auditable.

### Adding a provider

1. Implement the normalized `Provider` trait.
2. Resolve `Capabilities` per model when support varies.
3. Send only fields the model declares.
4. Normalize text, thinking, tools, usage, refusal, and stop behavior without leaking wire
   types into core.
5. Preserve any opaque replay blocks exactly and in order.
6. Use a configurable endpoint and hermetic transport fixtures.
7. Test malformed chunks, provider errors, refusal, tool-result continuation, and capability
   degradation.

OpenRouter belongs to M4. Do not add it merely as an Anthropic fallback; the design treats
providers as peers selected by role.

### Changing the loop

Protect these invariants with regression tests first:

- one live turn per invocation;
- at most one assistant history message per provider response;
- complete opaque thinking blocks retained in order;
- every tool call receives one matching result;
- all parallel results returned in one user message;
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

- M0 approvals are non-interactive: `Escalate` is a safe error, not a prompt.
- `memory_search` has no backing graph until M2.
- Usage is parsed but not accumulated into cost/staleness reporting until M5.
- There is no daemon, socket, session registry, reconnect, interrupt, or multi-client support.
- There is no OpenTUI client.
- There is no OpenRouter provider or model-backed critic.
- The guard layer reduces blast radius but is not an OS sandbox and cannot eliminate
  time-of-check/time-of-use filesystem races.

These limitations are milestone boundaries. Implement them through the corresponding plan
and gate rather than hiding partial versions behind existing APIs.

## 15. Troubleshooting

### `ANTHROPIC_API_KEY is not set`

Export the key into the process environment. Do not create a repository `.env`; it is ignored
by Git and denied by Elefante's own path policy.

### `bash requires approval (unavailable in M0)`

The classifier returned `Escalate`. Use a narrower existing tool (`read`, `write`, `edit`,
`glob`, or `grep`) or wait for the M1 approval transport. Do not broaden the shell allowlist
just to make a task convenient.

### `secret-path-policy: ...`

The requested lexical or resolved path is outside the workspace or matches a protected rule.
Move non-secret fixture data to an allowed path; never rename real credentials to evade policy.

### `memory_search is unavailable until milestone M2`

This is expected M0 behavior. Repository inspection currently uses `read`, `glob`, and `grep`.

### Provider stream ends with a protocol error

Start with the provider fixture tests. Check SSE framing, terminal stop events, tool block
completion, and exact thinking-signature replay before changing core validation.
