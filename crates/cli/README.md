# elefante-cli

The `elefante` binary. **It is a client, not the agent** — it spawns `elefante-srv` if none is
listening, then streams a session over a Unix socket. It does not link `elefante-core`, and
nothing here runs a turn.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/main.rs` | 435 | Argument and environment parsing, daemon discovery and spawn, socket client, session lifecycle. |
| `src/lib.rs` | 125 | `StreamRenderer` — the only public type in the crate. |

## Entry points

The binary has no public API. The library exposes one type:

```rust
use elefante_cli::StreamRenderer;

let mut out = StreamRenderer::new(io::stdout(), styled);
out.render(&notification)?;
out.finish()?;
```

**Two renderers, one per stream** (`main.rs:337-338`): the answer goes to `stdout`, reasoning
and tool activity to `stderr`. Each owns its own channel state, so neither can interleave into
the other's output.

## Invariants

**The stdout/stderr split is what resolves issue #4, and it resolves it by construction.** The
M0 CLI rendered reasoning and answer text through one match arm, so they printed as a single
undelimited run. Separating the streams means no formatting change can reintroduce the defect —
`stdout` redirected to a file contains the answer and nothing else, with no ANSI bytes.

**Styling is decided per stream by `is_terminal()`.** Redirecting one stream does not strip
color from the other, and neither emits escapes into a pipe.

**Daemon discovery is: sibling of the running executable, then the target profile directory,
then `PATH`** (`main.rs:103-116`). The sibling case is what makes a `cargo build --workspace`
tree work; the profile fallback is what makes `cargo run` work. A missing binary is a named
error — `failed to spawn elefante-srv; install it beside elefante or on PATH` — because the
bare OS error says nothing about the fact that two binaries are involved.

**Spawn waits for readiness, bounded at 2 seconds**, and captures the daemon's stderr on a
reader thread so a daemon that dies during startup reports *why* rather than timing out.

**With no key and a non-TTY stdin the client exits non-zero and does not prompt**
(`main.rs:303`). A prompt written to a pipe is a hang, not a prompt.

## Testing

```sh
cargo test -p elefante-cli                          # 13 hermetic
cargo test -p elefante-cli -- --ignored --nocapture   # 1 credentialed, billable
```

`tests/renderer.rs` (6) covers channel transitions and newline discipline against an in-memory
writer. `tests/client.rs` (3) covers the socket client. `tests/cli.rs` (5) drives the real binary
under `ELEFANTE_SOCKET` isolation.

One test is `#[ignore]`d — `live_edit_lands_in_a_scratch_repository` needs credentials and makes
billable requests. It runs in a `tempfile` directory rather than this repository, so a model
that misreads its instruction cannot touch tracked files, and it bounds itself to
`--max-tokens 2048 --effort low --max-rounds 8`. **It requires `elefante-srv` to have been
built** — `cargo test -p elefante-cli` alone does not build it, which is why CI runs
`cargo build --workspace` first.

## Boundaries

This crate depends only on `elefante-rpc`. Keep it that way: the moment it links `core`, the
daemon stops being the only place a turn runs, and "reconnect and the turn completes intact"
stops being true.

Composition and rendering. Nothing else.
