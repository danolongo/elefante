# elefante-rpc

The wire contract: JSON-RPC-shaped request and response types, notification envelopes, session
snapshots, and bounded NDJSON framing. Shared verbatim by the daemon and every client, which is
the point — one definition, so the two sides cannot disagree about the format.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 14 | Module declarations, glob re-exports, and the two capacity constants. |
| `src/framing.rs` | 124 | NDJSON encode/decode, `FrameDecoder`, preview truncation. |
| `src/notification.rs` | 197 | `Notification`, `NotificationEvent`, and `Sequence` with its coalescing rule. |
| `src/request.rs` | 207 | `Request`, `Method`, `Response`, and the ten error codes. |
| `src/snapshot.rs` | 68 | `AttachResult`, `SessionSnapshot`, `CompletedTurn`, `ActiveTurn`, `ToolStatus`. |

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

Truncation cuts on a UTF-8 character boundary, never mid-codepoint. `DEVELOPER_GUIDE.md` §5 is
the single source of truth for the protocol these types encode; this crate is where it is
spelled, not where it is explained.

## Testing

```sh
cargo test -p elefante-rpc     # 12 tests
```

`tests/wire.rs` (9) round-trips every request, response, notification, and snapshot shape.
`tests/framing.rs` (3) covers the byte-level cases that break naive line splitting — frames
split across chunk boundaries, oversized lines, escaped newlines inside strings. Nothing is
`#[ignore]`d; the crate is pure types and has no I/O to mock.

## Boundaries

Types only. No sockets, no sessions, no policy — `srv` owns the daemon and `cli` owns the
client, and both depend on this crate rather than the reverse.

A change here is a wire change. It lands on both sides at once or it lands broken.
