# elefante-tui

The interactive terminal client: a Bun + OpenTUI front end that attaches to the daemon over a
Unix socket. **Bun is required and npm is not an alternative** — `@opentui/core` loads its
native library through Bun's FFI, so Node cannot run this client at all.

```sh
bun install          # verifies against bun.lock, the one committed lockfile
bun run src/main.tsx
```

`bun.lock` is authoritative; see [`../docs/LOCKFILE.md`](../docs/LOCKFILE.md) for why there is
exactly one. `elefante` with no prompt is the normal way in: the Rust CLI starts the daemon if none is
listening, then runs this entrypoint under `bun` with the resolved model, effort, and limits.
Run directly, this client never spawns a daemon and exits with a named error if none is
listening.

## Layout

Three layers, deliberately separated so two of them are testable without a terminal. `store.ts`
and `Root.tsx` are the seam between the imperative entrypoint and the React tree.

| File | Lines | Contents |
|---|---:|---|
| `src/protocol.ts` | 238 | Framing, sequence windows, attach and gap handling, `connectOrExplain`. |
| `src/session.ts` | 51 | The pure reducer: `reduceSession`, `restoreSnapshot`, `addUsage`. No I/O, no clock. Keeps each turn's output as ordered `segments` (reasoning, text, tool calls). |
| `src/store.ts` | 28 | The external store `main.tsx` publishes into and `Root` subscribes to. |
| `src/main.tsx` | 102 | Entrypoint — argument parsing, connect, renderer construction, reconnect loop. |
| `src/ui/Root.tsx` | 10 | Mounted once; reads the store so listeners register once, not once per update. |
| `src/ui/Transcript.tsx` | 48 | Completed turns and the live turn; a shaded prompt, then reasoning (indented, dim, hidden until Ctrl+R), markdown text and tool calls drawn in the order they happened (a `read` is one line, `✓ read path (N ms)`, never the file), as separate renderables with no text labels. |
| `src/ui/Modal.tsx` | 25 | Approval prompts and the first-run key prompt. |
| `src/ui/App.tsx` | 44 | Composition, key handling and copy-on-select; the prompt bar shares a row with the perched elephant. |
| `src/ui/markdown.ts` | 17 | The `SyntaxStyle` the answer's markdown is drawn with. |
| `src/ui/copy.ts` | 22 | Where selected text goes: OSC52, plus `pbcopy` on macOS. |
| `src/ui/Mascot.tsx` | 57 | `useMascot` decides which stage the elephant is on; `TopMascot` (with the wordmark) and `PerchMascot` place it. |
| `src/ui/elephant/Elephant.tsx` | 113 | `TopElephant` and `PerchElephant`: a frame-buffer renderable played by an OpenTUI timeline. |
| `src/ui/elephant/scenes.ts` | 61 | Pure frame clips (intro, perch entrance, idle flap) and the 30–90 s flap pause. |
| `src/ui/elephant/sprites.ts` | 48 | The pixel art and its palette, ported from the design project. |
| `src/ui/elephant/wordmark.ts`, `Logo.tsx` | 16, 19 | The "elefante" wordmark as pixel rows, and the component that paints it once. |
| `src/ui/elephant/paint.ts` | 47 | Composes a frame into pixels and paints it as half-block cells. |
| `src/ui/elephant/phase.ts` | 21 | The `intro → perch` transition, as pure functions. |
| `src/ui/Status.tsx` | 10 | Thinking indicator, elapsed time, usage. |
| `src/ui/Input.tsx` | 6 | Prompt entry. |
| `src/ui/theme.ts` | 6 | The blue behind the prompt border and its two neighbouring tints. |

`session.ts` is short but dense — single-line functions, not a stub.

## Entry points

`--session ID` attaches an existing session; `--cwd`, `--model`, `--effort`, `--max-rounds`,
and `--max-tokens` configure a new one. `ELEFANTE_SOCKET` selects an isolated daemon. Model,
effort, and limits also read `ELEFANTE_MODEL`, `ELEFANTE_EFFORT`, `ELEFANTE_MAX_ROUNDS`, and
`ELEFANTE_MAX_TOKENS`, with explicit arguments taking priority.

Enter submits, **Esc interrupts** the running turn, **Ctrl+C exits**, and **Ctrl+R** shows or hides
the model's reasoning (hidden at start; the "Thinking" line stays either way). Approval
modals accept Y/N, and Esc there means *deny* — it does not also interrupt the turn.

The answer is rendered as markdown (headings, lists, bold, italic, inline and fenced code).
**Copying:** the terminal's own selection is blocked while the mouse is captured, so drag to select
inside the TUI and the text is copied when you let go, through OSC52 and, on macOS, `pbcopy`.
OSC52 works in Ghostty and iTerm2 (turn on "applications in terminal may access clipboard") and in
tmux with `set-clipboard on`; macOS Terminal.app relies on the `pbcopy` path.

"Exit" is a detach, not a shutdown: it closes this client's socket and leaves the session, its
running turn, and the daemon untouched (`exit()` in `main.tsx` never sends `session.close`). A
turn still in flight keeps running; Esc is the only key that stops it. Completed turns use a
scrollable transcript.

The status line above the prompt reads `MODEL EFFORT | N in / N out / N reasoning | cost: …`. The
three counts and the cost are **session totals**, the sum of every completed turn's `usage`, so
they stay at `0 in / 0 out / 0 reasoning | cost: ?` until the first `turn.finished` arrives —
the daemon reports usage per turn, not per token. `cost: ?` means the provider has reported no
cost yet (`null`); once any turn reports one it reads `cost: $0.0123`, and a reported zero reads
`cost: $0.0000`, which is distinct from unknown.

## The elephant

A pixel-art elephant (from the "8-bit Elephant TUI Animations" design) keeps the transcript company.
A new session opens with it walking in from the left edge of the terminal at the top, raising its trunk, flapping its ears and standing, with the "elefante" wordmark three rows below it
(left out under 61 columns or 20 rows);
the first prompt takes it off the stage at once, and the same elephant walks in at the
bottom-right corner, beside the prompt bar (which narrows to leave it room), where it stands facing the chat. It flaps its ears once after each random
30–90 second pause and is otherwise still. `--session ID` skips the intro and starts it on the perch, and
terminals under 60 columns or 18 rows show no elephant at all.

It is drawn with half-block cells (`▀`/`▄`, foreground = top pixel, background = bottom pixel), so one
sprite pixel is one column by half a row, into a `FrameBufferRenderable` with alpha blending so the
transcript shows through around it. Clips are plain arrays of `{ x, pose }` frames played by an OpenTUI
`Timeline` that tweens a tick counter at 80 ms a frame and paints straight into the buffer — no React
render per frame. The engine holds the renderer in live mode only while a clip is playing, so a standing
elephant costs nothing. Colours are the design's greys against a dark terminal; on a light one the body is
low contrast.

While it walks off the top stage it can pass over the first turn's text, which is drawn beneath it.

A lone Esc is held by OpenTUI's stdin parser for 20 ms before it is reported, since the byte is
also the start of every escape sequence. Tests that press Esc must wait past that window.

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
those cannot be reconstructed on a fresh attachment. They also carry text and tool calls as two
flat lists, so a turn restored from one is drawn text first and its tool calls after; only turns
streamed live keep the interleaved order. The daemon's retained history is unbounded
— see [`../docs/DEVELOPER_GUIDE.md`](../docs/DEVELOPER_GUIDE.md) §14.

## Testing

```sh
bun test              # 58 tests
bun run typecheck     # tsc --noEmit across all three layers
```

`tests/protocol.test.ts` (10) covers framing at every byte split, sequence windows, and the
no-daemon failure path. `tests/session.test.ts` (8) covers the reducer. `tests/ui.test.tsx` (16)
drives the real OpenTUI test renderer — **without a TTY**, which is what lets the render site be
asserted in CI. `tests/elephant.test.ts` (11) covers the sprites, wordmark, clips, flap pause and phase transitions
as pure functions; `tests/copy.test.ts` (2) covers where copied text goes; `tests/elephant.ui.test.tsx` (11) renders the app and steps the animation by calling
`engine.update(ms)`, so no test waits on a real clock. `App`'s `animateMascot={false}` makes the elephant
paint its resting pose.

## Boundaries

No protocol logic in components, and no rendering in the reducer. That split is what keeps two
of the three layers testable headlessly; collapsing it would leave the UI assertable only by
screenshot.

`DEVELOPER_GUIDE.md` §5 is the wire contract this client implements. OpenTUI reference:
<https://opentui.com/docs/bindings/react/> and
<https://opentui.com/docs/core-concepts/testing/>.
