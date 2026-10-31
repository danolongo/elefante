# elefante

The `elefante` binary and the three things it is made of: the **wire** contract, the **daemon**,
and the headless **client**. One executable plays both roles. `elefante daemon` serves the Unix
socket; every other invocation is a client that connects to it, starting one by running its own
executable when none is listening. A turn only ever runs in the daemon — clients come and go
around it.

This crate was `elefante-rpc`, `elefante-srv` and `elefante-cli` until they were merged; the module
boundaries below are the old crate boundaries and keep their rules.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/main.rs` | 565 | The binary: argument and environment parsing, `elefante daemon`, starting a daemon and waiting for it, the socket client and its `auth` handshake, the session lifecycle, `elefante memory`, the TUI launch. |
| `src/lib.rs` | 9 | `pub mod client; pub mod daemon; pub mod wire;` |
| `src/wire/mod.rs` | 14 | Glob re-exports and the two capacity constants. |
| `src/wire/framing.rs` | 124 | NDJSON encode/decode, `FrameDecoder`, preview truncation. |
| `src/wire/notification.rs` | 363 | `Notification`, `NotificationEvent` (hand-written `Deserialize`, `Unknown`), and `Sequence` with its coalescing rule. |
| `src/wire/request.rs` | 372 | `Request`, `Method`, `Response`, the error codes (including `UNAUTHORIZED`), `SandboxStatus` and `SandboxState`, and the memory types: `MemoryStatus`, `MemorySeed`, `MemoryNote`, `MemoryRow`, `MemoryChange`, the consent and learning enums. |
| `src/wire/snapshot.rs` | 85 | `AttachResult` (both kinds carry optional `memory` and `sandbox`), `SessionSnapshot` (with optional `sandbox`), `CompletedTurn` and `ActiveTurn` (with `memory_note_ids`), `PendingApproval` (with `network`), `ToolStatus`. |
| `src/daemon/mod.rs` | 14 | Module declarations, re-exports, three capacity constants. |
| `src/daemon/server.rs` | 2442 | **The largest file in the workspace.** Listener and token, `auth`, session registry, ring buffer, method dispatch, turn driving, memory and consent, sandbox status and the git identity. |
| `src/daemon/search.rs` | 242 | `ProviderSearch`, the `web_search` backend, and `search_model`. |
| `src/daemon/queue.rs` | 300 | `ClientQueue` — per-client delivery with coalescing and eviction. |
| `src/daemon/stub.rs` | 264 | `StubProvider`, the scripted fake the tests use. **Compiled into debug builds only.** |
| `src/daemon/credentials.rs` | 105 | `0700` directories and `0600` files, written by rename, for the socket, the token and the stored key. |
| `src/client/mod.rs` | 228 | `StreamRenderer`, one per output stream, and `sandbox_warning`. |
| `src/client/memory.rs` | 218 | `elefante memory`: argument parsing (`parse`), the consent request, and rendering daemon results. |

`server.rs` is the obvious first split: the listener, the registry and the dispatch table are three
separable concerns sharing one file.

## Entry points

```sh
elefante "prompt"           # headless one-shot: create a session, stream one turn, close it
elefante                    # launch the TUI (Bun) against the daemon
elefante memory <command>   # list | forget | approve | clear | learn, always through the daemon
elefante daemon             # serve the socket in the foreground until the idle timeout
```

```rust
use elefante::{client::StreamRenderer, daemon, wire};

let bytes = wire::encode_frame(&request)?;          // one JSON value + '\n'
daemon::run_from_env().await?;                       // reads ELEFANTE_SOCKET, ELEFANTE_IDLE_SECS, …
daemon::run(daemon::DaemonConfig { /* ... */ }).await?;
let mut out = StreamRenderer::new(io::stdout(), styled);
```

Without a prompt the client launches the TUI: `bun run tui/src/main.tsx` with `--model`, `--effort`,
`--max-rounds`, `--max-tokens` and `ELEFANTE_SOCKET` set. The entry point is found beside the
executable, else in the source tree (`CARGO_MANIFEST_DIR`); Bun must be on `PATH`. The TUI inherits
the working directory, which becomes the session root.

### `elefante memory`

```
elefante memory list [--all]   what was learned about this repository (--all adds superseded)
elefante memory forget <id>    delete one learned note
elefante memory approve <id>   accept a note only the assistant had vouched for
elefante memory clear --yes    delete every learned note for this repository
elefante memory learn on|off [--model SLUG]   allow or decline learning, for the extractor model it will use
```

Every subcommand goes through the daemon — `memory.list`, `forget`, `approve`, `clear` keyed by the
current directory, and `memory.consent` — starting one if none is listening, so the daemon stays the
only process that writes a store or the consent file and a change takes effect on its next
retrieval. Arguments are parsed first (`client::memory::parse`), so a malformed command is refused
before any daemon is started. Consent is kept per extractor model, so `learn` names the model it is
for (`--model`, else `ELEFANTE_EXTRACT_MODEL`, else `OPENROUTER_MODEL`, else the default) and prints
it. Every column is passed through `display_safe`, by the daemon and again before printing; rows
that fail their integrity check are listed, flagged, so they can be forgotten. A store this
installation did not stamp is refused and left untouched: the command prints `memory is unavailable
here: …` and exits 1. `clear` without `--yes` explains itself and exits 2; an unknown id prints `no
learned note <id>` (`no pending note` for `approve`) and exits 1. A daemon from an older build that
does not know a method is named as such, with how to stop it — nothing is worked around.

## Invariants

### Wire (`wire/`)

**The wire re-exports `StopReason` and `Usage` from `elefante-provider`** so the wire and the agent
loop cannot drift apart: if `StopReason` gains a variant, the wire gains it in the same commit,
because it is the same type.

**A `Notification` carries top-level `session_id`, `seq`, and `turn_id`, which strict JSON-RPC 2.0
does not permit.** The layout is JSON-RPC-*shaped*, not conformant, so an off-the-shelf library
will not round-trip it. Both sides hand-roll the envelope deliberately.

**`Sequence` is `Single` or `Coalesced { seq_from, seq_to }`, and `coalesce` only merges same-method
deltas.** The range is what makes a merge distinguishable from a gap.

**`NotificationEvent` accepts a method it does not know.** `Deserialize` is written by hand because
`#[serde(other)]` does not work on this adjacently-tagged enum for events with `params`: it reads the
`method`, deserialises the known ones, and returns `Unknown { method }` for the rest, provided
`params` is an object. `Unknown` is never serialised. The memory events are known events and, like
every non-delta, are never coalesced.

**The memory fields are additive and optional.** `CreateResult.memory` and both `AttachResult`
variants' `memory` are `Option<MemoryStatus>` skipped when absent; `memory_note_ids` is skipped when
empty. Nothing denies unknown fields, so an older peer ignores them.

A change in `wire/` is a wire change: the TypeScript client (`tui/src/protocol.ts`) speaks the same
format and shares no code, so it lands on both sides at once or it lands broken. `DEVELOPER_GUIDE.md`
§5 is the single source of truth for the protocol.

### Daemon (`daemon/`)

**The ring and the client queue are different things, and conflating them breaks resume.** The ring
holds 4,096 events *as emitted*, each with its own `seq`, never coalesced — `since: 412` must
address a real event. The client queue is a disposable delivery copy, 256 events, and the only
place merging happens.

**A client that stops reading gets its connection closed, with no `session.error` first.** The
close *is* the signal; the session is unaffected and the client reconnects and attaches.

**`StubProvider` validates `tool_use`/`tool_result` pairing and can record every request it
receives.** That is what makes the pairing assertions in `tests/daemon.rs` meaningful. It exists
only in debug builds: a release daemon never reads `ELEFANTE_STUB_PROVIDER`, so it cannot be
switched to a scripted provider by its environment. Run the tests in the debug profile.

**Memory is an aid, never a requirement, and one switch turns it all off.** The first session
beneath a repository root opens that repository's `Store` and starts its `Indexer`; later sessions
share them. If the store cannot be opened, or the directory is `/` or `$HOME`, the session is
created anyway and the reason goes to stderr. `DaemonConfig::memory` (`ELEFANTE_MEMORY=0`) is
checked first: nothing is opened, nothing is written under `.elefante/`, no context is added and
`memory_search` returns its fixed refusal.

**Learning and consent.** After each turn the daemon hands the session's new text to its `Learner`
(`elefante-core`); what is pending is extracted on close, after `ELEFANTE_LEARN_IDLE_SECS` of quiet,
and as the daemon exits. `DaemonConfig::learn` is a `LearnMode`: `ELEFANTE_LEARN` unset is `Ask`,
`1`/`on` is `On`, `0`/`off` and `ELEFANTE_MEMORY=0` are `Off`. In `Ask` one shared `ConsentGate`
reads the stamped `~/.elefante/memory-consent`, and a record for a different extractor reads as
unset. A turn reaches the learner only if consent was granted when it began and the gate's revision
is unchanged when it ends; a revoking `memory.consent` drops what the learner was holding.

**The memory wire.** `create` and `attach` return a `MemoryStatus`. Learner callbacks become
`memory.learned` and `memory.status` session events (`turn_id: null`), and injected notes become
`memory.context` with `memory_note_ids` on the turn. `memory.*` by `cwd` open the repository's store
on demand, never create one, and answer `MEMORY_OFF` or `UNKNOWN_NOTE`. Every note string leaving
the daemon passes `learn::display_safe`.

**Refreshes are requested at session create, turn start and turn end, and never awaited.**

**A turn continues with zero clients attached.** The daemon exits after the idle period only when
there are no clients *and* no active turns.

**The socket's parent directory is created `0700` before the socket is bound.** `bind` applies the
umask and there is no safe API to bind with an explicit mode; `credentials.rs` also rejects a
symlinked directory rather than following it.

**Every connection authenticates first.** At start the daemon draws a 128-bit token. The starter
that holds the socket directory's lock writes it to `token` beside the socket (`0600`, by rename),
then binds. The first request on a connection must be `auth { token }`. `same_token` compares
every byte, and a mismatch gets `UNAUTHORIZED` (`-32008`) before the daemon closes the connection.

**The daemon settles the sandbox state before any client connects.** At start the daemon removes stale
sandbox temp directories, calls `elefante_core::sandbox::probe` once and prints an `Unavailable`
reason to stderr. It also reads the user's git identity once. Each `session.create` builds the
session's `ToolExecutor`, attaches the identity (without one the gate denies harness git writes)
and attaches `ProviderSearch`. The create and attach results and the snapshot report the state as
`sandbox: { state, detail }`.

**`web_search` runs on the provider.** `ProviderSearch` sends one request per search with
`Request::web_search` set and no tools, on `ELEFANTE_SEARCH_MODEL` or else the session's model. It
reads the provider at each search, so a key set after the session started still works. Its usage,
including a failed search's, joins the turn's usage at `turn.finished`.

### Client (`client/`, `main.rs`)

**The client never runs a turn.** It talks to the daemon over the socket, which is what makes
"reconnect and the turn completes intact" true. Keep turn logic out of `main.rs`.

**The answer goes to stdout; reasoning, tool activity and errors to stderr**, each with its own
`StreamRenderer`, so neither can interleave into the other. This resolved issue #4 by construction:
a redirected `stdout` holds the answer and nothing else, with no ANSI bytes. Styling is decided per
stream by `is_terminal()`.

**Starting a daemon** runs `current_exe() daemon`, waits up to 2 seconds for the socket, and
captures the daemon's stderr on a reader thread, so a daemon that dies during startup reports *why*
rather than timing out.

**Every connection sends `auth` first.** The client reads the token beside the socket. It retries
for up to 2 seconds while a starting daemon writes the file, and sends an empty token when none
appears.
A daemon that answers `-32601` predates the token; the client names it as an older daemon, with how
to stop it. After `session.create` the client prints one warning line to stderr when `sandbox` is
`off` or `unavailable`.

**With no key and a non-TTY stdin the client exits non-zero and does not prompt.** A prompt written
to a pipe is a hang. With a TTY it reads the key and sends `key.set`.

**Headless learning setup is a notice, never a prompt**: with consent unset, a one-shot prints one
instruction to terminal stderr before the turn. With stderr a terminal, `memory: saved N note(s)` is
printed at the next `turn.finished`, and each distinct `problem` once. Never to stdout, never note
text.

**Headless mode denies every escalated command** by answering `approval.requested` with `deny` at
once: a one-shot cannot ask you. Use the TUI when a task needs approvals.

**The environment that counts is the daemon's.** The daemon reads `OPENROUTER_API_KEY`,
`OPENROUTER_BASE_URL`, `ELEFANTE_IDLE_SECS`, `ELEFANTE_MEMORY`, `ELEFANTE_LEARN` and
`ELEFANTE_SANDBOX` once, when it starts, from the environment of whichever client started it. A
client reuses a running daemon as-is. It reads `ELEFANTE_SEARCH_MODEL` at each `session.create`.
`--model`, `--effort`, `--max-rounds` and `--max-tokens` travel in `session.create`.

## Testing

```sh
cargo test -p elefante                            # hermetic, no network
cargo test -p elefante -- --ignored --nocapture   # 3 credentialed, billable
```

| File | Tests | Covers |
|---|---:|---|
| `tests/wire.rs` | 10 | Every request, response, notification and snapshot shape, the memory methods and events, and that an unknown method parses but cannot be serialised. |
| `tests/framing.rs` | 3 | Frames split across chunks, oversized lines, escaped newlines inside strings. |
| `tests/daemon.rs` | 17 | The real daemon over a real socket: resume, fanout, interrupt, approval, lifecycle, the token handshake and a client that arrives during startup. |
| `tests/daemon_memory.rs` | 8 | The repository index through the daemon: what a model is sent, `memory_search`, a subdirectory's scope, edits between turns, memory off writes nothing. |
| `tests/learn.rs` | 27 | Learning end to end through a scripted extractor; consent (nothing read before an answer, persistence, extractor binding, tampering, operator modes, revocation); the memory status and events; `memory.*` by `cwd`. |
| `tests/cli_memory.rs` | 13 | `elefante memory` through the real binary and a daemon it starts: listing, approve, forget, clear, tampered and hostile rows, an unstamped store, argument errors before any daemon, consent per extractor model. |
| `tests/client.rs` | 11 | The socket client against a scripted peer, a daemon that fails to start, `elefante daemon` in the foreground, a daemon from an older build or without the token, a token the daemon is still writing, and a refused token. |
| `tests/renderer.rs` | 13 | Channel transitions, newline discipline and the `memory:` lines. |
| `tests/cli.rs` | 7 | Startup failures named, not transport noise; 3 are credentialed and `#[ignore]`d. |
| `tests/sandbox.rs` | 1 | End to end through the real binary with a fake `HOME`: a scripted tool call cannot read an SSH key fixture through the file tools or the sandboxed shell. |

The credentialed tests run in a `tempfile` directory so a model that misreads its instruction cannot
touch tracked files, and bound themselves with `--max-tokens 2048 --effort low`:
`live_edit_lands_in_a_scratch_repository`, `live_memory_context_reaches_the_model` and
`live_learned_memory_is_extracted_stored_and_used`. Because the client starts its own executable as
the daemon, `cargo test -p elefante` needs no other binary built first.

Test daemons use `ELEFANTE_SOCKET` inside a fresh `run/` directory (the daemon insists on `0700`)
and `ELEFANTE_IDLE_SECS=1`, so none outlives its test.

## Boundaries

This crate composes; it does not implement conversation semantics. Turn logic belongs in `core`,
policy in `guard`, transport in `provider`. It is the top of the dependency graph and the only crate
that knows a socket exists.
