# Milestone record

One entry per completed milestone. A milestone is complete only when every item in its
**Baseline** is implemented and its full **Gate** passes from a clean checkout — including
the gates of all prior milestones. See [`PLAN.md`](./PLAN.md) § Development discipline.

Entry format:

```
## M<n> — <name>            (completed YYYY-MM-DD)

**Shipped.**   What landed.
**Gate.**      What the gate proved, and how it was run.
**Deferred.**  Anything deliberately left out, and why.
**Amended.**   Any change made to PLAN.md as a result of what was learned.
```

---

## M0 — Loop            (completed 2026-09-02)

**Shipped.** Cargo workspace of eight crates. `Provider` trait + `Capabilities` with a single
OpenRouter transport: OpenAI-compatible SSE, per-model capability resolution, `reasoning`
control and verbatim replay, `tool_calls` normalised by array index, prompt caching where the
upstream supports it, and `usage` accounting including real `cost`. Transport-generic SSE
decoder in its own module. Agent loop over seven tools with plain stdout. `PathPolicy`,
the deterministic bash classifier, and the output scrubber — an ungated bash tool never
existed, not even briefly.

**Gate.** 55 hermetic tests, plus both credentialed checks run against a live key:
`live_second_identical_request_reads_prompt_cache` and `live_edit_lands_in_a_scratch_repository`.
`cargo clippy --workspace --all-targets` clean under pedantic; `cargo fmt --check` clean.

The credentialed half earned its place. It caught a bug the hermetic suite **structurally
could not**: `DEFAULT_BASE_URL` lacked a trailing slash, and `Url::join` replaces the final
path segment without one, so `new()` resolved to `/api/chat/completions` with `v1` silently
dropped. Every hermetic test overrides the base URL to reach the mock server, and that path
normalises the slash — so the shipped default was the one code path nothing exercised. Anyone
relying on it would have hit a 404 on their first request. Now pinned by two offline tests.

Two further defects were found and fixed during the milestone, neither covered by the
original baseline:

- `elefante-core` discarded `Event::Usage` outright, which would have made the session-level
  cache gate unsatisfiable and left cost accounting with no input.
- Requests carrying `tools` did not set `provider: {require_parameters: true}`, so an endpoint
  could silently drop `tools` — the model answers in prose, the loop stops after one round,
  and the agent does nothing while appearing to work.

**Deferred.** Nothing from the baseline. The interactive approval transport for escalated bash
commands waits on the daemon and TUI, as planned — the classifier decides, but there is
nowhere to render a prompt yet.

**Amended.** The provider baseline was reversed mid-milestone from two peer providers to
OpenRouter alone, which settled how
reasoning blocks are represented: verbatim opaque JSON, because the replay rule requires
unknown future block types to round-trip. `PLAN.md` also gained the model-picker and
first-run key-setup designs, the plan to derive capabilities from catalogue metadata rather
than hardcoded slug prefixes, and gate items for `require_parameters` and `openrouter/auto`.
Default model moved to `anthropic/claude-sonnet-5` to keep credentialed runs cheap.

---

## M1 — Daemon + TUI            (completed 2026-09-05)

**Shipped.** Typed wire protocol with NDJSON framing, snapshots, and coalescing
(`elefante-rpc`). Unix-socket daemon with session registry, resume, fanout, backpressure,
credential storage, approvals, and a stub provider (`elefante-srv`). Persistent `Session`
owning history across turns, cancellation-safe tool pairing, and lifecycle events
(`elefante-core`). The bash gate moved from `tools` into `core`, so `Escalate` finally means
something distinct from `Deny`. Daemon-only CLI with a stream-separated renderer. Bun/OpenTUI
client with a pure protocol layer and reducer (`tui/`).

**Gate.** 118 hermetic Rust tests, 23 TypeScript tests (275 assertions), and both credentialed
checks — including `live_edit_lands_in_a_scratch_repository`, which had to be re-run because
the CLI's entire execution path changed from in-process to daemon-and-socket. Clippy under
`-D warnings`, `cargo fmt --check`, and `tsc --noEmit` all clean.

**The contract was split before implementation, and that is why this landed cleanly.** An
adversarial review of the first M1 draft found four blocking defects sharing one root cause:
the wire had been specified before the core types it derives from. Every notification was
unproducible from the shipped `OutputEvent`; the session type it presupposed did not exist;
the interrupt guarantee was unachievable against `join_all`. Splitting into `M1-CORE.md` and
`M1-PROTOCOL.md` cost one review and fixed all four before a line was written.

Two gate items existed only because that review found they would pass on a broken
implementation, and both proved load-bearing: the stub provider **validates**
`tool_use`/`tool_result` pairing (without it the pairing assertion is vacuous), and attach
asserts `kind == "resumed"` (without it an always-snapshot implementation passes while the
ring is dead code).

**Deferred.** `session.list` waits for the tmux fanout work. Session snapshots remain
unbounded, as the contract documents — open question 1.

**Amended.** MSRV raised 1.85 → 1.89 for dependency-free directory locking, then corrected to
1.90 on 2026-09-06 — see below. Bun is now a hard runtime requirement, not only a test
dependency: the interactive client will not launch without it. `PLAN.md`'s turn lifecycle
gained a status marker distinguishing shipped behaviour from design — reading that diagram as
description is what produced the first draft's unachievable guarantees.

---

## Post-M1 — CI and the lockfile (2026-09-06)

Settled the second of the two questions M1 left open. The first — Bun distribution — is
decided in principle and deferred in practice: `bun build --compile` produces a working
69.2 MB self-contained binary with the native `libopentui.dylib` embedded, at no startup cost
(80 ms, against 70 ms interpreted). CI now runs that compile on every push so the option stays
open, but the packaging work waits for a release.

**One lockfile.** `tui/package-lock.json` removed, `tui/bun.lock` committed. With both present
`bun install --frozen-lockfile` and `npm ci` each report a successful frozen install and
produce different trees, silently; `bun.lock` shadows `package-lock.json` completely, to the
point that a corrupt npm lockfile is simply never read. Gitignoring `bun.lock` instead is
worse — the untracked local file shadows the committed one, so dependency bumps reach CI and
not developers. Reasoning and measurements in [`LOCKFILE.md`](./LOCKFILE.md).

**CI exists now.** There was none — `cargo test` was as unautomated as `bun test`. `ci.yml`
runs fmt, clippy, the Rust suite, an MSRV floor check, the TypeScript suite, and the compile
smoke test on Linux and macOS. `credentialed.yml` runs the two billable live tests behind
`workflow_dispatch` only. Five gate items live solely in `bun test`, two of them regression
guards for defects that shipped: issue #4's render site, and the connect-before-renderer seam
(`e23eb6d`).

**The MSRV floor was already false.** The new `msrv` job failed on its first run: the
workspace declared `rust-version = "1.89"` but `tree-sitter@0.27.0` requires 1.90, so it had
never built on its own declared floor. Corrected to 1.90 and verified. `README.md` still said
1.85, two revisions stale. *This is the argument for the job in one paragraph — a declared
floor nobody compiles against is a comment.*

**And the toolchain was pinned only on one side.** The first CI run also failed `clippy`, on
`collapsible_match` in `srv/src/stub.rs` — a lint that exists in the runner's clippy 1.98 and
not in the 1.94 every local check had used, so no amount of local verification could have
found it. The workflow pinned Bun and left Rust on `stable`, which with `clippy::pedantic` at
warn and `-D warnings` means any Rust release can redden CI on an unrelated PR. Now pinned to
1.98.1, with an advisory `lints` job on floating `stable` that reports new lints without
power to fail the run. *Clippy's own suggestion was declined: it wanted the nested `if` folded
into a match guard, and guards run during pattern selection, so `remove` would have mutated
`pending` on arms that then did not match.*

**Three findings, one shape.** The credentialed run then failed a third time, for a third
reason: `cargo test -p elefante-cli` builds only that package, but the CLI spawns
`elefante-srv` — a binary from another crate — as a sibling of its own executable. Every local
`target/` had one left over from a `--workspace` build, so the test could not fail here.

All three defects were the same defect. A false MSRV floor, a lint absent from a six-month-old
local clippy, and a binary present only by accident: each was a local environment quietly
supplying something the declared inputs did not, and each was invisible to any amount of
careful local checking. *This is what CI buys that discipline cannot — not a second opinion,
but a machine with no memory of what we happened to build last week.* Worth holding onto for
M2, which adds a cache layer whose whole job is to make local state survive between runs.

---

_Next: M2 — repo cache and mechanical graph._
