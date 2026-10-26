# elefante-tui

The interactive terminal client: a Bun + OpenTUI front end that attaches to the daemon over a
Unix socket. **Bun is required and npm is not an alternative** — `@opentui/core` loads its
native library through Bun's FFI, so Node cannot run this client at all.

```sh
bun install          # verifies against bun.lock, the one committed lockfile
bun run src/main.tsx
```

**One lockfile.** `bun.lock` is the only committed lockfile. Do not add `package-lock.json`, and
do not gitignore `bun.lock`. While `bun.lock` exists, Bun never reads `package-lock.json`. With both
files present, `bun install --frozen-lockfile` and `npm ci` each succeed and install different
trees, with no warning (measured on Bun 1.4.2).

**Packaging.** `bun build --compile` produces one self-contained binary with the native library
embedded. CI compiles it on every pull request and on pushes to `main`, so the option stays open.
Shipping that binary waits for a release.

`elefante` with no prompt is the normal way in: the Rust CLI starts the daemon if none is
listening, then runs this entrypoint under `bun` with the resolved model, effort, and limits.
Run directly, this client never spawns a daemon and exits with a named error if none is
listening.

## Layout

Three layers, deliberately separated so two of them are testable without a terminal. `store.ts`
and `Root.tsx` are the seam between the imperative entrypoint and the React tree.

| File | Lines | Contents |
|---|---:|---|
| `src/protocol.ts` | 356 | Framing, sequence windows, attach and gap handling, `connectOrExplain`; reads the daemon's `token` beside the socket and sends `auth` first, mapping an older daemon's `-32601` to `OlderDaemonError`; accepts unknown notification methods; parses the optional `memory` and `sandbox` objects, the approval `network` flag, and `memory.list` and `memory.consent` results. |
| `src/session.ts` | 59 | The pure reducer: `reduceSession`, `restoreSnapshot`, `addUsage`. No I/O, no clock. Keeps each turn's output as ordered `segments` (reasoning, text, tool calls). |
| `src/store.ts` | 28 | The external store `main.tsx` publishes into and `Root` subscribes to. |
| `src/commands.ts` | 11 | Pure: `parseCommand` — only exactly `/memory` and `/forget <integer>` (spaces or tabs between) are commands; `//x` sends `/x`; anything else is a prompt. |
| `src/memoryView.ts` | 18 | Pure: provenance in words (`you asked`, `you said`, `assistant — unconfirmed`) and the one line saying why learning is paused or failing. |
| `src/main.tsx` | 144 | Entrypoint — argument parsing, connect, renderer construction, reconnect loop, the consent question, and the `/memory` and `/forget` actions. |
| `src/ui/Root.tsx` | 10 | Mounted once; reads the store so listeners register once, not once per update. |
| `src/ui/Transcript.tsx` | 66 | Completed turns and the live turn; a shaded prompt, then reasoning (indented, dim, hidden until Ctrl+R), markdown text and tool calls drawn in the order they happened, as separate renderables with no text labels. |
| `src/ui/toolView.ts` | 72 | Pure: a tool call as a header subject (`✓ bash \`ls\` (12 ms)`) and at most three body lines indented four spaces. A `read` is just its path, never the file. |
| `src/ui/Modal.tsx` | 43 | Approval, first-run key, memory-consent and `/memory` modals. The approval modal shows the command through `displaySafe` and a `[network]` badge when the command asks for internet access. |
| `src/ui/MemoryPanel.tsx` | 58 | The `/memory` panel: list, scroll, forget with a Y/N confirmation, close. |
| `src/ui/App.tsx` | 48 | Composition, key handling and copy-on-select; the prompt bar shares a row with the perched elephant. |
| `src/ui/markdown.ts` | 17 | The `SyntaxStyle` the answer's markdown is drawn with. |
| `src/ui/copy.ts` | 22 | Where selected text goes: OSC52, plus `pbcopy` on macOS. |
| `src/ui/Mascot.tsx` | 57 | `useMascot` decides which stage the elephant is on; `TopMascot` (with the wordmark) and `PerchMascot` place it. |
| `src/ui/elephant/Elephant.tsx` | 113 | `TopElephant` and `PerchElephant`: a frame-buffer renderable played by an OpenTUI timeline. |
| `src/ui/elephant/scenes.ts` | 61 | Pure frame clips (intro, perch entrance, idle flap) and the 30–90 s flap pause. |
| `src/ui/elephant/sprites.ts` | 48 | The pixel art and its palette, ported from the design project. |
| `src/ui/elephant/wordmark.ts`, `Logo.tsx` | 16, 19 | The "elefante" wordmark as pixel rows, and the component that paints it once. |
| `src/ui/elephant/paint.ts` | 47 | Composes a frame into pixels and paints it as half-block cells. |
| `src/ui/elephant/phase.ts` | 21 | The `intro → perch` transition, as pure functions. |
| `src/ui/Status.tsx` | 16 | Thinking indicator, elapsed time, usage, the memory suffix when memory is not healthy, and `sandbox: off` or `sandbox: unavailable` in the warning colour. |
| `src/ui/Input.tsx` | 13 | Prompt entry; routes `/memory` and `/forget <id>` through `parseCommand`. |
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
`cost: $0.0000`, which is distinct from unknown. The cost is one total: turn usage plus the
cumulative memory-extractor spend from `MemoryStatus`, so a snapshot restores it exactly. While
memory is healthy nothing else is added; otherwise a suffix says so in words: `memory: paused  /memory`
(no answer to the consent question yet), `memory: not saving  /memory` (three extraction failures in a
row), `memory: unavailable` (the store or its key). Declined or switched off adds nothing.

## Memory

All of it is driven by the daemon's `memory` result field and `memory.*` events
(`../docs/DEVELOPER_GUIDE.md` §5, **Memory wire**); a daemon that sends none gets none of it.

- **Consent.** When the create or attach result says `consent: "unset"` with a non-null extractor,
  a modal names the extractor (and says so when it is the session's own model) and takes only `Y`
  (learn), `N` (don't) or `Esc` (ask again next launch). It owns typing and Esc, so nothing reaches
  the prompt and Esc does not interrupt a turn. The answer is `memory.consent` and applies to every
  session.
- **Transcript lines.** A learned note is a persistent dim line after the turn that was on screen
  when it arrived: `◇ remembered: …  /forget 14`, or `◇ updated: “old” → “new”  /forget 15`. A turn
  that used saved notes gets `◇ used N saved notes`. A reinforced or pending note draws nothing.
  Lines live in client state; a fresh attach starts from the snapshot and does not replay them.
  A snapshot restore (after a reconnect that cannot resume from the ring) does not bring back the
  `◇ remembered` / `◇ used` markers that happened inside a turn: snapshots carry no ordering, so
  there is nowhere to place them. `/memory` is the durable view of what was saved. The memory
  status (status line and cost) is not affected: every attach, including a reconnect, carries the
  daemon's current `memory`, and the client applies it.
- **`/memory`** opens a panel for the working directory's notes (`memory.list`), only while the
  prompt is enabled. `↑↓` select, `f` asks `Forget #14? “…”  Y / N`, `Esc` closes. It is a modal, so
  no key reaches the prompt. Approving, history and clearing stay in `elefante memory`.
  **`/forget <id>`** does the same without the panel.
- Every note string is sanitised again at the boundary (`display_safe` also runs on the daemon).

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
bun test              # 89 tests
bun run typecheck     # tsc --noEmit across all three layers
```

`tests/protocol.test.ts` (12) covers framing at every byte split, sequence windows, and the
no-daemon failure path. `tests/session.test.ts` (13) covers the reducer, including turn-less memory events. `tests/ui.test.tsx` (28)
drives the real OpenTUI test renderer — **without a TTY**, which is what lets the render site be
asserted in CI. `tests/elephant.test.ts` (11) covers the sprites, wordmark, clips, flap pause and phase transitions
as pure functions; `tests/commands.test.ts` (4) covers slash-command routing and note sanitising; `tests/copy.test.ts` (2) covers where copied text goes; `tests/toolView.test.ts` (8) covers how each tool call is laid out; `tests/elephant.ui.test.tsx` (11) renders the app and steps the animation by calling
`engine.update(ms)`, so no test waits on a real clock. `App`'s `animateMascot={false}` makes the elephant
paint its resting pose.

## Boundaries

No protocol logic in components, and no rendering in the reducer. That split is what keeps two
of the three layers testable headlessly; collapsing it would leave the UI assertable only by
screenshot.

`DEVELOPER_GUIDE.md` §5 is the wire contract this client implements. OpenTUI reference:
<https://opentui.com/docs/bindings/react/> and
<https://opentui.com/docs/core-concepts/testing/>.
