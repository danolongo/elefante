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
| `src/main.rs` | 515 | Argument and environment parsing, daemon discovery and spawn, socket client, session lifecycle. |
| `src/lib.rs` | 177 | `StreamRenderer`, and the `memory` module. |
| `src/memory.rs` | 335 | `elefante memory` administration — direct mode, daemon requests and their rendering, and learned-memory consent; see below. |

## Entry points

The binary has no public API. The library exposes one type:

```rust
use elefante_cli::StreamRenderer;

let mut out = StreamRenderer::new(io::stdout(), styled);
out.render(&notification)?;
out.finish()?;
```

### `elefante memory`

```
elefante memory list [--all]   what was learned about this repository (--all adds superseded)
elefante memory forget <id>    delete one learned note
elefante memory approve <id>   accept a note only the assistant had vouched for
elefante memory clear --yes    delete every learned note for this repository
elefante memory learn on|off   allow or decline learning from conversations
```

Every subcommand prefers an already-listening daemon (`memory.list`, `forget`, `approve`, `clear`,
`consent`, keyed by the current directory), so a change takes effect on the next retrieval and a
consent answer reaches live sessions at once. None starts a daemon; with none listening they open
the repository's store directly (`list`, `forget`, `approve`, `clear`) or write the tamper-evident
consent file (`learn`). Learned notes are free-form text that is injected into prompts, so a person
can always see them and remove them: the direct `list` also shows rows that fail their integrity
check, flagged, so they can be forgotten, and checks the store's stamp first (a foreign store prints
one explanation and no rows). Every column is passed through `display_safe` before printing.
`clear` refuses without `--yes`; an unknown id prints `no learned note <id>` (`no pending note` for
`approve`) and exits 1 either way.

**Two renderers, one per stream** (`main.rs:337-338`): the answer goes to `stdout`, reasoning
and tool activity to `stderr`. Each owns its own channel state, so neither can interleave into
the other's output.

**Memory lines.** The stderr renderer, built with `with_memory_notices(io::stderr().is_terminal())`,
counts `memory.learned` events that are not `pending` and prints `memory: saved N note(s)` at the
next `turn.finished`, and prints each distinct `memory.status` `problem` once as `memory: <problem>`.
Never to stdout and never note text. In a one-shot the session usually closes before extraction
finishes, so in practice only a problem line appears; the count appears when a later turn on the same
session finishes.

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

**Headless learning setup is a notice, never a prompt.** If the daemon reports consent as unset, a
one-shot invocation prints one instruction to terminal stderr before the turn
(`memory: learning is paused (not set up) — run “elefante memory learn on” or set ELEFANTE_LEARN=1`). Redirected stderr and
older daemons that omit the additive memory result remain silent; stdout is always only the answer.

**Headless mode denies every escalated command.** On `approval.requested` it immediately sends
`session.resolve_approval` with `deny` (`main.rs:382-396`), so a one-shot run can never execute a
command the classifier escalated — it has no way to ask you. Use the TUI when a task needs
approvals.

**The environment that counts is the daemon's, not yours.** `OPENROUTER_API_KEY`,
`OPENROUTER_BASE_URL`, `ELEFANTE_IDLE_SECS`, and `ELEFANTE_MEMORY` are read once, when `elefante-srv` starts, and a
running daemon is reused as-is (`srv::run` returns immediately if the socket answers). So a new
key exported after a daemon is already up is ignored until it idles out (15 minutes by default)
or is killed. `--model`, `--effort`, `--max-rounds`, and `--max-tokens` are unaffected — they
travel in `session.create`.

## Testing

```sh
cargo test -p elefante-cli                          # hermetic
cargo test -p elefante-cli -- --ignored --nocapture   # 2 credentialed, billable
```

`tests/renderer.rs` (13) covers channel transitions, newline discipline and the `memory:` lines against
an in-memory writer. `tests/memory.rs` (12) covers `elefante memory` directly. `tests/client.rs` (5) covers the
socket client, including `memory` commands preferring a listening daemon. `tests/cli.rs` (7, two of them `#[ignore]`d) drives
the real binary under `ELEFANTE_SOCKET` isolation.

Two tests are `#[ignore]`d; both need credentials and make billable requests. They run in a
`tempfile` directory rather than this repository, so a model that misreads its instruction cannot
touch tracked files, and they bound themselves with `--max-tokens 2048 --effort low`.

- `live_edit_lands_in_a_scratch_repository` — a real edit lands (`--max-rounds 8`).
- `live_memory_context_reaches_the_model` — the repository index reaches a real model. The scratch
  repository is indexed *before* the daemon starts so the first prompt cannot race the background
  indexer, and `--max-rounds 1` makes the answer depend on context alone: a model that reaches for a
  tool exhausts its single round and fails. It is the only test of whether a provider *accepts* the
  injected message shape; the hermetic tests prove the request is built correctly, not accepted.

**Both require `elefante-srv` to have been built** — `cargo test -p elefante-cli` alone does not
build it, which is why CI runs `cargo build --workspace` first. This crate's tests gained a
dev-dependency on `elefante-mem` only to pre-index that repository.

## Boundaries

This crate depends only on `elefante-rpc`. Keep it that way: the moment it links `core`, the
daemon stops being the only place a turn runs, and "reconnect and the turn completes intact"
stops being true.

Composition and rendering. Nothing else.
