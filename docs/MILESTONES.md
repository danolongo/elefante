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

_No milestones completed yet._

**M0 is open.** Its provider baseline was reversed on 2026-09-02 — from two peer providers
(Anthropic + OpenRouter) to OpenRouter alone. The Anthropic transport was implemented, passed
its gate, and was then deleted. That work was not wasted: its SSE decoder and hermetic TCP
test harness carried over intact, and writing it is what surfaced the reasoning-block question
the contract settles. Patch, frozen type contract, and the decisions made while implementing
it: [`M0-PROVIDER-MIGRATION.md`](./M0-PROVIDER-MIGRATION.md).

Settled by that change: reasoning blocks are stored verbatim as opaque JSON rather than typed
variants, because OpenRouter's replay rule requires unknown future block types to round-trip.

**State: 52 tests passing, 2 ignored, `cargo clippy --workspace --all-targets` clean under
pedantic, `cargo fmt --check` clean.** Every gate item in [`PLAN.md`](./PLAN.md) § M0 now maps
to a test except the two below.

Two defects were found during implementation that the original M0 did not cover, both fixed:

- `elefante-core` discarded `Event::Usage` outright, which would have made the session-level
  cache gate unsatisfiable and left cost accounting with no input. `RunOutcome` now carries a
  `Usage` summed across rounds.
- Requests carrying `tools` did not set `provider: {require_parameters: true}`, so an endpoint
  could silently drop `tools` — the model answers in prose, the loop stops after one round,
  and the agent does nothing while appearing to work.

## Remaining before M0 can close

Both require `OPENROUTER_API_KEY` and issue real, billable requests. Neither can be run
without an operator's credentials, so neither is claimed as passing.

```sh
export OPENROUTER_API_KEY="..."
export OPENROUTER_MODEL="..."     # optional; a cheap tool-capable slug is fine

# Prompt-cache hit on the second identical request.
cargo test -p elefante-provider live_second_identical_request_reads_prompt_cache -- --ignored

# End-to-end: the loop completes and the edit lands. Runs in a temp directory,
# not this repository, so a misread instruction cannot touch tracked files.
cargo test -p elefante-cli -- --ignored --nocapture
```

The cache check needs a slug whose upstream actually supports prompt caching — `pricing`
must carry `input_cache_read`. `openrouter/auto` will not do: its capability profile leaves
caching off, because its pricing declares no cache rates.
