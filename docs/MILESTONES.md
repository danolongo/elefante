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

**M0 is open. Its provider baseline was reversed on 2026-09-02** — from two peer providers
(Anthropic + OpenRouter) to OpenRouter alone. The Anthropic transport was implemented, passed
its gate, and was then deleted. That work was not wasted: its SSE decoder and hermetic TCP
test harness carried over intact, and writing it is what surfaced the reasoning-block question
the contract now settles. Patch, frozen type contract, and the decisions made while
implementing it: [`M0-PROVIDER-MIGRATION.md`](./M0-PROVIDER-MIGRATION.md).

Settled by this change: reasoning blocks are stored verbatim as opaque JSON rather than typed
variants, because OpenRouter's replay rule requires unknown future block types to round-trip.

**State after the migration:** 47 tests passing (up from 39), 1 ignored (credentialed),
`cargo clippy --workspace --all-targets` clean under pedantic, `cargo fmt --check` clean.

One defect was found and fixed during integration that neither the migration nor the original
M0 covered: `elefante-core` was discarding `Event::Usage` outright, which would have made the
M0 session-level cache gate unsatisfiable and left M5 with no input. `RunOutcome` now carries
a `Usage` summed across rounds, with tests for the sum and for usage arriving after the stop
event.

Remaining before M0 can close: the rewritten gate in `PLAN.md` § M0 — specifically the
credentialed cache-hit check and the live repository-edit check, both of which need
`OPENROUTER_API_KEY`.
