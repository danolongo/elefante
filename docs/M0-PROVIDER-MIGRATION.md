# M0 patch — single provider: OpenRouter

Reverts the two-provider approach. `elefante-provider` exposes exactly one transport.
This document is the **frozen contract** for that change; work in parallel against it.

Scope: `elefante-provider` (rewrite), `elefante-core` + `elefante-cli` (adapt),
`elefante-guard` (one scrub pattern), `docs/` (references). Nothing else moves.

## Why one provider

Two impls doubled the transport surface of a harness whose value is elsewhere — memory,
guard, critics. OpenRouter reaches Claude anyway (`anthropic/claude-opus-5`), so this
narrows the *code*, not the *models*. Adversarial review still runs on a different vendor's
model; it just no longer needs a second integration to do it.

**What is actually given up**, stated plainly so nobody rediscovers it later:

- Anthropic-native request shapes — `output_config.effort`, `thinking.display`,
  `cache_control` placement semantics for non-Anthropic upstreams.
- A proxy hop on every request.
- Cache behaviour becomes model-dependent rather than guaranteed.

**What survives intact:** reasoning control, reasoning replay, prompt caching with real
hit metrics, structured outputs, parallel tool calls. All verified against current
OpenRouter docs before this was written.

---

## Current state (measured, not assumed)

39 hermetic tests green, 1 ignored (credentialed). `run_agent_loop` already takes
`&dyn Provider`, so core has no transport coupling. Blast radius:

| File | Change |
|---|---|
| `provider/src/anthropic.rs` (1279 L) | **delete**; salvage the SSE decoder |
| `provider/src/lib.rs` (242 L) | reasoning types, `Usage` fields, `Event` variants |
| `core/src/lib.rs` | 6 sites: lines 43, 108–116, 302–308 |
| `core/tests/loop.rs` | 5 sites: lines 35, 62–64, 119, 144–147 |
| `cli/src/main.rs` | env var names, provider construction (lines 8, 11, 25, 119–124) |
| `guard/src/scrub.rs` | **add** `sk-or-v1-`; **keep** `sk-ant-` |

`sk-ant-` stays because it is a *secret* pattern, not a provider dependency — Anthropic
keys still leak from repos elefante edits regardless of what elefante calls.

---

## Frozen type contract

Everything below is settled. Implement to it; do not renegotiate it mid-flight.

### Reasoning — replaces `Thinking` / `RedactedThinking`

OpenRouter's `reasoning_details` is a superset of what we had. Its replay rule is strict:
*the sequence of reasoning blocks must be returned unmodified and unreordered.* Lossless
round-trip therefore outranks ergonomic access, so we keep the original object verbatim:

```rust
/// One reasoning block, preserved exactly as the model produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasoningDetail {
    /// Wire discriminator: "reasoning.text" | "reasoning.summary" | "reasoning.encrypted".
    pub kind: String,
    /// The complete original object. Replayed unmodified; never edited or reordered.
    pub raw: serde_json::Value,
}

impl ReasoningDetail {
    /// Human-visible text, if this block carries any. Reads `.text` or `.summary`;
    /// encrypted blocks return None.
    #[must_use]
    pub fn display_text(&self) -> Option<&str>;
}
```

A typed enum was rejected: unknown future `reasoning.*` types would fail to round-trip,
which silently violates the replay rule rather than erroring.

```rust
pub enum ContentBlock {
    Text { text: String },
    /// Ordering within an assistant turn is significant.
    Reasoning { details: Vec<ReasoningDetail> },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}
```

### Events

```rust
pub enum Event {
    ReasoningDelta(String),                  // display fragments; renderable
    ReasoningDetails(Vec<ReasoningDetail>),  // replay state; frontends must NOT render
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta(String),
    ToolUseComplete { id: String, input: Value },
    Usage(Usage),
    Stop(StopReason),
}
```

`ThinkingDelta` → `ReasoningDelta`. `ThinkingBlockComplete` and
`RedactedThinkingBlockComplete` collapse into `ReasoningDetails`.

### Usage

```rust
pub struct Usage {
    pub input_tokens: u64,          // prompt_tokens
    pub output_tokens: u64,         // completion_tokens
    pub reasoning_tokens: u64,      // completion_tokens_details.reasoning_tokens
    pub cached_input_tokens: u64,   // prompt_tokens_details.cached_tokens  ← cache-hit signal
    pub cache_write_tokens: u64,    // cache_write_tokens
    pub cost: Option<f64>,          // cost — real dollars, straight from the provider
}
```

`cost` is a genuine upgrade: M5 cost attribution stops being a price-table calculation.

### Stop reasons

`stop` → `EndTurn` · `tool_calls` → `ToolUse` · `length` → `MaxTokens` ·
`content_filter` → `Refusal` · anything else → `Other(String)`.

### Effort

`Effort` is unchanged and maps 1:1 onto `reasoning.effort`:
`Low|Medium|High|XHigh|Max` → `"low"|"medium"|"high"|"xhigh"|"max"`.

### Capabilities

Kept, and now resolved **per model** — within OpenRouter, model variance is larger than
the old cross-provider variance was, so this matters more than before, not less.

```rust
fn capabilities(&self, model: &ModelId) -> Capabilities
```

All six existing fields are kept. `adaptive_thinking`/`effort` follow reasoning support;
`prompt_caching`/`cache_metrics` follow upstream (`anthropic/*`, `google/*`, `openai/*`);
`structured_output` follows `response_format: json_schema` support; `mid_conversation_system`
is `true` — OpenAI-shaped APIs accept a `{"role":"system"}` message anywhere in the array —
and gains its consumer in M2/M3 memory placement. Resolution is prefix matching on the model
slug, with a conservative all-false default for unrecognised slugs: an unknown model must not
silently receive fields it cannot handle.

### API surface the rest of the workspace depends on

```rust
pub use openrouter::OpenRouterProvider;   // re-exported at the crate root

impl OpenRouterProvider {
    pub fn new(api_key: impl Into<String>) -> Self;
    pub fn with_base_url(self, base_url: impl AsRef<str>) -> Result<Self, ProviderError>;
}
```

`ReasoningDetail` derives `Clone, Debug, PartialEq, Serialize, Deserialize`. `PartialEq` is
load-bearing: a core test asserts whole-vector equality across a tool continuation, which is
the strongest guarantee available on the replay rule.

`elefante-core` merges consecutive `ReasoningDetails` events into one contiguous
`ContentBlock::Reasoning` per assistant turn, appending in arrival order and never reordering.
The request mapper may therefore rely on a turn's reasoning being a single block, and must
emit its details in original order.

---

## Request mapping

`POST {base}/chat/completions`, `Authorization: Bearer $OPENROUTER_API_KEY`, `stream: true`.

- `system: Vec<String>` → leading `{role:"system"}` messages.
- `tools` → `[{type:"function", function:{name, description, parameters}}]`, name-sorted
  with stable key order (cache stability — the existing Anthropic test for this carries over).
- `effort` → `reasoning: {effort: ...}`, **omitted entirely** when the model does not declare it.
- Assistant `ToolUse` → `tool_calls[{id, type:"function", function:{name, arguments}}]`,
  `arguments` serialized to a string.
- Assistant `Reasoning` → `reasoning_details` on the assistant message, verbatim.
- `cache_control` breakpoint on the **final system block only**, and only when
  `prompt_caching` holds.

  This is one breakpoint where the deleted Anthropic impl used two — it also marked the
  final tool definition. The narrowing is deliberate and loses no coverage: an OpenAI-shaped
  `tools` array has no `cache_control` slot, and the cached prefix is ordered
  `tools → system → messages`, so a breakpoint at the end of system already covers the tool
  definitions sitting in front of it. This is exactly why name-sorted tools with stable key
  order still matter — the tool list remains inside the cached prefix even though it no
  longer carries a breakpoint of its own. Do not "restore" the second breakpoint.

### One real semantic difference — do not paper over it

Anthropic returns every tool result in **one** user message. OpenAI-shaped APIs require
**one `{role:"tool"}` message per result**. So `ContentBlock::ToolResult` fans out into
consecutive tool messages, in the same order as the `tool_calls` that produced them.

The plan's rule was "all results in a single user message"; the *intent* behind it —
never split a result batch across turns, because that trains the model out of parallel
tool calls — is preserved by emitting all tool messages consecutively before the next
assistant turn. `PLAN.md` is amended to say this. A test must assert the ordering.

---

## Streaming

Reuse the salvaged SSE decoder (frame splitting across chunk boundaries, CRLF/LF,
multi-line `data:`, comments) — it is transport-generic and already well tested.

OpenRouter specifics on top:

- Terminal sentinel is `data: [DONE]`.
- OpenRouter emits `: OPENROUTER PROCESSING` comment keep-alives. **These must be ignored,
  not treated as frames** — this is the most likely source of a spurious protocol error.
- `delta.tool_calls[]` accumulate **by `index`**, not by id: `id` and `function.name`
  arrive on the first fragment only, `function.arguments` streams as string fragments.
  This differs from Anthropic's block-index model and is the second likely bug source.
- `usage` arrives on the final chunk before `[DONE]`.
- `finish_reason` arrives on the chunk before that.
- Malformed accumulated `arguments` JSON is an **error**, never a silent `{}`.

---

## Test plan

Preserve every behaviour the current suite proves; retarget the transport.

**Carry over** (rewrite fixtures, keep the assertion): SSE frames split mid-boundary ·
mixed line endings · unknown future event types ignored · malformed tool JSON errors ·
request has sorted tools and stable key order · unsupported model receives no reasoning
field · standing prompt stays small.

**New:**
- `[DONE]` sentinel and `: OPENROUTER PROCESSING` keep-alives handled.
- `tool_calls` accumulate correctly by index across fragments, with `id`/`name` on the
  first fragment only.
- Multi-tool round trip: two parallel `tool_calls` produce two consecutive `role:"tool"`
  messages in call order.
- `reasoning_details` round-trip byte-identical through a tool continuation.
- `usage.prompt_tokens_details.cached_tokens > 0` parses into `cached_input_tokens`
  (hermetic fixture — the cache *gate* stays credentialed and separate).
- Unknown model slug yields all-false capabilities and a request carrying no reasoning field.

**Environment:** `OPENROUTER_API_KEY` (required), `OPENROUTER_MODEL`
(default `anthropic/claude-sonnet-5`), `OPENROUTER_BASE_URL`
(default `https://openrouter.ai/api/v1`), `ELEFANTE_EFFORT`.

## Decisions made during implementation

Recorded here because each is a real semantic choice, not a detail:

- **`usage: {"include": true}` is sent on every request.** Without it the stream carries no
  `cost` and no `prompt_tokens_details`, so `cached_input_tokens` and the whole M5 cost
  upgrade would silently read zero. This was missing from the original mapping.
- **`Usage` arrives after `Stop`.** The loop therefore drains every stream to completion
  rather than returning at the stop event, and `RunOutcome.usage` is a sum across rounds.
  A reader that breaks at `Stop` loses all token and cost data with no error — there is a
  regression test for exactly this.
- **`ToolResult::is_error` has no OpenAI-shape representation.** A `{"role":"tool"}` message
  carries no error flag, so failing results are prefixed `Error: ` in their content rather
  than dropping the signal. This is a small mutation of tool output and is tested.
- **`ReasoningDelta` comes only from `delta.reasoning`.** A model that emits
  `reasoning_details` without a flat `reasoning` string produces replay state but no
  renderable stream. Deriving deltas from `display_text()` was rejected because it risks
  double-rendering when both fields are present. **M1 must handle a turn that reasons
  visibly for a long time while emitting nothing renderable** — otherwise the TUI shows a
  silent pause, which is the exact failure the old `thinking.display` note warned about.
- **`StopReason::StopSequence` is retained** though unreachable through this mapping;
  removing a variant would break core's exhaustive matches for no gain.
- **`Usage` keeps `Copy` but loses `Eq`** — it now holds an `Option<f64>`.

## Definition of done

`cargo test --workspace` green with no fewer tests than the 39 that pass today, `cargo
clippy --workspace --all-targets` clean under the workspace's `pedantic` lint config, no
`anthropic` identifier outside `guard/src/scrub.rs`, and `docs/` free of stale
Anthropic-transport instructions.
