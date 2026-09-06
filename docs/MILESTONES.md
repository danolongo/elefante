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

_Next: M1 — daemon, RPC protocol, and OpenTUI client. Frozen contract in [`M1-PROTOCOL.md`](./M1-PROTOCOL.md); no implementation started._
