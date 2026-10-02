# elefante-cli

The `elefante` binary. **It is a client, not the agent** — it spawns `elefante-srv` if none is
listening, then talks to it over a Unix socket. It does not link `elefante-core`, and nothing
here runs a turn.

It has two modes, chosen by whether a prompt was given:

- **With a prompt** — a headless one-shot: create a session, stream one turn, close the session.
- **Without a prompt** — launch the interactive TUI (`launch_tui`, `main.rs:266`). The CLI has
  already ensured the daemon is up, so it runs `bun run tui/src/main.tsx` with `--model`,
  `--effort`, `--max-rounds`, `--max-tokens`, and `ELEFANTE_SOCKET` set, and waits for it. The TUI
  entrypoint is found beside the executable, else in the source tree (`CARGO_MANIFEST_DIR`);
  Bun must be on `PATH`. The TUI inherits the working directory, which becomes the session root.

Everything below describes the headless mode unless it says otherwise.

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
(`main.rs:303`). A prompt written to a pipe is a hang, not a prompt. With a TTY it reads the key
from stdin and sends `key.set`.

**Headless mode denies every escalated command.** On `approval.requested` it immediately sends
`session.resolve_approval` with `deny` (`main.rs:382-396`), so a one-shot run can never execute a
command the classifier escalated — it has no way to ask you. Use the TUI when a task needs
approvals.

**The environment that counts is the daemon's, not yours.** `OPENROUTER_API_KEY`,
`OPENROUTER_BASE_URL`, and `ELEFANTE_IDLE_SECS` are read once, when `elefante-srv` starts, and a
running daemon is reused as-is (`srv::run` returns immediately if the socket answers). So a new
key exported after a daemon is already up is ignored until it idles out (15 minutes by default)
or is killed. `--model`, `--effort`, `--max-rounds`, and `--max-tokens` are unaffected — they
travel in `session.create`.

## Testing

```sh
cargo test -p elefante-cli                          # 13 hermetic
cargo test -p elefante-cli -- --ignored --nocapture   # 1 credentialed, billable
```

`tests/renderer.rs` (6) covers channel transitions and newline discipline against an in-memory
writer. `tests/client.rs` (3) covers the socket client. `tests/cli.rs` (5, one of them `#[ignore]`d) drives
the real binary under `ELEFANTE_SOCKET` isolation.

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
