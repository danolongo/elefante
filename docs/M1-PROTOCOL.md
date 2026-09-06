# M1 — daemon, protocol, and TUI client

**Prerequisite: [`M1-CORE.md`](./M1-CORE.md). Read it first.** This document is *derived*
from those types — every notification below maps to an `OutputEvent` variant defined there,
and none of them can be produced until it is frozen.

**Do not start work from this document until `M1-CORE.md` is frozen.** The two are separate
so that the daemon and the TypeScript client can proceed **in parallel** once the core types
are decided — freezing is not the same as implementing, and neither protocol stream waits on
the Rust core being written. What they cannot survive is a *change* to those types
mid-implementation: it invalidates work in two languages at once, and the TypeScript side
has no compiler to catch it.

Scope: `elefante-rpc` (wire types), `elefante-srv` (daemon), `elefante-cli` (becomes a
client), `tui/` (new). Core, tools, and guard changes are specified in `M1-CORE.md`.

An adversarial review of the first draft of this document found four blocking defects, all
from specifying the wire before the types it derives from. The corrections are folded in
below; the split into two documents is the structural fix.

---

## Decisions settled before writing this

| Question | Decision | Why |
|---|---|---|
| What does `attach` return? | Snapshot + `seq`-addressed deltas | The gate needs both mid-turn resume (delta) and a fresh second client (snapshot). Only this serves both. |
| Sequencing | One monotonic `seq` per session, `turn_id` on every event | A second per-turn counter is derivable from `turn_id` and adds a consistency bug class. |
| Delivery | Bounded ring, client dedupes on `seq`, explicit "cannot resume" | The silent-gap failure is the one that matters; it must be an answer, not an omission. |
| Backpressure | Coalesce in the per-client queue; drop the client as backstop | Text deltas are associative, so merging is lossless. Blocking the turn on the slowest client would let a suspended terminal freeze the agent. |
| Ring size | 4,096 events per session | ~40–70 s of streaming at 60–100 tok/s, against a disconnect window measured in seconds. |
| CLI | Daemon-always; one-shot runs use a headless client | One execution path. A dual path would leave the RPC unexercised in CI and subagents unobservable. |
| `session.list` | Deferred with the tmux work | Nothing in M1 needs to enumerate sessions. |

---

## Transport

Unix domain socket at `$XDG_RUNTIME_DIR/elefante.sock`, falling back to
`~/.elefante/run/elefante.sock`. `ELEFANTE_SOCKET` overrides the path — required for test
isolation, see **Testing the daemon**.

**Access is enforced by a `0700` parent directory, created before the socket.**
`UnixListener::bind` applies the process umask and there is no safe API to bind with an
explicit mode; the umask route needs `libc::umask`, which is `unsafe`, and the workspace sets
`unsafe_code = "forbid"`. Chmod-after-bind leaves a window. A private directory closes the
window, needs no `unsafe`, and is the conventional answer. The first draft required mode
`0600` "at creation, not after", which is not satisfiable under our own lint configuration.

**Framing: newline-delimited JSON.** One JSON value per line, `\n` terminated, UTF-8. No
embedded raw newlines — `serde_json` escapes them inside strings, which is what makes the
framing safe. A line longer than 8 MiB is a protocol error and closes the connection;
nothing legitimate approaches that once tool previews are capped.

**JSON-RPC-*shaped*, not JSON-RPC 2.0 conformant.** Requests and responses follow the 2.0
member layout (`id`, `method`, `params`, `result`, `error`), but notifications carry
top-level `seq`, `turn_id`, and `session_id` members, which strict 2.0 does not permit. This
is deliberate — those members are routing and ordering metadata that every consumer needs
before it looks at `params` — but it means **an off-the-shelf JSON-RPC library will not
round-trip our notifications**. Both sides hand-roll the envelope. Claiming conformance and
then extending it is how the two implementations silently diverge.

Bidirectional over one connection: client→server requests carry `id` and get exactly one
response; server→client notifications carry no `id` and are never acknowledged.

The scaffold `RequestEnvelope` (`crates/rpc/src/lib.rs:8-14`) has a non-`Option` `id` and
cannot represent a notification. It is replaced, not extended.

---

## Sequencing

Every notification carries:

```jsonc
{ "session_id": "s1", "seq": 413, "turn_id": "t7", "method": "text.delta", "params": { ... } }
```

- **`session_id` is mandatory on every notification.** A client may attach to several
  sessions on one connection, and `seq` is per-session — without it the merged stream reads
  `413, 7, 414, 8`, which trips the gap rule below on every alternation and makes the dedupe
  rule discard live events. Omitting it was a blocking defect in the first draft.
- `seq` is **monotonic per session**, starting at 1, never reused, never reset — including
  across turns and across daemon-side reconnects. `session.create` returns 0, so the first
  event is 1 and no event is lost to an off-by-one.
- `turn_id` groups events. "The third event of turn 7" is computed client-side by counting
  within a `turn_id`; there is deliberately no second counter to keep in sync.
- Events not belonging to a turn (`session.stale`, and later `approval.requested` outside a
  turn) carry `turn_id: null`.

A gap in `seq` means the client missed something and must resynchronise. It must never be
interpreted as "nothing happened."

---

## Method set (M1 only)

Client → server. Anything not listed is out of scope for M1.

| Method | Params | Result |
|---|---|---|
| `session.create` | `{ cwd, model?, effort?, max_rounds?, max_tokens? }` | `{ session_id, seq }` |
| `session.prompt` | `{ session_id, text }` | `{ turn_id }` — notifications follow |
| `session.interrupt` | `{ session_id }` | `{}` |
| `session.attach` | `{ session_id, since?: number }` | see **Attach** |
| `session.close` | `{ session_id }` | `{}` |
| `session.resolve_approval` | `{ session_id, approval_id, decision: "allow" \| "deny" }` | `{}` |
| `key.set` | `{ api_key }` | `{}` — first-run setup |

`max_rounds` and `max_tokens` are carried because the M0 CLI accepts them
(`crates/cli/src/main.rs:42-49`) and the credentialed test uses them to bound its spend
(`crates/cli/tests/cli.rs:105-112`). Without a protocol destination those flags silently
vanish and that test's cost ceiling evaporates.

`session.close` exists because sessions otherwise accumulate for the daemon's lifetime, each
holding a 4,096-event ring plus full history. Closing is idempotent; a close during a live
turn cancels it first.

`session.create` validates `cwd`: it must exist, be a directory, and be canonicalisable.
A long-lived daemon that roots a `ToolExecutor` at any path a caller names is a larger blast
radius than the M0 CLI, which was confined to `env::current_dir()`.

Server → client notifications:

| Method | Params |
|---|---|
| `turn.started` | `{}` |
| `reasoning.delta` | `{ text }` — renderable |
| `text.delta` | `{ text }` |
| `tool.started` | `{ tool_call_id, name, input_preview }` |
| `tool.finished` | `{ tool_call_id, is_error, output_preview, duration_ms }` |
| `approval.requested` | `{ approval_id, command, rule }` |
| `approval.resolved` | `{ approval_id, approved }` |
| `turn.finished` | `{ stop_reason, usage }` |
| `session.error` | `{ message }` — turn ended abnormally, always followed by `turn.finished` |

Each maps 1:1 onto an `OutputEvent` variant in `M1-CORE.md` § 1.

`approval.requested` carries **no `reason`**: `BashDecision` has `verdict` and `rule` only
(`crates/guard/src/bash.rs:34-38`), and a prose reason is the M4 triage-critic verdict.

`approval.resolved` is required by fanout. With several clients attached, one resolves the
prompt and the rest must dismiss their modal; without it the losing client's modal sits
until the user clicks it and receives `-32005`.

**`reasoning.delta`, not `thinking.delta`** — the provider migration renamed these. Reasoning
*replay* state (`ReasoningDetails`) is never sent to clients: it exists so the next request
can echo it verbatim, and rendering it would present internal replay tokens as output.

`critic.findings`, `memory.search`, `stats.session`, `session.stale`, and `session.list`
belong to later milestones and are not implemented in M1.

### Wire shapes

The TypeScript side renders every one of these, so they are fixed here rather than inferred.

**`stop_reason`** — `StopReason` (`crates/provider/src/lib.rs:275-284`) derives
`#[serde(rename_all = "snake_case")]`, so `Other(String)` serialises as the **object**
`{"other": "..."}`, not a string. The wire value is therefore a union:

```jsonc
"end_turn" | "tool_use" | "max_tokens" | "refusal" | "stop_sequence"
  | "interrupted" | { "other": string }
```

`Interrupted` is a **new variant** added in M1; interrupt has no representation today.

**`usage`** — all six fields, always present, `cost` nullable:

```jsonc
{ "input_tokens": 0, "output_tokens": 0, "reasoning_tokens": 0,
  "cached_input_tokens": 0, "cache_write_tokens": 0, "cost": 0.0 | null }
```

`cost: null` means *not reported*, which is distinct from `0.0`.

**Truncation marker** — previews are cut at 2 KiB on a UTF-8 character boundary, never
mid-codepoint, and the removed span is replaced by exactly `\n… [truncated]`.

---

## Attach

```jsonc
// request
{ "method": "session.attach", "params": { "session_id": "s1", "since": 412 } }
```

Three possible outcomes, and the client must handle all three:

**1. Resumed.** `since` is still inside the ring.

```jsonc
{ "result": { "kind": "resumed", "from": 413, "events": [ ... ] } }
```

**2. Snapshot.** `since` was omitted, or predates the oldest buffered event.

```jsonc
{ "result": { "kind": "snapshot", "seq": 981, "snapshot": { ... } } }
```

The client discards local state and rebuilds from the snapshot. **This is the case that
must never be silent**: a daemon that answers a stale `since` with recent events produces a
transcript with an invisible hole.

**3. Unknown session.** A normal JSON-RPC error (see below).

`events[]` elements are **complete notification envelopes** — `session_id`, `seq`,
`turn_id`, `method`, `params` — identical to what live delivery sends. One shape, one
parser.

`since` is **exclusive**: `since: 412` yields `from: 413`.

### Ordering against live notifications

Attach races the live stream, and getting this wrong silently loses events — the failure
this whole section exists to prevent.

**The daemon holds notification delivery on a connection from the moment an `attach` for
that session is received until the response has been written.** Without that rule a
notification at `seq: 415` can arrive before a result carrying 413–416, and a client
deduping on a high-water mark then discards 413 and 414 as already-seen. The snapshot branch
is worse: an event that arrived before the snapshot is destroyed by the "discard local
state" step.

**Client-side dedupe is a seen-set over the resume window, not a high-water mark.** A
watermark cannot distinguish "already applied" from "arrived out of order", and the two need
different handling.

### Snapshot shape

Materialised session state, not an event log:

```jsonc
{
  "session_id": "s1",
  "seq": 981,                      // cursor: events after this are still streaming
  "cwd": "/path/to/repo",
  "model": "anthropic/claude-sonnet-5",
  "turns": [                       // completed turns, oldest first
    { "turn_id": "t1", "prompt": "...", "text": "...",
      "tools": [ { "name": "read", "is_error": false } ],
      "stop_reason": "end_turn", "usage": { ... } }
  ],
  "active_turn": {                 // null when idle
    "turn_id": "t7",
    "text_so_far": "...",
    "reasoning_so_far": "...",
    "tools": [ { "tool_call_id": "c1", "name": "grep", "state": "running" } ],
    "pending_approval": null
  }
}
```

Tool entries use **one shape everywhere** — `{ tool_call_id, name, state, is_error }` — in
both `turns[]` and `active_turn`. Two shapes for one concept forces the reducer to branch on
where it found the value.

`state` is `"running" | "ok" | "error" | "cancelled"`.

Full assistant text is retained per turn, and tool bodies are not — only previews. **This is
bounded per turn but not per session**: turn count grows without limit, and assistant text
is itself streaming volume. A long session's snapshot therefore grows unboundedly, which is
open question 1 rather than a solved problem. When it is solved, the answer belongs here.

---

## The ring, and the two buffers

These are different things and conflating them breaks resume.

**Session ring** — 4,096 events, source of truth for resume. Stores events **as emitted**,
each with its own `seq`. **Never coalesced**, because `since: 412` must address a real
event.

**Per-client output queue** — live delivery. This is where backpressure is handled.

**Per-client queue capacity is 256 events.** Without a stated number, "coalesced, not
dropped" cannot be implemented or tested — the test author picks the threshold and the item
passes on anything.

Under pressure the client queue coalesces adjacent deltas **of the same method** by
concatenating their text. A merged event carries **`seq_from` and `seq_to`** rather than a
single `seq`:

```jsonc
{ "session_id": "s1", "seq_from": 495, "seq_to": 500, "turn_id": "t7",
  "method": "text.delta", "params": { "text": "..." } }
```

The range is what makes a merge distinguishable from a drop. Carrying only the highest `seq`
would make every coalesce look like a gap, tripping the client's own resynchronise rule
during normal operation — a defect in the first draft. The client advances its seen-set to
`seq_to` and treats the whole range as delivered. Discrete events — `tool.started`, `tool.finished`, `approval.requested`,
`turn.finished` — are never merged or dropped.

**Coalescing is per-method and never crosses channels.** A `reasoning.delta` is never
merged into an adjacent `text.delta`, or the reverse. Merging across channels would destroy
the reasoning/answer distinction *in the transport* — and only under load, so issue #4 would
reappear as an intermittent defect that no render-site fix could reach. A queue fed
`reasoning, reasoning, text, text` emits **two** events, not one.

**Coalescing only helps on runs of the same method.** A tool-heavy turn interleaving
`text, tool.started, text, tool.finished` has no two adjacent deltas and coalesces nothing,
so a hung client on that workload falls through to eviction. This is a known limit, not an
oversight: merging across a discrete event would reorder it, and reordering `tool.started`
against the text that explains it is worse than dropping a client that has stopped reading.

If a client is genuinely hung and its queue overflows even after coalescing, the daemon
**closes the connection** and forgets it. It does not send `session.error` first: a client
whose queue is overflowing is by definition one that cannot receive it, and `session.error`
means "the turn ended abnormally" and promises a following `turn.finished`, neither of which
is true here. The connection closing *is* the signal. The session is unaffected; the client
reconnects and attaches.

**Preview cap: 2 KiB.** `input_preview` and `output_preview` are truncated to 2 KiB with a
marker. This is what makes a count-based ring safe — without it a single `grep` across a
large repository puts megabytes into one slot and the memory estimate collapses. The model
still receives the full tool result; only the client-facing preview is bounded.

---

## Session lifecycle

- **Create** — `session.create` registers a session, builds a `ToolExecutor` rooted at
  `cwd`, and returns its id.
- **Prompt** — takes the session's turn lock. A second prompt while a turn is live is
  **rejected, not queued**; silent queueing makes an interrupted session behave
  unpredictably.
- **Attach / detach** — many clients per session, many sessions per client. Detaching is
  not a signal to the session; it changes nothing about execution.
- **The turn continues with zero clients attached.** If every terminal dies mid-turn, tools
  keep running and events keep landing in the ring. This is precisely what makes "reconnect
  and the turn completes intact" achievable.
- **Idle exit** — the daemon exits after an idle period with **no clients *and* no active
  turns**. Checking only the client count would make the daemon kill its own work. The
  period defaults to 15 minutes and is set by `ELEFANTE_IDLE_SECS`; without the override the
  gate item asserting it would have to wait 15 minutes.

### Interrupt

`session.interrupt` cancels the in-flight provider stream and aborts the tool `JoinSet`.
Tool calls that already completed still return their results, so the assistant message and
its `tool_result` blocks stay paired — an unpaired `tool_use_id` makes the next request
400. The turn ends with `turn.finished { stop_reason: "interrupted" }`.

---

## Error model

Standard JSON-RPC error objects. Application codes in the `-32000` range:

| Code | Meaning |
|---|---|
| `-32001` | Unknown session |
| `-32002` | Turn already in progress |
| `-32003` | `since` is greater than the session's current `seq` — the client claims to have seen events that do not exist. A `since` that merely predates the ring is **not** an error; it returns a snapshot. |
| `-32004` | No API key configured — client should prompt and call `key.set` |
| `-32005` | Unknown approval id, or approval already resolved |

Transport and provider failures inside a turn are **not** RPC errors — the request already
succeeded. They arrive as `session.error` followed by `turn.finished`.

---

## CLI becomes a client

`elefante-cli` no longer runs the loop in process. `run_agent_loop` moves behind the
daemon; the binary connects to the socket, spawning the daemon first if the connect fails.

- `elefante` (no prompt) → spawn daemon if needed, launch the TUI client.
- `elefante "prompt"` → **headless client**: connect, `session.create`, `session.prompt`,
  stream to the terminal, exit on `turn.finished`.

The headless path preserves scripting *and* doubles as the daemon's test harness, so no
separate scaffolding is needed. Both existing CLI tests survive: they assert startup errors
and a live edit, and both hold through a client.

### Stream discipline

**`stdout` carries the answer and nothing else. `stderr` carries everything else** —
reasoning, tool activity, errors.

This is not cosmetic. Issue #4 records the current CLI rendering `ReasoningDelta` and
`TextDelta` through one match arm, so a run prints reasoning and answer as one undelimited
wall of prose, with sentences running together across delta boundaries. The information was
present in `OutputEvent` and discarded at the render site.

Splitting by stream fixes that **by construction** rather than by careful formatting:
`elefante "..." > summary.md` captures exactly the answer, with no reasoning to strip and
no delimiter to get right. Dimming reasoning on stdout — the obvious alternative — still
puts it in the redirected file.

Consequences that are therefore requirements:

- **Never write ANSI escapes to a stream that is not a TTY**, and honour `NO_COLOR`.
  `std::io::IsTerminal` covers the check; no new dependency.
- **A channel transition within one stream emits exactly one newline; consecutive
  same-channel deltas emit none.** Splitting streams does not retire this rule — `stderr`
  still interleaves reasoning with tool activity. Same-channel deltas must concatenate
  untouched, or streaming text gains spurious breaks mid-sentence.
- `tool.started` / `tool.finished` render on `stderr`. The current CLI matches these
  variants and discards them, so tool activity is invisible today.

### Where the render logic lives

`crates/cli/src/lib.rs`, a new lib target beside the existing `[[bin]]`.

A binary crate's internals are unreachable from `crates/cli/tests/`, and the M0 test
reorganisation established that tests live in per-crate test files. A lib target reconciles
the two — without it, the renderer is either untested or forces its own internals public.

```rust
pub struct StreamRenderer<W: Write> {
    out: W,
    styled: bool,
    last: Option<Channel>,
    at_line_start: bool,
}

impl<W: Write> StreamRenderer<W> {
    pub fn render(&mut self, note: &Notification);
    pub fn finish(&mut self);
}
```

It takes a **`Notification`**, not an `OutputEvent`. The headless client consumes the wire,
and `OutputEvent` has no representation for `session.error`, `turn.finished`, or approval —
all of which this renderer routes to stderr. Taking `OutputEvent` was a leftover from the
in-process design.

Two streams means two instances, each with its own `styled` and its own channel state.

Invariants, each one a test:

- a channel transition emits exactly one newline; consecutive same-channel deltas emit none;
- no ANSI bytes are written when `styled` is false;
- every faint span is closed by a reset, so a span interrupted by an error cannot leak
  dimming into the terminal;
- `finish()` terminates a mid-line stream. This replaces the current
  `if !outcome.text.ends_with('\n')` check in `main.rs`, which re-derives from accumulated
  text what the renderer already knows.

**No `--no-daemon` escape hatch.** A second execution path would drift, would leave the RPC
unexercised in CI, and — once fanout lands — would make subagents unobservable, since a
session with no socket cannot be attached to.

Startup cost is a one-time ~20–50 ms daemon spawn; a warm connect is ~0.1 ms. Against a
model time-to-first-token of 300 ms–2 s this is noise, and the warm case is *faster* than
in-process because the HTTPS connection pool is already established.

---

## Daemon startup

`crates/srv` currently has no `[[bin]]` and no `main.rs` — **the daemon binary does not
exist**. M1 adds `crates/srv/src/main.rs` and a `[[bin]] name = "elefante-srv"`.

**Locating it.** The client resolves the daemon as a sibling of `std::env::current_exe()`,
falling back to `PATH`. Sibling-first is what makes `cargo test` work: the test binary and
`elefante-srv` share a target directory, and a `PATH` lookup would find a stale installed
copy instead.

**Stale socket.** After a crash the socket file survives. `connect()` then fails with
`ECONNREFUSED` and `bind()` fails with `EADDRINUSE` — a permanent wedge, not a transient
one. The daemon therefore attempts `connect()` first; on `ECONNREFUSED` it unlinks the path
and binds. On success it exits, because another daemon is already live.

**Spawn races.** Two terminals starting at once both fail to connect and both spawn.
Resolution is bind-wins: the loser's `bind()` fails, it retries `connect()`, and attaches to
the winner. No lock file.

**Readiness.** After spawning, the client retries `connect()` every 10 ms for up to 2
seconds. If the daemon exits during startup the client reports its stderr rather than a
connect timeout — a misconfigured daemon must not look like a hung one.

---

## Testing the daemon

**There is no test seam otherwise, and roughly a third of the gate is unwritable without
one.** Once the provider lives inside the daemon, `OPENROUTER_BASE_URL` is read from
whatever shell started it — possibly hours earlier — so no hermetic test can produce a
deterministic stream.

Two mechanisms, both required:

**`ELEFANTE_SOCKET`** gives each test its own socket path in a temp directory. Without it a
test shares the developer's warm daemon, and `crates/cli/tests/cli.rs:16-19` — which clears
`OPENROUTER_API_KEY` and friends so that "an operator's real environment cannot make the
test pass or fail by accident" — becomes meaningless, because the environment that matters
is the daemon's, not the child's.

**`ELEFANTE_STUB_PROVIDER=<path>`** makes the daemon serve turns from a scripted JSON file
of `Event`s instead of calling OpenRouter. This is what makes the streaming, ordering,
resume, and stdout-content gate items assertable. It is refused unless `ELEFANTE_SOCKET` is
also set, so a stub can never be reached by a normal invocation.

---

## First-run key setup

When no key resolves, `session.prompt` fails with `-32004` rather than a transport error.

**On a TTY** the client prompts, calls `key.set`, and retries.

**When stdin is not a terminal** the client does not prompt. It writes
`OPENROUTER_API_KEY is not set` to stderr and exits non-zero. This preserves the M0 gate
item verbatim (`crates/cli/tests/cli.rs:26-45`), which asserts that exact text and a nonzero
exit in a harness with nobody to answer a prompt. `PLAN.md:171` anticipated this handover;
this is where it lands.

`key.set` **rebuilds the daemon's cached provider.** The provider is constructed with the
key, so a daemon that keeps the old instance fails the retry identically and traps the user
in a prompt loop.

`key.set` writes `~/.elefante/credentials.toml` at mode `0600`, created with those
permissions rather than chmod'd afterwards. **Never inside a working tree** — a key under
`<repo>/.env` sits in the directory the agent edits, which is why the path policy has to
guard it there.

`OPENROUTER_API_KEY` in the environment always wins over the stored value; environment
beats config everywhere else here.

---

## TUI

Three layers, two of which never touch OpenTUI. The constraint driving this is not
rendering — it is that the gate must be testable **without driving a real terminal**.

```
tui/src/protocol.ts    framing, JSON-RPC, seq tracking, dedupe, reconnect+since
tui/src/session.ts     pure reducer: (state, notification) -> state
tui/src/ui/*.tsx       OpenTUI components: render state, emit intents
```

**`session.ts` must contain no I/O.** That is what makes "two clients render the same
stream" a cheap assertion instead of a screen-scraping exercise, and it is the one rule in
this section worth treating as non-negotiable.

The gate item then splits into two inexpensive tests:

- **Daemon fan-out** — a Rust integration test spawns the daemon and two socket clients and
  asserts both receive identical event sequences. Tests the server.
- **Render determinism** — feed one event sequence into two reducers, assert deep-equal
  state. Tests the client.

### Components

- **Transcript** — user prompts, assistant text, reasoning (dimmed, collapsible), tool rows
  with status glyphs. **The reducer holds reasoning and answer text as distinct state, not
  one appended buffer.**

  This needs its own gate item, because the render-determinism check does not cover it: two
  reducers that both merge the channels identically are still deep-equal. The assertion
  passes on a reducer with exactly the #4 defect.
- **Status line** — model, effort, running token and cost totals from `usage`.
- **Input box** — submit, and Ctrl+C → `session.interrupt`.
- **Modal overlay** — approval prompts and first-run key entry. The same interaction shape,
  so one component.

### The silent-reasoning problem

A model can reason for a long time while emitting no renderable text, which reads as a
frozen UI. Whenever a turn is active and no `text.delta` has arrived, show a persistent
indicator with elapsed time and live reasoning-token count. This is a gate item, not
polish.

---

## Gate

Every item names what makes it *fail*. The first draft's gate contained several items that
passed on a broken implementation; those are corrected here and the correction is noted.

### Resume and sequencing

- Kill and reconnect a client mid-turn: the attach result has **`kind == "resumed"`**, its
  `from` equals `since + 1`, and the turn completes with output intact. *Asserting the kind
  is the correction: without it, an implementation that answers every attach with a snapshot
  passes — leaving the ring dead code.*
- Attach with a `since` older than the ring returns **`kind: "snapshot"`**, explicitly.
- Attach with `since` greater than current `seq` returns `-32003`.
- A notification pushed while an attach is in flight is delivered **after** the attach
  response, and the client's seen-set drops nothing. *Driven by holding delivery during
  attach; without the rule this test loses events.*
- `seq` is monotonic and gapless across turn boundaries **and across a client disconnect and
  reconnect**. *The reconnect half is the correction — the stated risk is a reset on
  reconnect, which a two-turn single-connection test cannot see.*

### Fanout

- Two clients attached to one session, **both draining promptly**, receive identical event
  sequences. *Qualified deliberately: under coalescing two clients draining at different
  rates receive different sequences by construction, so the unqualified form forbade the
  backpressure design.*
- A merged event carries `seq_from`/`seq_to`, and a client that applies it advances its
  seen-set to `seq_to` with no gap reported.
- A slow client whose 256-event queue fills is coalesced, not dropped; its merged text per
  channel equals the ring's unmerged text for that channel. A queue fed
  `reasoning, reasoning, text, text` emits **two** events, not one.
- A client that stops reading entirely has its connection closed, and **no `session.error`
  is sent** before the close.
- One client resolving an approval produces `approval.resolved` on every other attached
  client.

### Interrupt

- Interrupt during dispatch: completed tools report real results, aborted tools report
  synthesized `is_error` results, and **every `tool_use` has exactly one matching
  `tool_result`**.
- The stub provider **validates `tool_use`/`tool_result` pairing and rejects a mismatch**, so
  the pairing assertion can fail. *The correction: against a stub that ignores its input,
  "the next request does not 400" passes vacuously on an implementation that drops history.*
- `turn.finished` carries `stop_reason: "interrupted"`.

### Approval

- An escalated bash command emits `approval.requested` and does not execute while pending.
- Resolving `allow` executes it; resolving `deny` synthesizes an error result and the
  filesystem is untouched.
- A **denied** command — as opposed to escalated — emits no `approval.requested` at all.
  *This is what proves `Deny` and `Escalate` are distinct rather than nominally so.*
- An interrupt while an approval is pending resolves it as cancelled and still pairs its
  `tool_result`.

### Daemon lifecycle

- Socket path is overridable by `ELEFANTE_SOCKET`; its parent directory is mode `0700`.
- A stale socket file left by a killed daemon is unlinked and rebound, not fatal.
- Two clients spawning concurrently converge on one daemon.
- The daemon does **not** exit while a turn is live with zero clients attached, and does exit
  after `ELEFANTE_IDLE_SECS`.
- `session.close` during a live turn cancels it and releases the session.

### Client output

- Headless: on a turn producing both reasoning and an answer, `stdout` is byte-for-byte the
  answer with no reasoning in it, and `stderr` carries the reasoning. *Requires the stub
  provider; without a deterministic stream this is unassertable.*
- Headless: `stdout` redirected to a file contains no ANSI escape bytes.
- Headless with no key and a non-TTY stdin: exits non-zero with
  `OPENROUTER_API_KEY is not set`, and does not prompt.
- Renderer: a reasoning→text transition emits exactly one newline; ten consecutive
  same-channel deltas emit none.
- Reducer: reasoning and answer text occupy separate fields and are never concatenated.
- **The TUI transcript component renders those two fields as separately identifiable output.**
  *The correction: asserting only the reducer leaves the render site uncovered, and issue #4
  was a render-site defect — a component reading two fields and concatenating them at draw
  time reproduces it while passing every other item here.*
- Two reducers fed one event sequence reach deep-equal state.
- A client started against a socket with no daemon fails **before** the renderer is
  constructed, naming the socket path and the remedy. *This path is the seam between the two
  test surfaces — component tests render with no socket, daemon tests drive the wire with no
  renderer — so nothing crossed it until a user hit it. Connecting first is what makes it
  safe: a failure that has not touched the terminal has no state to restore and no
  capability probes outstanding to leak their replies into the user's shell.*
- A turn that reasons for 5 s while emitting no `text.delta` leaves reducer state with a
  non-null `thinking_since` and a reasoning-token count. *Given an observable, because
  "does not appear frozen" is not assertable.*

### Key setup

- First launch with no key prompts on a TTY, stores at `0600` under `~/.elefante/`, and never
  writes into a working tree.
- An existing `OPENROUTER_API_KEY` wins over the stored value.
- `key.set` rebuilds the provider: a retry after setting a key succeeds without restarting
  the daemon.

### Regression

- **M0 gate still green**, including `crates/cli/tests/cli.rs` under `ELEFANTE_SOCKET`
  isolation and the credentialed test's `--max-tokens` bound reaching the daemon.

---

## Open questions for implementation

Deliberately unsettled; decide and record here when reached.

1. Does the TUI ship a scrollback view of completed turns, or only the live turn plus a
   compact history? Affects snapshot size in long sessions.
2. ~~Bun runtime distribution — assume `bun` on PATH, or vendor it?~~ **Decided 2026-09-06:
   vendor, via `bun build --compile`, but not yet.** Measured: a 69.2 MB self-contained
   binary, `libopentui.dylib` embedded at `/$bunfs/root/`, 80 ms startup against 70 ms
   interpreted — so latency does not bear on the choice. Ships 12.3 MB → 81.5 MB, but the
   user-side footprint *falls* from ~143 MB (12.3 Rust + 59 Bun + 72 `node_modules`), because
   the bundler drops dev dependencies the developer install carries. The packaging work waits
   for a release; what is decided now is the constraint it implies, because that constrains
   M2+ client code: **the TUI must stay compile-able** — no runtime `node_modules` lookups,
   no dynamic imports of paths absent at bundle time. CI enforces it with a compile and smoke
   run on every push, on Linux and macOS. Cross-compiling is deliberately not used: it embeds
   the host's native library unless the target's optional dependency is installed.
3. Tool lifecycle events are dropped entirely by the M0 CLI, so a run gives no sign that
   tools ran. M1 routes them to `stderr` in the headless client and to transcript rows in
   the TUI, so this likely needs no further change — noted so it is checked against the
   implementation rather than assumed.
4. Whether `tool.started` should carry streamed argument fragments. Currently it fires once
   with a complete `input_preview`; streaming arguments would show tool inputs building in
   real time, at the cost of a much noisier event stream.
