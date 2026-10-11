# PR #15 review fixes — implementation plan

> **Status: draft for approval. Nothing here is implemented.** Source: two independent adversarial
> reviews of PR #15 (memory UI; agent behaviour). Findings below are cited as **U1–U9** (UI review),
> **A1–A6** (agent review) and **N\*** (nits). Severity is the reviewers'; where the plan disagrees it
> says so. A finding is **verified** if the reviewer demonstrated it with a test or probe, **read**
> if it came from reading code only.

## 0. Method and ground rules

1. **Reproduce first.** Each fix starts with a failing test that demonstrates the finding (the
   reviewers' throwaway tests are in the scratchpad and are the starting point). No fix lands
   without its test going red → green.
2. **Scopes ship independently** (§ 1–6), each as one or two scoped commits, each leaving
   `cargo fmt --check`, `clippy -D warnings`, `cargo test --workspace`, `tsc` and `bun test` green.
3. **Say what is guaranteed.** Where a fix has a residual window or a known gap, the code comment,
   the README and `MILESTONES.md` state it in those words; no doc may claim more than a test asserts.
4. **Branch:** commit onto `fix/agent-memory-use` so PR #15 updates in place (review remediation
   belongs to the PR). Alternative in § 8.

## 1. Scope S1 — Consent and learner lifecycle (`core`, `srv`, `mem`)

The only blocker lives here, and two majors.

### S1.1 U1 (blocker, verified) — revoking consent must stop a running learner
**Cause.** `discard_revoked_learning` (`srv/server.rs:1405`) clears the session's held history and
drops its learner handle, but `Learner::run_loop` (`core/learn.rs:651`) holds its own `Arc`, has no
consent check, retries a failed batch (`retry_delay`, `max_failures`) and keeps taking pending
batches; `ServerLearnSink` keeps emitting `memory.learned`.

**Design.**
- `Learner` gains a `cancelled: AtomicBool` plus the existing `idle` `Notify`; `Learner::cancel()`
  sets it, clears `pending`, wakes waiters. `observe`, `flush` and `kick` become no-ops once
  cancelled.
- `run_once` checks it at four points: before `take_batch`, before `ask`, after `ask` returns
  (reply is discarded — usage **is** recorded, the money was spent — but nothing is parsed,
  ingested, logged as `ingested`, or sent to the sink), and before the `changed_notes`/sink step.
  `complete()` races the stream against a cancel `Notified` so an in-flight HTTP call is dropped.
- `srv`: both revoke paths (`discard_revoked_learning` and the `!still_granted` branch of
  `observe_learning`) call `learner.cancel()` before dropping the handle.
- **Stated guarantee (and its limit):** after `cancel()` returns, no new extraction request starts
  and no new note is reported; an ingest transaction already committing on the writer thread may
  complete. `MILESTONES.md`'s "revoking discards held text before idle, close or exit can flush it"
  is reworded to exactly this.
- N2 (bundled, **verify first**): the reviewer says an idle flush is cancelled by any consent answer,
  even one for another extractor. `discard_revoked_learning` only touches entries that are no longer
  granted, so this may be unreal; write the test, fix only if red.

**Tests.** core: scripted provider whose first call fails → `cancel()` → assert the second call is
never made, no note stored, sink silent, usage still counted; cancel during a gated in-flight call →
reply discarded. srv (stub provider, real daemon): consent `learn` → cue turn with a gated extractor →
`memory.consent off` mid-call → release → assert zero notes, no `memory.learned`, request count
unchanged afterwards, `memory list` empty.

### S1.2 U2 (major, read) — check-then-act gap in `observe_learning`
**Cause.** `srv/server.rs:1603-1607` reads consent, drops the consent lock, then takes the learning
lock; a revoke's discard can run in the gap and the turn then rebuilds a learner and starts one.

**Design.** Take the learning lock **while holding** the consent guard (lock order consent → learning
is already the order `discard_revoked_learning` uses), compute `still_granted`, and keep the consent
guard until after `learner.observe(...)` returns (it only spawns). Audit `ServerLearnSink` and
`refresh_memory_status` for any path that takes learning → consent (would deadlock); assert the order
in a comment at the lock definitions.

**Tests.** A stress test (200 iterations, stub provider, interleaved `memory.consent off` and
prompt turns on two sessions): the invariant "no extractor request begins after the revoke response
was received" holds each time. Not a proof; the lock-order reasoning is the proof, the test guards
regressions.

### S1.3 U3 (major, read+demonstrated by reading) — `memory learn on` can grant for the wrong model
**Cause.** One consent record `{answer, extractor}` for everything. A fresh connection's
`memory.consent` targets `standalone_extractor()` (`ELEFANTE_EXTRACT_MODEL`, else `OPENROUTER_MODEL`,
else a hard-coded default); a session's extractor falls back to the **session's** model. `elefante
--model X …` with no extract model therefore prints "granted" while its session stays `unset`; and the
single record means the command also overwrites (silently revoking) a TUI's grant for X.

**Design.**
- **Consent file holds answers keyed by extractor**: `{"records":[{answer, extractor, stamp}…]}`,
  each stamp binding `(answer, extractor)` as today, so a tampered or reordered list stays
  tamper-evident per record. `mem::consent` gains `read_all`, `write_one` (read-modify-write under an
  exclusive create-temp-rename, as the current writer does); `read` for a single extractor remains.
  The old single-object file is read as a one-record list (this branch is unreleased; no migration
  beyond that).
- `ConsentGate.record: Option<ConsentRecord>` → `records: HashMap<String, ConsentAnswer>`; `seed`,
  `answer`, `granted` look up by extractor; `revision` still bumps on any change; revoke discards
  only entries whose own extractor was revoked.
- `memory.consent` accepts an optional `extractor` param (explicit target; falls back to the
  connection's last session, then the standalone default — today's behaviour).
- CLI: `elefante memory learn on|off [--model SLUG]` resolves the extractor exactly as a session
  would (`--model`, else `ELEFANTE_EXTRACT_MODEL`, else `OPENROUTER_MODEL`, else the default),
  sends it explicitly, and prints it: `memory learning is granted for z-ai/glm-5.3-flash`.
- Consent-file re-read: `seed`/`granted` re-read the file when its mtime changed, so a direct write
  made while a daemon is up (different socket path) is honoured (U4's second half).

**Tests.** mem: two records coexist; tampering one leaves the other valid; legacy single-object file
reads. srv: grant for X does not grant Y and does not revoke X when answering Y; a session on model
X with no extract model is granted by `learn on --model X`; mtime re-read. cli: `learn on --model`
prints the extractor.

### S1.4 U7 (minor, read) — health stays "failing" after revoke → re-grant
**Design.** Reset `SessionMemoryState.health` and recompute `problem` whenever the learner is cleared
(both discard paths) and when a new learner is created, then `refresh_memory_status`.
**Test.** srv: failing learner (stub returns errors ×3) → status `failing` → revoke → re-grant →
status `working`, `problem: null`.

## 2. Scope S2 — Repository handlers and CLI compatibility (`srv`, `mem`, `cli`)

### S2.1 U5 (major, verified) — `memory.*` by cwd creates directories and rebuilds stores
**Cause.** `repository_memory` (`srv/server.rs:1090`) calls `RepoMemory::open` → `Store::open_with_key`,
which creates `.elefante` and **rebuilds** a store with a foreign stamp (a sentinel row was deleted).
Direct-mode CLI only ever printed "not from this installation".

**Design.**
- `mem::Store::open_existing_with_key(repo, key) -> Result<Store, OpenError::{Absent, Foreign, …}>`:
  never creates a directory, never rebuilds, verifies the stamp. `RepoMemory::open_existing` wraps it
  (indexer not started).
- Handlers for `memory.list/forget/approve/clear/status` first reuse a live session's `RepoMemory`
  (unchanged); otherwise `open_existing`. `Absent` → `list`/`status` answer empty/`index: off`
  (`notes: 0`), mutating methods answer `UNKNOWN_NOTE`/`removed: 0`; `Foreign` → `MEMORY_OFF` with the
  reason "memory here was not made by this installation" (no rebuild, no delete). Rebuild remains
  only on the session-create/attach path, as before.
- Handler-opened stores are not inserted into `state.memories` (nit: "cached forever" disappears
  with it).

**Tests.** srv: `memory.list` on a plain temp dir creates nothing (`!dir/.elefante`); on a
foreign-stamped store returns the refusal and a sentinel row **survives**; on an absent store returns
`notes: []`; forget on absent → `UNKNOWN_NOTE`; the existing live-session paths unchanged.

### S2.2 U9 (minor, read) — `memory.status` by cwd reports an arbitrary session
**Design.** Deterministic aggregation: with several live sessions on one repository, `spend` is the
sum, `learning`/`problem` the worst (failing > paused > working) among sessions whose consent state
matches the standalone default's, `notes`/`index` from the store, `consent` from the standalone
extractor. With none, unchanged. Documented in `DEVELOPER_GUIDE.md` § 5.
**Test.** srv: two sessions, one failing → repository status `failing`; spend is the sum.

### S2.3 U4 (major, read) — CLI against an older daemon
**Cause.** `cli/main.rs` (~424-467) returns any RPC error except `UNKNOWN_NOTE`; a pre-branch daemon
answers `memory.*` with `-32601` (method not found), so list/forget/learn all hard-fail, and that old
daemon still learns by default.
**Design.** On `-32601` for `memory.list|forget|approve|clear` fall back to the direct store path
(current behaviour before the daemon route). For `memory learn on|off` on `-32601`: write the consent
file directly **and** print `warning: the running daemon predates consent control; restart it
(pkill elefante-srv) for this to take effect`. Exit status non-zero for `off` only if the daemon could
not be told (so scripts notice).
**Tests.** cli: a fake daemon (the helper `cli/tests/client.rs` already uses) answering `-32601` →
list/forget fall back and succeed; `learn off` writes the file and prints the warning.

## 3. Scope S3 — TUI and wire shape (`tui`, `srv`)

### S3.1 U6 (major, verified) — provenance is wrong for every note
**Cause.** `memory.list` rows carry `origin: "learned:stated"` (`mem/learn/admin.rs:116`); `memoryView.provenance`
compares with `"explicit"`/`"stated"`, so every note reads "assistant — you confirmed". The TS test
fixtures use unprefixed values, hiding it.
**Design.** One wire shape: the daemon strips `learned:` in `memory.list` exactly as `wire_note`
already does for events; `parseMemoryList` also tolerates the prefix (defence in depth); fixtures in
`tui/tests` and `cli/tests/client.rs` switch to the real daemon shape.
**Tests.** srv: `memory.list` `origin ∈ {explicit, stated, assistant}` for notes taught by user cue,
user statement and assistant-only; TS: `parseMemoryList` + `provenance` over **both** shapes;
contract test comparing a live daemon list to the TS parser (bun test spawning no daemon: use a
recorded fixture generated by the srv test and committed under `tui/tests/fixtures/`).

### S3.2 U8 (minor, read) — reconnect drops state
**Cause.** `recover()` (`main.tsx:122`) ignores the attach result's `memory`; a snapshot restore keeps
the old `state.memory` and no `memory.status` follows (the daemon's `view.memory` already matches), so
the status line and cost can be stale; mid-turn markers are lost on a snapshot restore.
**Design.** Have `RpcClient.reconnect` return the `AttachResult`; `recover` applies `memory` through
the same path first attach uses. Markers lost on a snapshot restore are **documented, not fixed**
(snapshots carry no ordering; `/memory` is the durable view) — stated in `tui/README.md`.
**Tests.** protocol.test: fake server, drop and reconnect, assert state picks up a changed `memory`.

### S3.3 Bundled nits (same files, cheap)
- N4: `/forget` accepts any whitespace run (`^/forget\s+(\d{1,15})$`); multiple spaces no longer
  become a model prompt. **Test:** commands.test.
- N5: `displaySafe` applied to `extractor` and `cwd` wherever the panel/modal draw them. **Test:**
  ui.test with control characters in both.
- Suspicion S1 (Esc closing the panel while another client's turn runs): **write the test**
  (`state.active_turn` set + panel open + Esc → `onInterrupt` not called, panel closes). It is
  expected green because the app's Esc handler checks `!props.modal`; if red, fix the handler.

## 4. Scope S4 — Guard (`guard`)

### S4.1 A1 (major, verified) — `.elefante/` bash guard bypasses
**Reality check.** The guard is argument-text matching, not a sandbox; the plan and PR body said
"cannot read around" — that overclaims. Note bodies are plaintext in SQLite, so the practical exposure
is a pending/superseded assistant claim, not a secret. The fix narrows the cheap holes and states the
rest.

**Design.**
1. **Final component.** `built_in_secret_rule` denies any `.elefante` component (not only one with a
   child), for `resolve` and bash arguments alike, so `grep -ra x .elefante` and `cat .elefante` are
   denied. (`is_secret_path` callers: the indexer walk already skips the directory; the daemon never
   resolves through `PathPolicy` under it. Audit and note.)
2. **Glob arguments.** In `looks_like_secret_path`, if an argument contains `*?[`, test each
   leading-dot component pattern against `.elefante` with `globset`; a match is `Deny`
   (`cat .elef*/memory.db`, `cat .[e]lefante/memory.db`).
3. **Recursive content readers over the workspace root.** `grep -r|-R|--recursive|-d recurse`
   (and `egrep`/`fgrep`) with a path argument of `.`, `./`, the workspace root, or no path, and no
   `--exclude-dir=.elefante`, plus `rg|ag|ack` with `-u|-uu|-uuu|--no-ignore|--hidden`, are
   **Escalated** (`recursive-search-includes-harness-state`). Plain `rg pattern` is left alone: the
   directory self-ignores (`*` gitignore), so ripgrep skips it by default. `find` and `ls -R` list
   names only and stay allowed; `find -exec` is already escalated.
4. **Known gaps recorded**, in `BashClassifier::known_gaps()`, the guard README and the milestone
   record: shell variables (`D=.elefante; cat $D/x`), `cd` then a relative path, interpreters
   (`python -c`, `sqlite3` reading a path assembled at runtime), `xargs`/command substitution
   producing a path. The wording becomes "reduces accidental reads; not a sandbox".

**Open decision D1:** item 3 adds approval prompts for `grep -r pattern .` (a common agent command).
Recommended: do it (escalate, not deny) — the same call the guard already makes for other
indeterminate commands. Alternative: document the gap and skip item 3.

**Tests.** guard bash_classifier: each reported bypass → Deny/Escalate as designed; legitimate
`grep -rn x src/`, `rg x`, `find . -name '*.rs'`, `ls -R` stay Allow; known-gap commands asserted as
Allow **and** listed in `known_gaps()` (the existing "documents the limit" pattern).

### S4.2 A2 (minor, verified) — a workspace under any `.elefante/` is fully denied
**Cause.** The rule runs on the absolute path (`path.rs:167`; `bash.rs:385` joins the root first).
**Design.** Apply the component rule to the **root-relative** path; separately deny the user-level
harness directory by canonical prefix (`<home>/.elefante`, already computed as `canonical_credentials`'
parent). **Tests.** root at `tmp/.elefante/proj`: `resolve("src/a.rs")` allowed, `ls src`/`cargo test`
allowed; `~/.elefante/memory.db` denied; symlink-into-`.elefante` cases unchanged.

## 5. Scope S5 — Agent prompt and tools (`core`, `tools`, `srv`)

### S5.1 A3 (minor) — the "may save preferences" sentence goes to every session
**Design.** Split the default prompt: keep "never read or edit `.elefante/`, which belongs to the
harness" (true everywhere); move "The harness may save lasting preferences … do not write them into
repository files unless asked" into `LEARNING_HINT`, appended through a new
`Session::with_system_note(&str)` **only when `learn_config.is_some()`** (memory on and learning not
`Off`) — decided in `srv::create` where that is already known. One system string, frozen at creation.
**Tests.** srv: `ELEFANTE_LEARN=0` session lacks `LEARNING_HINT` but has the memory hint; `ELEFANTE_MEMORY=0`
lacks both and equals `DEFAULT_SYSTEM_PROMPT`; word-cap test over the longest composition.

### S5.2 Tool listing derives from the executor (nit, bundled — removes a split-brain)
**Design.** `ToolExecutor::has_memory()`; `Session`'s per-round listing uses the executor it is
handed instead of `self.context.is_none()`. The hint stays tied to `with_context`.
**Test.** core: executor without memory + session with a context source → `memory_search` not listed;
and vice versa.

### S5.3 A6 (minor, design risk) — extractor rule can save a fact the assistant disproved
**Design.** Reword: "A statement says only what its quote supports. Record the person's preference,
convention or decision as they stated it; skip a factual claim the assistant has shown to be wrong,
and add no reasons, caveats or facts from elsewhere in the excerpt." JSON contract untouched.
**Tests.** core: the prompt constant contains the load-bearing phrases; the existing request-shape
tests pass. **Manual check** with `ELEFANTE_LEARN_LOG`: replay the "every test is `should_` and we use
`cargo nextest`" prompt on GLM; expect the convention saved and the false `nextest` claim not.

### S5.4 Marker ambiguity (nit, bundled)
Description names the exact marker: lines of the form ``memory <kind> — <statement>`` are saved notes.
**Test.** tools description test updated.

## 6. Scope S6 — Tests, docs, honesty

- **A4 — true test gaps** (the reviewer missed that some exist: the srv hint/tool assertions and the
  core gating test were added in `78fdc64`): add (a) tools: `kind:"note"` accepted, error names
  `file`, `symbol`, `note`; (b) core: `MemSearchTool` maps `Note` over a real store and returns the
  statement; (c) the pending-note test asserts the pending note **was stored** (list `all`) before
  asserting it is not returned; (d) the long-statement test uses a 180-character statement (it
  currently uses 161–180) or is renamed.
- **S2 log (suspicion, cheap):** `ELEFANTE_LEARN_LOG` `reply_head` passes through `display_safe`
  before writing so C1/bidi characters cannot reach someone who `cat`s the log.
- **Docs.** `MILESTONES.md`: revocation wording (S1.1), "CLI with and without a daemon" now covers an
  old daemon (S2.3), provenance (S3.1), eleven no-test items updated to what remains. `PLAN.md` and
  `DEVELOPER_GUIDE.md` § 5: per-extractor consent, `extractor` param, repository status aggregation,
  read-only handlers. Guard README: guard rail not sandbox, the known gaps. `AGENT-MEMORY-USE-PLAN.md`
  § 4: list exactly the tests that exist.
- **PR #15 body** (A5): credentialed run is stated against the actual tree; "agent behaviour" is
  four code commits plus the plan doc; add a "Review remediation" section linking this plan.
- **Credentialed suite** re-run on the final tree and recorded in `MILESTONES.md` with the sha
  (`live_learned_memory_is_extracted_stored_and_used` must run against the new extractor prompt).

## 7. Order, commits, gate

| # | Scope | Commit (scoped) | Depends on |
|---|---|---|---|
| 1 | S1.1, S1.4 | `fix(core): stop a running learner when consent is revoked` | — |
| 2 | S1.2 | `fix(srv): close the gap between checking consent and starting extraction` | 1 |
| 3 | S1.3 | `feat(mem): keep a consent answer per extractor model` + `fix(cli): grant consent for the model the session will use` | 2 |
| 4 | S2.1, S2.2 | `fix(srv): never create or rebuild a store for a read of saved notes` | — |
| 5 | S2.3 | `fix(cli): fall back to the store when the daemon predates memory control` | 3, 4 |
| 6 | S3.* | `fix(tui): show where a note came from; keep memory state across a reconnect` | 4 |
| 7 | S4.* | `fix(guard): close the cheap .elefante bypasses and say what remains` | — |
| 8 | S5.* | `fix(core): offer the learning hint only where learning can run` | — |
| 9 | S6 | `test: cover the gaps the review found` + `docs: correct claims the review disproved` | all |

Order is by severity then dependency (blocker first); 4, 7, 8 are independent of 1–3 and could be
done in parallel by separate agents if desired, but share files (`srv/server.rs`) with 1–3, so they
are sequenced rather than parallelised for the srv-touching ones (1–6) and parallelised only for
7 and 8.

**Gate after every commit:** `cargo fmt --check`, `clippy --workspace --all-targets -D warnings`,
`cargo test --workspace`, TUI `tsc` + `bun test`. **Gate at the end:** the above plus `cargo +1.90.0
check`, `bun build --compile` + the CI smoke test, the credentialed suite (user's shell), and a
second adversarial review of the diff against this plan.

## 8. Decisions needed

- **D1** — escalate recursive `grep -r .` / `rg -u*` over the workspace (recommended) or document the
  gap only (§ S4.1 item 3).
- **D2** — consent file format changes to a per-extractor list (unreleased, no migration beyond reading
  the old one-record shape). Recommended.
- **D3** — commit onto `fix/agent-memory-use` (PR #15 updates in place; recommended) or a new
  `fix/pr15-review` branch with its own PR stacked on #15.
- **D4** — confirm the deferrals below.

## 9. Deliberately deferred (with reasons)

| Item | Why not now |
|---|---|
| N1 held history grows for the whole session | Needs the learner's watermark/history ownership reworked; separate issue |
| N3 new enum values break both parsers | Protocol policy (unknown-value tolerance) is a wider decision than this PR; record in `DEVELOPER_GUIDE.md` § 5 as a rule for the next addition |
| `kind:"note"` returns ≤ ~6 text-matched notes | Disclosed in the tool description; revisit with the § 5 measurements of the agent-memory plan |
| Notes ranking below code in unfiltered search | Unmeasured and predates the PR |
| 3 `elefante-cli` tests failing in the reviewer's sandbox | Looks environmental (2 s daemon-ready timeout); they pass locally — confirm on CI, do not change code on a guess |
| Cached handler-opened stores | Disappears with S2.1 (handlers stop caching) |
