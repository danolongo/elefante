# elefante-tui

The interactive terminal client: a Bun + OpenTUI front end that attaches to the daemon over a
Unix socket. **Bun is required and npm is not an alternative** — `@opentui/core` loads its
native library through Bun's FFI, so Node cannot run this client at all.

```sh
bun install          # verifies against bun.lock, the one committed lockfile
bun run src/main.tsx
```

`bun.lock` is authoritative; see [`../docs/LOCKFILE.md`](../docs/LOCKFILE.md) for why there is
exactly one. The Rust CLI starts the daemon before launching this entrypoint — this client
never spawns one.

## Layout

Three layers, deliberately separated so two of them are testable without a terminal.

| File | Lines | Contents |
|---|---:|---|
| `src/protocol.ts` | 238 | Framing, sequence windows, attach and gap handling, `connectOrExplain`. |
| `src/session.ts` | 39 | The pure reducer: `reduceSession`, `restoreSnapshot`, `addUsage`. No I/O, no clock. |
| `src/main.tsx` | 97 | Entrypoint — argument parsing, connect, renderer construction. |
| `src/ui/Transcript.tsx` | 26 | Completed turns and the live turn; reasoning and answer as separate renderables. |
| `src/ui/Modal.tsx` | 25 | Approval prompts and the first-run key prompt. |
| `src/ui/App.tsx` | 24 | Composition and key handling. |
| `src/ui/Status.tsx` | 9 | Thinking indicator, elapsed time, usage. |
| `src/ui/Input.tsx` | 5 | Prompt entry. |

`session.ts` is short but dense — single-line functions, not a stub.

## Entry points

`--session ID` attaches an existing session; `--cwd`, `--model`, `--effort`, `--max-rounds`,
and `--max-tokens` configure a new one. `ELEFANTE_SOCKET` selects an isolated daemon. Model,
effort, and limits also read `ELEFANTE_MODEL`, `ELEFANTE_EFFORT`, `ELEFANTE_MAX_ROUNDS`, and
`ELEFANTE_MAX_TOKENS`, with explicit arguments taking priority.

Enter submits, Ctrl+C interrupts, Ctrl+R collapses reasoning, Ctrl+D detaches. Approval modals
accept Y/N. Completed turns use a scrollable transcript.

## Invariants

**Connect before constructing the renderer.** `createCliRenderer` fires terminal capability
probes on startup; if a connect failed afterwards and the renderer were torn down mid-handshake,
the terminal's replies would arrive after the process exited and land in the user's shell as
typed input. `connectOrExplain` runs first, so a failure has touched nothing and has nothing to
restore.

**The reducer performs no I/O and reads no clock.** Time is passed in by the caller, which is
what makes `reduceSession` deterministic — two reducers fed one event sequence reach deep-equal
state.

**Reasoning and answer occupy separate fields end to end**, and the transcript renders them as
separately identifiable output. Concatenating them at draw time would reproduce issue #4 while
passing every reducer-level test, so the render site is asserted directly.

**The live reasoning-token count is a display heuristic, not accounting.** It is the accumulated
reasoning text's UTF-16 length divided by four, rounded up — zero for empty text, independent of
delta chunk boundaries, and reconstructed from active snapshot reasoning. Usage arrives only on
`turn.finished`, so until then the thinking indicator shows elapsed time and this clearly
labeled estimate. It **never enters completed or cumulative usage**, which use only the exact
provider report.

**API keys are masked and never placed in an OpenTUI input value.**

Reconnect uses the last contiguous cursor. Gaps trigger replay; overlapping merged ranges
trigger an explicit snapshot, because concatenated text cannot be split back into original
sequence entries. Multiple live approvals are retained locally, while a snapshot carries only
the one pending approval the contract provides.

Snapshots do not include completed reasoning, the active prompt, or the elapsed start time, so
those cannot be reconstructed on a fresh attachment. The daemon's retained history is unbounded
— see [`../docs/DEVELOPER_GUIDE.md`](../docs/DEVELOPER_GUIDE.md) §14.

## Testing

```sh
bun test              # 23 tests
bun run typecheck     # tsc --noEmit across all three layers
```

`tests/protocol.test.ts` (10) covers framing at every byte split, sequence windows, and the
no-daemon failure path. `tests/session.test.ts` (7) covers the reducer. `tests/ui.test.tsx` (6)
drives the real OpenTUI test renderer — **without a TTY**, which is what lets the render site be
asserted in CI.

## Boundaries

No protocol logic in components, and no rendering in the reducer. That split is what keeps two
of the three layers testable headlessly; collapsing it would leave the UI assertable only by
screenshot.

`DEVELOPER_GUIDE.md` §5 is the wire contract this client implements. OpenTUI reference:
<https://opentui.com/docs/bindings/react/> and
<https://opentui.com/docs/core-concepts/testing/>.
