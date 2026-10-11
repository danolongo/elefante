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
`M1-PROTOCOL.md` cost one review and fixed all four before a line was written. Both documents
were retired once the code shipped — the wire contract they carried now lives in
[`DEVELOPER_GUIDE.md`](./DEVELOPER_GUIDE.md) §5.

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

**The M1 contracts were retired.** `M1-CORE.md` and `M1-PROTOCOL.md` are deleted. The core
types they froze are now embodied in code and needed no second home, but the wire contract did:
the daemon and the TypeScript client speak that format across a process boundary, so it moved
into [`DEVELOPER_GUIDE.md`](./DEVELOPER_GUIDE.md) §5 — transport, sequencing, method set,
attach and resume, the ring and the two buffers, session lifecycle, and the error codes. The
two questions the contract left open moved to §14. *Retiring them exposed how far the guide had
drifted: it still announced "the repository is at M0", "there is no daemon", and "there is no
OpenTUI client", and told readers to wait for an approval transport that had shipped. A
reference that describes a version of the project nobody is running is worse than none, so it
was corrected in the same pass.*

---

## M2 — Repo cache + mechanical graph            (completed 2026-10-08)

**Status.** Every item of the gate is green, the credentialed ones included. On 2026-10-08, against
`anthropic/claude-sonnet-5`, `live_memory_context_reaches_the_model` passed — a real provider accepted
the injected preamble-and-block request shape, and with `--max-rounds 1` the model named a function it
could only know from the index it was sent. Because `tools`, `core` and `srv` changed, the two M0/M1
credentialed tests were re-run too and also pass: `live_second_identical_request_reads_prompt_cache`
and `live_edit_lands_in_a_scratch_repository`.

**Shipped.** A per-repository index in `elefante-mem`: a walk that honours ignore files and prunes
secret paths unread; stat-based change detection with a racy-timestamp rule; a content-addressed
extraction cache; symbols and imports for Rust, TypeScript/TSX and Markdown headings; a graph in
`memory.db` with stable node ids; import edges derived from the set of paths; `co_edited_with`
edges from `git log` with rename aliasing; FTS5 retrieval with graph expansion, packed into a token
budget and rendered as an escaped, framed block; a deterministic repository brief; a background
`Indexer` whose triggers coalesce and never block. Integration: `memory_search` is a real tool
(`MemorySearch` is a trait in `tools`, implemented in `core`, so `tools` still depends only on
`guard`); `core` places an ephemeral block after each turn's user message and a frozen brief at the
head of the request, both `user`-role and neither stored; `srv` opens one store and indexer per
repository and refreshes at session create and around every turn; `ELEFANTE_MEMORY=0` turns it all
off. No RPC method, notification or error code was added.

**Gate.** 350 hermetic Rust tests (210 in `mem`), 24 TypeScript tests, clippy under `-D warnings`,
`cargo fmt --check`, `tsc --noEmit`, and `cargo +1.90.0 check` — all after `cargo clean`. The M0 and
M1 suites for `guard`, `tools`, `core` and `srv` re-ran after every stage that touched them. Every
assertion in the plan's gate is a counter, never a clock: `parsed == 0` on a warm start,
`parsed == 1` after one edit, `git_log_runs == 0` when `HEAD` has not moved, reader-actor call
counts for the cache, and the raw bytes of every file under `.elefante/` for the secret fixture.

**Where the tests earned their keep.** Defects found *by the tests, not by inspection*, each fixed
and each now pinned:

- The identifier splitter classified characters from its own lowercased output, so every upper-case
  run split into single letters (`HTTPServer`, `TURN_LOCK`); caught by the first table.
- A `///` comment separated from its item by a blank line was attached as its doc, because a node
  that ends with a newline ends at column 0 of the *next* row.
- A `touch` refresh counted a graph commit it had not made.
- **A crash after a content batch committed would have left import edges un-derived forever**, since
  every file looked up to date on the next run. The trigger was in-memory; it is now a persistent
  `meta.edges_dirty` written in the same transaction as the change. The mutation that removes it
  fails five tests.
- **`partial` as designed ("the indexer is running") would have marked nearly every block**, because
  refreshes run at every turn start, concurrently with retrieval. It now means "the first full index
  has not finished". Found by an integration test whose tool result carried an "index still being
  built" note it had no business carrying.
- The first draft of the brief's cache key was the tree fingerprint alone; "central files" depends
  on edges, which move with `HEAD` while no file does.

Each guard was also checked the other way: the racy-timestamp window, the orphan sweep, the dirty
flag, `HEAD` gating, rename aliasing, the secret rules, block placement, coalescing, the kill
switch and the ranking order were each disabled in turn and the corresponding tests confirmed to
fail. A test that cannot fail is documentation.

**Observed, not gated.** On this repository (124 files, 1,048 symbols, 38 commits): a cold index
took about 320 ms with 92 parses, a warm refresh 5 ms when files had settled (33 ms immediately after
a fresh clone, when every file is inside the racy window and is rehashed once), and the two databases
total about 1.2 MB. 130 `depends_on` edges and 306 `co_edited_with` edges. Timing was never asserted.

**Deferred.** Learned memory (M3). Workspace-crate, `tsconfig` and bare-package import resolution;
a call or reference graph; `.js`/`.jsx` extraction; further languages; embeddings; a file watcher; a
model-written brief. Session-touched files as a retrieval signal (`Query.files` carries only prompt
mentions; the field exists so adding them changes no signature). Whether the agent's own tools may
open `.elefante/*.db` — decide with the transcript question in M6.

**Amended.** `PLAN.md`: placement of retrieved context is ephemeral and `user`-role, not
capability-dependent `system` — `Capabilities::mid_conversation_system` therefore has no core caller
until M4; storage, the memory actor (two threads, not one), the schema (no `CHECK` on `kind`/`rel`;
`source` as the ownership discriminator; `import_ref`; stable ids), ingestion and scheduling were
rewritten from what shipped; the M2 gate gained its additions. `DEVELOPER_GUIDE.md` and every
affected crate README were rewritten, `mem`'s from "a four-line scaffold" to the real contract.
`memory_search`'s refusal text no longer says "milestone M2", so the one M0 test that asserted the
old wording was updated. `docs/M2-DESIGN.md`, the design pass the plan required, is retired into
these as the M1 contracts were; it remains in history at `f5485f7:docs/M2-DESIGN.md`.

**Deviations from the design pass, recorded where they happened.** Extraction uses hand-written tree
walks rather than `.scm` query files; `file_state` gained a `skip` column so a binary is read once;
the brief's cache key covers the co-change head; `partial` ignores a running refresh. Each is
explained in `PLAN.md` or the crate README.

---

## M3 — Learned memory            (implemented 2026-10-11; credentialed check passed on its second live run)

**Status, stated plainly.** Every item of the gate is green. Hermetic: `cargo fmt --check`, clippy under
`-D warnings`, 486 Rust tests across the workspace (5 credentialed ones ignored) and 67 TypeScript tests,
`cargo +1.90.0 check`, `tsc --noEmit` and `bun test`, all run locally in the order CI runs them.
Credentialed: `live_learned_memory_is_extracted_stored_and_used` — which proves a real provider accepts
the extraction request, that a real model's reply passes the store's verification, and that the stored
note reaches a later prompt through a real daemon — **failed on its first live run and passed on its
second** (5.5 seconds, 2026-10-11), after the changes described below. It has passed once.

**The first live run failed, the second passed.** `live_learned_memory_is_extracted_stored_and_used` was run against
a real provider and stored no note (`found []` after 90 seconds). The test as written could not say why,
so the cause is **not known**. Candidates, none confirmed: the provider refusing the strict
`response_format` (with `require_parameters`) for the chosen model; a reply cut short by `max_tokens`
before any JSON; the store refusing a note whose quote or backticked example the model altered. In
response the extractor now retries once without the schema when a provider refuses it, uses a schema
without union types, asks for 2,048 output tokens, and keeps diagnostics (`Learner::diagnostics`); the
live test now runs the extraction in-process and prints them on failure. A local server speaking
OpenRouter's wire format (`crates/core/tests/learn_wire.rs`) shows the transport, SSE parsing, schema
fallback and error reporting lose nothing that arrives, and it exposed one realistic cause: a model that
quotes the rule and restates the user's example in a code span had its note refused, because the
payload rule compared against the quote rather than the user's turn. Three checks that real model
output trips were loosened: a quote now matches on whole words regardless of punctuation, backticks or
an ellipsis; a command, URL or identifier in a note may be anything the user wrote in the turn the
quote came from (it was: only the quoted part), matched as whole words; and a label longer than 60
characters is cut instead of refusing the note. The test passed on its next live run. **Which of these
changes made the difference is not known**: they were made together, and the first run had no diagnostics.
The offline reproduction found the payload-rule refusal to be a real defect either way; the schema
fallback and the larger output budget are insurance whose effect was not isolated. Until a live run passes, the claim that a real model's reply survives verification is
unproven.

**Shipped.** `elefante-mem::learn`: learned notes with provenance by speaker, verification (quote,
payload-shaped wording, scrubber), hybrid dedupe and supersession, reinforcement and promotion,
read-time decay, compaction, and the controls behind `elefante memory`. `elefante-mem::trust`: a key
at `~/.elefante/memory.key`, keyed stamps on both databases and a MAC on every learned row; a store
without a valid stamp is discarded and rebuilt, and retrieval renders only index rows and verified
learned rows. Retrieval gained learned seeds, `about` hops in both directions, confidence weighting,
a scope rule and a separate `<repo-memory>` section within its own budget. `elefante-core::learn`: the
extraction loop — gate, watermark, single-flight, caps, three-strike skip. `elefante-provider`:
`Request::response_schema`, sent as a strict `response_format` only to models that declare structured
output. `elefante-srv`: one learner per session, flushed on close, after quiet, and at exit;
`ELEFANTE_LEARN`, `ELEFANTE_LEARN_IDLE_SECS`, `ELEFANTE_EXTRACT_MODEL`, `ELEFANTE_EXTRACT_EFFORT`.
No RPC method, notification or error code was added.

**A hole in M2, closed here.** A repository could ship its own `.elefante/`; `Store::open` accepted any
database at the right schema version, retrieval rendered rows without checking `source`, and a
hostile `index_complete` was believed. The fix was written before its test, so the test was checked by
mutation: with the `source` filter removed, `rows_of_an_unknown_source_are_never_rendered` renders the
planted file's "SYSTEM:" text; with it, it does not. Version-1 stores are rebuilt, not migrated: they held
nothing irreplaceable and a migration could not tell a legitimate store from a hostile one.

**Where the tests earned their keep.** Defects found by tests, not inspection: the key's temporary file
was named by process id alone, so concurrent first use inside one process collided; the stemmer left
`tabs` unstemmed (it only stripped plurals from words longer than four letters); and `flush` bypassed the
per-session call cap. The verdict-handling tests also had to be rewritten once, because a test that
reused one store across cases let the first case's paraphrase become the second case's duplicate.

**Deferred.** The user-level graph; transcript compaction (the seam exists); re-linking a note across
a rename or branch switch (a direct `about` edge goes with the file and the note stays, found by its
words); learned memory shared across worktrees; `contradicts` edges; a TUI surface for pending notes;
embeddings or any handling of a prompt phrased in vocabulary the code never uses. The first-run
setup flow that will ask for an extractor model is in `PLAN.md` and unbuilt: until then it is
environment variables.

**Deviations from the design pass.** `quote_turn` was dropped from the reply schema (the quote is
searched in every turn, preferring a user's); nearest-note lookup for the extractor is a lexical scan
of at most a few hundred rows, not an FTS query; `*.key` was already a built-in secret path, so no new
`PathPolicy` rule was needed; `cache.db` is stamped too; the relatedness test for a model verdict uses
the overlap coefficient (0.4) while duplicates use Jaccard (0.9), both pinned by a labelled corpus.

---

_Next: M4 — critics._
