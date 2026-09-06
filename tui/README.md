# Elefante TUI

Bun is required on PATH; this client does not vendor a runtime, and npm is not an
alternative — `@opentui/core` loads its native library through Bun's FFI, so Node cannot run
this client at all. Run `bun install`, then `bun run src/main.tsx`. `bun.lock` is the
authoritative lockfile; see `../docs/LOCKFILE.md` for why there is exactly one. The Rust CLI
starts the daemon before launching this entrypoint. `ELEFANTE_SOCKET` selects an isolated
daemon; `--session ID` attaches an existing session.
The entrypoint also accepts `--cwd`, `--model`, `--effort`, `--max-rounds`, and `--max-tokens`.
Model, effort and limits also accept `ELEFANTE_MODEL`, `ELEFANTE_EFFORT`,
`ELEFANTE_MAX_ROUNDS`, and `ELEFANTE_MAX_TOKENS`, with explicit arguments taking priority.

Use Enter to submit, Ctrl+C to interrupt, Ctrl+R to collapse reasoning, Ctrl+D to detach.
Approval modals accept Y/N. API keys are masked and never placed in an OpenTUI input value.
Disconnects reconnect with the last contiguous cursor, with explicit snapshot replacement
when the ring can no longer resume. Gaps trigger replay; overlapping merged ranges trigger
a snapshot because concatenated text cannot be split into original sequence entries.

`bun test` runs protocol, pure reducer, and real OpenTUI test-renderer checks without a TTY.
`npm run typecheck` checks all three layers.

Completed turns use a scrollable transcript. The daemon's retained history remains unbounded
as documented in the frozen contract. Snapshots do not include completed reasoning, active
prompt, or elapsed start time, so those cannot be reconstructed on a fresh attachment.
M1 only transmits usage on `turn.finished`; the thinking indicator displays elapsed time and
a clearly labeled local reasoning-token estimate until then. The estimate is the accumulated
reasoning text's UTF-16 length divided by four, rounded up (zero for empty text), independent
of delta chunk boundaries and reconstructed from active snapshot reasoning. It is a display
heuristic, not provider token accounting. Completed and cumulative usage use only the exact
provider report from `turn.finished`; estimates never enter those totals.
Multiple live approvals are retained locally; a snapshot represents only
the one pending approval provided by the contract.

OpenTUI API reference: https://opentui.com/docs/bindings/react/ and
https://opentui.com/docs/core-concepts/testing/.
