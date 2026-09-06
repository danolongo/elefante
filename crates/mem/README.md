# elefante-mem

Graph memory for projects (M2) and for the user (M3). **Neither exists yet, and nothing in the
workspace depends on this crate.** It is a four-line scaffold whose current job is to hold a
place in the topology so the dependency edges do not have to be retrofitted later.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 4 | One doc comment and one const. That is the entire crate. |

No `tests/` directory, because there is nothing to test.

## Entry points

One, and it is a string:

```rust
pub const NOT_AVAILABLE: &str = "memory search is not available until milestone M2";
```

**It has no callers.** `elefante-tools` implements the `memory_search` refusal with its own
hardcoded literal (`crates/tools/src/lib.rs:375`), and the two have already drifted apart —
the const says *"memory search is not available until milestone M2"*, the tool returns
*"memory_search is unavailable until milestone M2"*. Wiring `tools` to this const would delete
the duplicate, but it would also add a `tools → mem` edge for one string, so the duplication
stands until M2 gives the crate real content. Noted here because a reader who assumes the const
is load-bearing will be wrong.

## Invariants

**The `elefante-guard` dependency is declared but unused, and that is deliberate.** It reserves
the edge for the M2 write path, where every value entering the graph must pass through
`Scrubber` before it is persisted — a memory store that durably records a leaked credential is
worse than a tool that prints one, because the tool's output scrolls away. Declaring the edge
now also fixes its direction while the graph is small: `mem → guard`, never the reverse.

## Testing

```sh
cargo test -p elefante-mem     # 0 tests
```

Nothing to run. The crate is covered by `cargo clippy --workspace` and `cargo check`, which is
all a const needs.

## Boundaries

Do not build M2 here early. `PLAN.md` specifies the graph store, indexer, retrieval, and cache
as a milestone with its own gate, and a partial version hidden behind this crate's API is
exactly the accepted debt the milestone rule exists to prevent.

If you are here because `memory_search` returned an error, that is the tool working as designed
— see `DEVELOPER_GUIDE.md` §14.
