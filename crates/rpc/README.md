# elefante-rpc

The wire contract: JSON-RPC-shaped request and response types, notification envelopes, session
snapshots, and bounded NDJSON framing. Shared verbatim by the daemon and every client, which is
the point — one definition, so the two sides cannot disagree about the format.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 14 | Module declarations, glob re-exports, and the two capacity constants. |
| `src/framing.rs` | 124 | NDJSON encode/decode, `FrameDecoder`, preview truncation. |
| `src/notification.rs` | 352 | `Notification`, `NotificationEvent` (hand-written `Deserialize`, `Unknown`), and `Sequence` with its coalescing rule. |
| `src/request.rs` | 352 | `Request`, `Method`, `Response`, the twelve error codes, and the memory types: `MemoryStatus`, `MemorySeed`, `MemoryNote`, `MemoryRow`, `MemoryChange`, the consent and learning enums. |
| `src/snapshot.rs` | 76 | `AttachResult` (both kinds carry optional `memory`), `SessionSnapshot`, `CompletedTurn` and `ActiveTurn` (with `memory_note_ids`), `ToolStatus`. |

## Entry points

```rust
use elefante_rpc::{decode_frame, encode_frame, FrameDecoder, Notification, Request};

let bytes = encode_frame(&request)?;              // one JSON value + '\n'
let request: Request = decode_frame(&line)?;

let mut decoder = FrameDecoder::default();        // for a byte stream
for frame in decoder.push(&chunk)? { /* ... */ }
```

Constants worth knowing before writing daemon or client code:

| Constant | Value | Meaning |
|---|---:|---|
| `SESSION_RING_CAPACITY` | 4096 | Events retained per session for resume. |
| `CLIENT_QUEUE_CAPACITY` | 256 | Per-client delivery queue before coalescing. |
| `MAX_FRAME_BYTES` | 8 MiB | A longer line is a protocol error and closes the connection. |
| `PREVIEW_BYTES` | 2 KiB | Tool preview cap; `truncate_preview` applies `TRUNCATION_MARKER`. |

## Invariants

**This crate depends on `elefante-provider` solely to re-export `StopReason` and `Usage`.**
That edge exists so the wire and the agent loop cannot drift apart: if `StopReason` gains a
variant, the wire gains it in the same commit, because it is literally the same type. The cost
is that `rpc` is not a true leaf — worth it, and worth knowing before anyone tries to remove the
dependency to "clean up" the graph.

**A `Notification` carries top-level `session_id`, `seq`, and `turn_id`, which strict JSON-RPC
2.0 does not permit.** The layout is JSON-RPC-*shaped*, not conformant, so an off-the-shelf
library will not round-trip it. Both sides hand-roll the envelope deliberately; claiming
conformance and then extending it is how two implementations silently diverge.

**`Sequence` is `Single` or `Coalesced { seq_from, seq_to }`, and `coalesce` only merges
same-method deltas.** The range is what makes a merge distinguishable from a gap — a merged
event carrying only its highest `seq` would trip the client's own resynchronise rule during
normal operation. `coalesce` returns `bool` so the caller knows whether the merge happened.

**`NotificationEvent` accepts a method it does not know.** `Deserialize` is written by hand because
`#[serde(other)]` does not work on this adjacently-tagged enum for events with `params`: it reads the
`method`, deserialises the known ones, and returns `Unknown { method }` for the rest, provided `params`
is an object. `Unknown` is never serialised. A new event therefore needs no version field and shares
the existing `seq` space; only an old client attached to a daemon replaced under it could still
choke, which a normal upgrade cannot produce. The memory events (`memory.status`, `memory.learned`,
`memory.context`) are known events and, like every non-delta, are never coalesced.

**The memory fields are additive and optional.** `CreateResult.memory` and both `AttachResult`
variants' `memory` are `Option<MemoryStatus>` skipped when absent; `memory_note_ids` is skipped when
empty. Nothing in the crate denies unknown fields, so an older peer ignores them.

Truncation cuts on a UTF-8 character boundary, never mid-codepoint. `DEVELOPER_GUIDE.md` §5 is
the single source of truth for the protocol these types encode; this crate is where it is
spelled, not where it is explained.

## Testing

```sh
cargo test -p elefante-rpc     # 13 tests
```

`tests/wire.rs` (10) round-trips every request, response, notification, and snapshot shape, including the
memory methods, events and results, and checks that an unknown method parses but cannot be serialised.
`tests/framing.rs` (3) covers the byte-level cases that break naive line splitting — frames
split across chunk boundaries, oversized lines, escaped newlines inside strings. Nothing is
`#[ignore]`d; the crate is pure types and has no I/O to mock.

## Boundaries

Types only. No sockets, no sessions, no policy — `srv` owns the daemon and `cli` owns the
client, and both depend on this crate rather than the reverse.

A change here is a wire change. It lands on both sides at once or it lands broken.
