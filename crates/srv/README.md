# elefante-srv

The daemon. Owns the Unix socket, the session registry, durable per-session state, and bounded
delivery to every attached client. This is where a turn actually lives — clients come and go
around it.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/main.rs` | 4 | The binary. Calls `run_from_env()` and nothing else. |
| `src/lib.rs` | 12 | Module declarations, four re-exports, three capacity constants. |
| `src/server.rs` | 1112 | **The largest file in the workspace.** Listener, session registry, ring buffer, method dispatch, turn driving. |
| `src/queue.rs` | 300 | `ClientQueue` — per-client delivery with coalescing and eviction. |
| `src/stub.rs` | 198 | `StubProvider`, the scripted fake used by daemon tests. |
| `src/credentials.rs` | 95 | `0700` directories and `0600` files for the socket and stored key. |

`server.rs` is a fair first target if you are looking for something to split — the listener,
the registry, and the dispatch table are three separable concerns sharing one file.

## Entry points

```rust
elefante_srv::run_from_env().await?;              // reads ELEFANTE_SOCKET, ELEFANTE_IDLE_SECS
// or
elefante_srv::run(DaemonConfig { /* ... */ }).await?;

elefante_srv::socket_path()?;                      // $XDG_RUNTIME_DIR, else ~/.elefante/run/
```

`DaemonConfig::from_env()` builds the config the same way `run_from_env` does, which is what
tests use to point a daemon at an isolated socket.

`queue`, `stub`, and `credentials` are private modules — nothing from them is re-exported, and
`StubProvider` is `pub(crate)` on purpose.

## Invariants

**The ring and the client queue are different things, and conflating them breaks resume.** The
ring holds 4,096 events *as emitted*, each with its own `seq`, and is never coalesced — `since:
412` must address a real event. The client queue is a disposable delivery copy, 256 events, and
is the only place merging happens. `queue.rs` states this in its first line for exactly this
reason.

**A client that stops reading gets its connection closed, with no `session.error` first.** A
client whose queue is overflowing is by definition one that cannot receive an error; the close
*is* the signal. The session is unaffected and the client reconnects and attaches.

**`StubProvider` validates `tool_use`/`tool_result` pairing and rejects a mismatch, and can record
every request it receives.** This is
what makes the pairing assertions in `tests/daemon.rs` meaningful — against a stub that ignored
its input, "the next request does not 400" would pass vacuously on an implementation that drops
history. If you extend the stub, keep `validate_pairing` in the path.

**Memory is an aid, never a requirement, and one switch turns it all off.** A session's working
directory resolves to a repository root (`RepoId`); the first session beneath a root opens that
repository's `Store` and starts its `Indexer`, and every later session shares them. If the store
cannot be opened, or the directory is `/` or `$HOME`, the session is created anyway and runs exactly
as it did before — the reason goes to stderr. `DaemonConfig::memory` (`ELEFANTE_MEMORY=0`) is
checked before anything else: nothing is opened, nothing is written under `.elefante/`, no context
is added and `memory_search` returns its fixed refusal. It is a config field, not a process-wide
env read, so tests can run daemons with and without memory in one process.

**Refreshes are requested at three moments and never awaited.** Session create, turn start and
turn end each call `Indexer::refresh()`, which returns immediately and coalesces. Turn start picks
up edits made in an editor between turns; turn end picks up the agent's own. A prompt is never held
behind an index.

**A turn continues with zero clients attached.** The daemon exits after an idle period only when
there are no clients *and* no active turns; checking client count alone would make it kill its
own work.

**The socket's parent directory is created `0700` before the socket is bound.**
`UnixListener::bind` applies the umask and there is no safe API to bind with an explicit mode —
`libc::umask` is `unsafe`, which the workspace forbids, and chmod-after-bind leaves a window.
`credentials.rs` also rejects a symlinked directory rather than following it.

`DEVELOPER_GUIDE.md` §5 is the protocol contract this crate implements.

## Testing

```sh
cargo test -p elefante-srv     # 30 tests
```

`tests/daemon.rs` (15) drives the real daemon over a real socket with `ELEFANTE_SOCKET` pointed
at a `tempfile` path — resume, fanout, interrupt, approval, and lifecycle. `tests/memory.rs` (8)
does the same for the repository index: what a model is sent, what `memory_search` returns, a
subdirectory session's scope, edits between turns, and that switching memory off writes nothing.
It reads the requests the model would have received from a file the stub records them to
(`record_requests` in the script). Inline tests cover
`queue.rs` (4), plus one each in `credentials.rs`, `server.rs`, and `stub.rs`.

Nothing here needs credentials: `ELEFANTE_STUB_PROVIDER` swaps in `StubProvider`, which is why
the whole daemon is testable without network or spend.

## Boundaries

The daemon composes; it does not implement conversation semantics. Turn logic belongs in `core`,
wire types in `rpc`, policy in `guard`. This crate is the only one that knows a socket exists.

It is also the top of the dependency graph — `core`, `rpc`, `provider`, and `tools` all flow
into here, and nothing flows back out.
