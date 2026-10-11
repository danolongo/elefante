# elefante-mem

The repository index: a graph of the files and symbols in a repository, kept current in the
background, searched by prompt, and packed into a small block of context for each turn. **It never
runs a model.** Everything here is deterministic, which is what lets a fixture repository's
co-change weights be computed by hand and a query's output be byte-identical across runs.

Nothing here talks to a provider or a socket. `elefante-core` turns a retrieval into a message
and `elefante-srv` decides when to refresh; this crate only knows how to build, store and query
the graph.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 30 | Module declarations and re-exports. |
| `src/layout.rs` | 98 | `StoreDir` — the self-ignoring `.elefante/` directory, its modes, and file paths. |
| `src/repo.rs` | 79 | `RepoId` — which directory owns the index for a given working directory. |
| `src/schema.rs` | 301 | DDL for both databases, versioning, and the pairing epoch (`pair`). |
| `src/store.rs` | 147 | `Store` — the open pair of databases, two actors, and `IndexState`. |
| `src/actor.rs` | 111 | `Actor` — a thread that owns a connection and serialises access to it. |
| `src/walk.rs` | 141 | The repository walk and its filter rules. |
| `src/diff.rs` | 67 | Decides which files must be read (stat shortcut, racy-timestamp rule). |
| `src/hash.rs` | 52 | Reading and `blake3`-hashing files on a bounded thread pool. |
| `src/state.rs` | 82 | `file_state` rows: what the cache remembers about each file. |
| `src/extract/` | 1,020 | Symbols and imports per language, and the content-addressed cache. |
| `src/graph.rs` | 699 | Every write to the graph, stable ids, edge derivation, co-change storage. |
| `src/resolve.rs` | 215 | Import specs → file paths, for Rust and TypeScript. |
| `src/cochange.rs` | 332 | `git log` parsing, rename aliasing, pair counting, the bounded runner. |
| `src/indexer.rs` | 547 | `refresh_blocking`, and `Indexer`: single-flight background refreshes. |
| `src/retrieve.rs` | 897 | Query processing, seeds, expansion, packing, rendering, `Retriever`. |
| `src/brief.rs` | 259 | The repository brief and its Merkle-root cache. |
| `src/terms.rs` | 109 | Identifier and path splitting for the search index. |
| `src/consent.rs` | 111 | The user-level consent file: `ConsentRecord { answer, extractor }`, read and written with a keyed stamp. |
| `src/trust.rs` | 190 | `MemKey` — the keyed `blake3` stamps and row MACs that say which rows this harness wrote. |
| `src/learn/` | 1,900 | Learned memory: verify, dedupe, supersede, decay, compact, and the person's controls. |
| `src/stats.rs` | 64 | `Counters` and `IndexStats` — what the gates assert on. |
| `src/error.rs`, `src/util.rs` | 85 | `MemError`; `now_ns` and `par_map`. |

## The two databases

```
<repo_root>/.elefante/
  .gitignore      # `*` — written first, before any database exists
  memory.db       # the graph: node, edge, import_ref, node_fts, meta
  cache.db        # what indexing already did: file_state, extraction, brief
```

Directory `0700`, databases and their `-wal`/`-shm` sidecars `0600`.

**`memory.db` is the graph.** It is what retrieval queries. From M3 it also holds learned memory,
which cannot be regenerated, so its schema migrates; `cache.db` is disposable and is dropped and
recreated on any version mismatch.

| Table | Role |
|---|---|
| `node` | Every thing in the graph: a `file` or a `symbol` today. `(kind, key)` is a stable natural key. `source = 'index'` marks rows the indexer owns; it never touches any other. |
| `edge` | `depends_on` (file → file, from an import) and `co_edited_with` (file ↔ file, stored once with `src < dst`). |
| `import_ref` | Raw import specs as written. Edges are *derived* from these plus the set of paths. |
| `node_fts` | FTS5 over pre-split identifier terms, path terms and body terms. |
| `meta` | `graph_epoch`, `repo_generation`, `index_complete`, `cochange_head`, `edges_dirty`, `truncated`. |

**`cache.db` is bookkeeping.** `file_state` is `(path, size, mtime, blake3, indexed_at, skip)` per file; `extraction` is the parse result keyed by **content hash** and extractor id, so a
rename, a branch switch back, or a rebuilt `memory.db` costs no parse; `brief` is the cached repo
summary. Symbols are stored as signatures and one doc sentence — **never source, never a body**.

## How the two stay honest with each other

They cannot share an atomic commit (WAL is atomic per database), so three rules replace
atomicity:

1. **Pairing epoch.** `memory.db.meta.graph_epoch` is random per database; `cache.db` records the
   epoch its `file_state` was written against. A mismatch — including a deleted `memory.db` — clears
   `file_state` and **keeps `extraction`**, so the graph rebuilds with zero parses.
2. **Commit order: graph first, cache second.** A crash between leaves the cache *behind* the
   graph; those files are re-examined and re-upserted identically. The reverse order would claim a
   file was indexed that never reached the graph.
3. **Orphan sweep.** After a *complete* walk, index-owned file nodes whose path was not walked are
   deleted. This is what handles a deleted `cache.db`, where no deletion is ever noticed.

Deleting either database, or both, converges on the same graph; the tests compare node and edge
multisets for equality, not non-emptiness.

## Entry points

```rust
use elefante_mem::{RepoId, Store, Indexer, indexer::IndexOptions, retrieve::{Query, Retriever}};

let repo = RepoId::resolve(&cwd)?;                  // refuses `/` and $HOME
let store = Arc::new(Store::open(&repo)?);
let indexer = Indexer::new(store.clone(), repo.root().into(), IndexOptions::default());
indexer.refresh();                                  // returns immediately; coalesces

let retriever = Retriever::new(store.clone());
let block = retriever.retrieve(Query::new("how does the session take the turn lock")).await?;
// -> Option<Arc<MemoryBlock>>; `block.rendered` is the text to inject

let text = elefante_mem::brief::get_or_render_async(&store).await?;   // the standing summary
```

`refresh_blocking(store, root, counters, options)` is the same work, synchronously, and is what the
tests drive. `retrieve::search` renders a plain `path:line  kind name — description` listing for the
`memory_search` tool.

## What retrieval does

1. Split the prompt into words the way identifiers are split at index time, drop function words,
   and quote every term — raw prompt text is **never** handed to FTS5, where a stray `"`, `NEAR(`,
   `*` or `-` is a syntax error.
2. Take the top 12 FTS rows by `bm25(10, 4, 1)` over name, path and body terms. Scores are
   **rank-based**, because bm25 magnitudes are not comparable across queries.
3. Seed any file the prompt names outright (an exact path, or an unambiguous bare file name).
4. Expand two hops along `depends_on` and `co_edited_with`, at most 8 neighbours per node, damped
   near hubs, scored by the **best** path rather than the sum so a hub cannot accumulate.
5. Pack by file into the token budget and render. Ties break on the key, so output is
   byte-deterministic.

The result is *data*: escaped, framed as reference material, never instructions, and `None` — not
an empty envelope — when nothing matches. `partial` means the first full index has not finished; an
incremental refresh on a complete index is not partial.

## Learned memory

`learn/` holds durable notes distilled from conversations: `preference`, `convention`, `decision`
and `fact` nodes in `memory.db`, `source = 'learned:*'`, which the indexer never touches. **This
crate still never runs a model.** A caller (`elefante-core`) hands it *candidates* — what a model
proposed — and the excerpt they were drawn from; every decision is made here, by code with tests.

```
verify (per candidate)       quote is in the excerpt (whole words, punctuation ignored) · wording carries no
                             command/URL/path/identifier the speaker did not write in that turn ·
                             survives the Scrubber unchanged · evidence's speaker sets origin
dedupe (against live notes)  Jaccard over stemmed content words, negations kept as tokens:
                             ≥ 0.9 and the same negations → duplicate, whatever the model said
                             otherwise a model verdict (same_as / replaces) is honoured only if it names
                             a note it was shown, of the same kind, lexically related or sharing an anchor
commit (one transaction)     add · reinforce · promote · supersede · revive, then one generation bump
```

| Origin | Evidence | Status | Starts at |
|---|---|---|---|
| `learned:explicit` | a user message with a cue ("remember", "from now on"…) | `active` | 0.9 |
| `learned:stated` | a user message | `active` | 0.6 |
| `learned:assistant` | assistant text only | `pending` — never searched, never shown | 0.4 |

Only the user's own words can reinforce, promote or replace a note, so the assistant restating a
note it was shown cannot raise it. A `replaces` verdict retires the old note (`status =
'superseded'`, a `supersedes` edge, its FTS row deleted); history stays inspectable until the
retention window ends.

**Integrity.** `memory.db` and `cache.db` each carry a keyed stamp, and every learned row a keyed
MAC over the fields that decide what the model reads (`trust.rs`). The key lives in
`~/.elefante/memory.key` (`0600`, created on first use), never in a working tree. A database that
is not at the current schema or lacks a valid stamp — a version-1 store, a `.elefante/` a repository
shipped, one made under another key — is **discarded and rebuilt**; a row that fails its MAC is
treated as absent; retrieval renders only `source = 'index'` rows and verified learned rows. This is
integrity and origin, not truth, and it does not stop a process that can read the key.

**Decay.** `effective = confidence × 0.5^(age ÷ half-life)` since the note was last reinforced
(preference and convention 180 days, decision 90, fact 30), computed on read so nothing is written as
time passes. Below 0.1 a note is no longer retrieved; `compact` deletes it, expired superseded
notes (90 days), rows that fail their MAC, and the weakest beyond 300.

**Retrieval.** Learned notes are seeded by their own full-text query (6), joined by notes anchored
at the best code hits, weighted by effective confidence, and in turn lift the code they are about
(`about` edges, written when the note is). They render in a separate `<repo-memory>` section of at
most 400 estimated tokens after the `<repo-index>`; the index gets the rest, so a query with no
notes renders exactly what it did before notes existed. A note with no anchors belongs to the whole
repository; an anchored one is shown only when something it is about is within the session's scope.

**Controls.** `learn::list`, `forget`, `approve` and `clear` back `elefante memory …`. The `*_async`
forms in `learn/service.rs` (`list_async`, `forget_async`, `approve_async`, `clear_async`,
`counts_async`, `load_by_ids_async`) run on the store's actors and advance its generation mirror and
cached note counts, which is what lets the daemon serve `memory.*` and report counts without a refresh;
`forget` returns `false` for an absent or non-learned note and `approve` for one that is not pending;
the daemon reports either as `UNKNOWN_NOTE`. `clear` removes learned notes only, never the code graph.

**Display.** `learn::display_safe(&str)` is the one definition of "safe to show": it removes Unicode
categories Cc, Cf, Zl and Zp (controls, bidirectional and zero-width formatting, line and paragraph
separators), turns tab and newline into a space and collapses whitespace. Ingest applies it to a
candidate's label, statement and quote *before* verification (stripping, not rejecting, so pasted
coloured output does not cost a legitimate note; the quote check ignores non-alphanumerics, so a
stripped quote still matches), the prompt-path escapes use it, and the CLI and daemon apply it to every
column they print or send. Notes stored before it existed are cleaned at display.

**Consent** (`consent.rs`). The file `memory-consent` beside `memory.key` holds a list of
`{ answer: "learn" | "off", extractor, stamp }` records, one per extractor, each stamp a keyed MAC over
its answer and extractor. `read_all` / `read_for` return only records that verify — a missing,
malformed, tampered or foreign-key record is indistinguishable from no answer, so it fails closed —
and `write` replaces the answer for one extractor, keeps the verified others, and is atomic and
`0600`. This crate stores the answer; whether
learning may run is the daemon's decision.

**Rebuilds are reportable.** `Store::reset_reason()` is `Some(Untrusted | SchemaVersion)` when opening
discarded and recreated the database, so the daemon can say "memory was reset" instead of silently
losing learned notes.

## Invariants

**Stable ids across edits.** The indexer upserts. A file node is updated in place and a symbol
keeps its id as long as its key does, so a learned edge into either survives re-indexing.

**Nothing secret enters the graph, by construction first and by scrubbing second.** A denied path
(`.env`, `*.pem`, `.ssh/**`, …) is never read, hashed, or named. A signature ends at the item's
body or initializer, string literals inside it are elided, TypeScript constants with non-function
values are skipped entirely, and every string passes `elefante_guard::Scrubber`. The tests read the
**raw bytes** of every file under `.elefante/`, WAL included, for the fixture secret.

**Edges are derived, and the trigger is persistent.** An import edge is a function of a file's raw
imports *and the set of paths in the repository*, so creating `foo.ts` can resolve an import in a
file that did not change. `meta.edges_dirty` is set in the same transaction as the change that
makes edges stale, so a crash cannot leave them stale with nothing to say so.

**Git runs without a shell, under a timeout and a size cap, and never trusts the repository.**
`-c core.fsmonitor=false`, `--no-ext-diff`, `--no-textconv`, `GIT_CEILING_DIRECTORIES` so a plain
directory is not mistaken for the repository above it. It runs only when `HEAD` has moved, and the
head is stored in the same transaction as the edges it produced.

**Heavy work never holds a database.** Walking, hashing and parsing happen on worker threads; a
write transaction is as long as the rows it changes. A reader thread with its own read-only
connection answers retrieval while the writer commits.

**Counters, not clocks.** Gates assert `parsed == 0`, `hashed == 1`, `git_log_runs == 0`, and
reader-actor call counts. Timing is reported once, not asserted anywhere.

## What it does not do

No call graph or identifier references, no languages beyond Rust, TypeScript/TSX and Markdown
headings, no cross-crate or `tsconfig` import resolution, no embeddings, no file watcher, no
model-written summary. `.js`/`.jsx` get file nodes only. Learned notes are not shared across
worktrees (each working tree keeps its own store), have no user-level graph, and are not re-linked
when an anchored file is renamed or deleted: the link goes with the file and the note stays,
searchable by its words.

## Testing

```sh
cargo test -p elefante-mem                                  # hermetic tests, no network
ELEFANTE_DOGFOOD=/path/to/a/copy cargo test -p elefante-mem --test dogfood -- --ignored --nocapture
```

The `dogfood` test indexes a real repository and prints edges, symbols and counts so a human can
judge whether they are plausible; it is not part of the gate. `tests/common/` holds a hermetic git
fixture (empty global config, throwaway `HOME`, fixed identities and dates) and a `Tree`/`Live`
harness — the same discipline that `MILESTONES.md` records learning three times: a test that
passes only because the developer's machine supplied something is a test that fails in CI.

## Boundaries

`elefante-mem` depends on `elefante-guard` and nothing above it. `tools` does not depend on this
crate: `memory_search` is a trait defined in `tools` and implemented in `core` over this crate.
Retrieval results are placed in a request by `core`, as an ephemeral `user` message — never
`system` — and never stored in history.
