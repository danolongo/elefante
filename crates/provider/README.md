# elefante-provider

Provider-neutral request and streaming response types, plus the one transport that implements
them. Wire formats stay behind the `Provider` trait, so the agent loop understands exactly one
message model and one event stream no matter what is on the other end.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 305 | The `Provider` trait and every normalized type: `Request`, `Message`, `ContentBlock`, `Event`, `Usage`, `StopReason`, `Capabilities`. |
| `src/sse.rs` | 103 | Transport-generic Server-Sent Events decoder. Knows nothing about any provider. |
| `src/openrouter.rs` | 598 | The crate's only transport: `POST {base}/chat/completions`, driving the decoder and mapping wire chunks back to normalized events. |
| `src/openrouter/tests.rs` | 878 | Tests for that transport — larger than the transport itself, and the only `src/<module>/` subdirectory in the workspace. |

The split matters: `sse.rs` handles what byte-oriented HTTP streams get wrong — frames split
across chunk boundaries, mixed CRLF and LF, multi-line `data:` payloads, comment keep-alives —
and it does so without knowing a single OpenRouter field name.

## Entry points

```rust
use elefante_provider::{Effort, Message, ModelId, OpenRouterProvider, Provider, Request, Role};

let provider = OpenRouterProvider::new(api_key);          // or ::with_base_url(url)
let caps = provider.capabilities(&ModelId::new("anthropic/claude-sonnet-5"));

let mut request = Request::new("anthropic/claude-sonnet-5", 2048,
    vec![Message::text(Role::User, "Say ready.")]);
request.effort = Some(Effort::Low);

let mut events = provider.stream(request);                // BoxStream<Result<Event, _>>
```

`Provider` is deliberately two methods — `capabilities` and `stream`. It is object-safe and
`Send + Sync`, so `core` holds it behind a trait object and the daemon can swap in a stub.

## Invariants

**Reasoning blocks must be replayed unmodified and unreordered.** `ReasoningDetail` therefore
stores the complete original object verbatim in `raw` and treats `kind` as a read-only
discriminator. A typed enum was rejected deliberately: an unknown future `reasoning.*` kind
would fail to round-trip, which violates the replay rule *silently* instead of erroring. If you
are tempted to parse these into structured variants, that is the reason not to.

**Tool-call fragments accumulate in a `BTreeMap` keyed by the wire's array index.** Streaming
deltas arrive interleaved and out of order; keying by index and draining in key order is what
makes completion order equal call order. A `HashMap` would lose that, and the loss would only
show up on multi-tool turns.

**`provider: { require_parameters: true }` is set on every request that carries tools.**
Without it OpenRouter silently drops parameters a model does not declare, so a routing choice
becomes a missing `tools` array and the model answers in prose instead of calling anything.
Failing the request is the better outcome.

**`DEFAULT_BASE_URL` ends in a slash.** `Url::join` replaces the final path segment otherwise,
so `.../api/v1` + `chat/completions` resolves to `/api/chat/completions` and 404s. Every
hermetic test uses `with_base_url()`, which normalizes — so only the credentialed test could
catch this, and it did. Two offline regression tests now cover it.

Capabilities resolve by prefix-matching the model slug, defaulting to all-false for unrecognized
slugs: an unknown model must not silently receive fields it cannot handle. `DEVELOPER_GUIDE.md`
§7 covers the wire mapping in full.

## Testing

```sh
cargo test -p elefante-provider                          # 23 hermetic
cargo test -p elefante-provider -- --ignored --nocapture  # 1 credentialed, billable
```

24 tests total. `src/openrouter/tests.rs` holds 21 and drives a loopback mock HTTP server, which
is why `tokio`'s `net` feature is a dev-dependency. `tests/sse.rs` holds 3 covering the decoder
in isolation.

One test is `#[ignore]`d — `live_second_identical_request_reads_prompt_cache` needs real
credentials, network, and billable requests. It bounds itself to 64 output tokens and runs in CI
through `.github/workflows/credentialed.yml`, not on push.

## Boundaries

Normalized types live here; provider-specific field names do not leave `openrouter.rs`. If
`core` ever needs to know an OpenRouter spelling, the normalization is incomplete and the fix
belongs in this crate.

Resist adding a second transport. The `Provider` trait exists to keep wire types out of `core`,
not to host implementations — two transports would double the surface of a harness whose value
is elsewhere. Prefer a different model slug.
