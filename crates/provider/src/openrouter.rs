//! Raw HTTP `OpenRouter` chat-completions implementation: the crate's only transport.

use std::collections::{BTreeMap, btree_map::Entry};

use async_stream::try_stream;
use futures_util::{StreamExt, stream::BoxStream};
use reqwest::{Client, Url, header};
use serde_json::{Map, Value, json};

use crate::{
    Capabilities, ContentBlock, Event, Message, ModelId, Provider, ProviderError, ReasoningDetail,
    Request, Role, StopReason, ToolDefinition, Usage,
    sse::{SseDecoder, SseFrame},
};

// The trailing slash is load-bearing: `Url::join` replaces the last path
// segment when the base does not end in one, so omitting it resolves to
// `/api/chat/completions` and 404s. `with_base_url` normalises user input the
// same way.
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1/";
const DONE_SENTINEL: &str = "[DONE]";

/// Streaming client for the `OpenRouter` chat-completions API.
#[derive(Clone, Debug)]
pub struct OpenRouterProvider {
    client: Client,
    api_key: String,
    base_url: Url,
}

impl OpenRouterProvider {
    /// Creates a client using `OpenRouter`'s public API origin.
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time `DEFAULT_BASE_URL` constant is invalid.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: Url::parse(DEFAULT_BASE_URL).expect("default OpenRouter URL is valid"),
        }
    }

    /// Overrides the API origin. Intended for compatible gateways and hermetic tests.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Configuration`] when `base_url` is not a valid URL.
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Result<Self, ProviderError> {
        let mut parsed = Url::parse(base_url.as_ref())
            .map_err(|error| ProviderError::Configuration(error.to_string()))?;
        if !parsed.path().ends_with('/') {
            let path = format!("{}/", parsed.path());
            parsed.set_path(&path);
        }
        self.base_url = parsed;
        Ok(self)
    }

    fn completions_url(&self) -> Result<Url, ProviderError> {
        self.base_url
            .join("chat/completions")
            .map_err(|error| ProviderError::Configuration(error.to_string()))
    }

    fn request_body(&self, request: &Request) -> Value {
        let capabilities = self.capabilities(&request.model);
        let mut body = Map::new();
        body.insert("max_tokens".into(), json!(request.max_tokens));
        body.insert(
            "messages".into(),
            messages_json(request, capabilities.prompt_caching),
        );
        body.insert("model".into(), json!(request.model.as_str()));
        body.insert("stream".into(), Value::Bool(true));
        // Opt into usage accounting; without it the final chunk carries no token
        // counts and no `cost`, which M5 attribution depends on.
        body.insert("usage".into(), json!({ "include": true }));

        if !request.tools.is_empty() {
            body.insert("tools".into(), tools_json(&request.tools));
            // Confine routing to endpoints that honour every parameter sent.
            // Without this an endpoint may silently drop `tools`: the model
            // answers in prose, the loop finds no tool calls and stops after one
            // round, and the agent does nothing while appearing to work. Turning
            // that silent no-op into a routing failure is worth one field.
            body.insert("provider".into(), json!({ "require_parameters": true }));
        }
        if capabilities.effort
            && let Some(effort) = request.effort
        {
            body.insert("reasoning".into(), json!({ "effort": effort }));
        }
        if capabilities.structured_output
            && let Some(schema) = &request.response_schema
        {
            body.insert(
                "response_format".into(),
                json!({
                    "type": "json_schema",
                    "json_schema": { "name": schema.name, "strict": true, "schema": schema.schema },
                }),
            );
            // Without this an endpoint that does not implement structured outputs may ignore the
            // field and return prose, which the caller would then have to reject.
            body.insert("provider".into(), json!({ "require_parameters": true }));
        }
        Value::Object(body)
    }
}

impl Provider for OpenRouterProvider {
    fn capabilities(&self, model: &ModelId) -> Capabilities {
        let slug = model.as_str().to_ascii_lowercase();
        PROFILES
            .iter()
            .filter(|profile| slug.starts_with(profile.prefix))
            .max_by_key(|profile| profile.prefix.len())
            .map_or_else(unknown_model_capabilities, |profile| profile.capabilities)
    }

    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event, ProviderError>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url = self.completions_url();
        let body = self.request_body(&request);

        Box::pin(try_stream! {
            let url = url?;
            let response = client
                .post(url)
                .header(header::AUTHORIZATION, format!("Bearer {api_key}"))
                .header(header::ACCEPT, "text/event-stream")
                .json(&body)
                .send()
                .await?;

            let status = response.status();
            let response = if status.is_success() {
                response
            } else {
                let body = response.text().await.unwrap_or_else(|error| error.to_string());
                Err(ProviderError::HttpStatus {
                    status: status.as_u16(),
                    body: truncate_error_body(&body),
                })?
            };

            let mut decoder = SseDecoder::default();
            let mut parser = OpenRouterEventParser::default();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                for frame in decoder.push(&chunk?)? {
                    for event in parser.parse(&frame)? {
                        yield event;
                    }
                }
            }
            if let Some(frame) = decoder.finish()? {
                for event in parser.parse(&frame)? {
                    yield event;
                }
            }
            if parser.has_pending_tool_calls() {
                Err(ProviderError::Protocol(
                    "stream ended during a tool call".to_owned(),
                ))?;
            }
        })
    }
}

/// Caps an error body so a misrouted request cannot bury its own cause.
///
/// A wrong path returns an HTML error page, and printing tens of kilobytes of
/// markup hides the status line that actually explains the failure.
fn truncate_error_body(body: &str) -> String {
    const LIMIT: usize = 600;
    if body.len() <= LIMIT {
        return body.to_owned();
    }
    let mut end = LIMIT;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes total]", &body[..end], body.len())
}

/// Capability resolution for one model-slug prefix.
struct ModelProfile {
    prefix: &'static str,
    capabilities: Capabilities,
}

/// Capabilities of an upstream that supports prompt caching with hit metrics.
const fn upstream(reasoning: bool) -> Capabilities {
    Capabilities {
        adaptive_thinking: reasoning,
        effort: reasoning,
        prompt_caching: true,
        mid_conversation_system: true,
        structured_output: true,
        cache_metrics: true,
    }
}

/// An unrecognised slug receives nothing model-dependent.
///
/// `mid_conversation_system` is the one exception: it describes the
/// chat-completions message array, which accepts a `system` message at any
/// position regardless of which model is behind it.
const fn unknown_model_capabilities() -> Capabilities {
    Capabilities {
        adaptive_thinking: false,
        effort: false,
        prompt_caching: false,
        mid_conversation_system: true,
        structured_output: false,
        cache_metrics: false,
    }
}

/// Longest matching prefix wins, so a narrower slug can opt out of its family.
const PROFILES: &[ModelProfile] = &[
    ModelProfile {
        prefix: "anthropic/claude-",
        capabilities: upstream(true),
    },
    ModelProfile {
        prefix: "anthropic/claude-3",
        capabilities: upstream(false),
    },
    ModelProfile {
        prefix: "google/gemini-",
        capabilities: upstream(false),
    },
    ModelProfile {
        prefix: "google/gemini-2.5",
        capabilities: upstream(true),
    },
    ModelProfile {
        prefix: "google/gemini-3",
        capabilities: upstream(true),
    },
    ModelProfile {
        prefix: "openai/gpt-",
        capabilities: upstream(false),
    },
    ModelProfile {
        prefix: "openai/gpt-5",
        capabilities: upstream(true),
    },
    ModelProfile {
        prefix: "openai/o",
        capabilities: upstream(true),
    },
    // The auto router, which also covers `openrouter/auto-beta` by prefix.
    //
    // Not a guess: the catalogue entry for this slug declares `tools`,
    // `reasoning`, `reasoning_effort`, and `structured_outputs` itself. Caching
    // is the one axis left off, because auto's pricing carries no
    // `input_cache_read` — the routed model may or may not cache, and the
    // metadata says nothing either way.
    ModelProfile {
        prefix: "openrouter/auto",
        capabilities: Capabilities {
            adaptive_thinking: true,
            effort: true,
            prompt_caching: false,
            mid_conversation_system: true,
            structured_output: true,
            cache_metrics: false,
        },
    },
];

fn messages_json(request: &Request, cache: bool) -> Value {
    let mut messages = Vec::new();
    let last = request.system.len().saturating_sub(1);
    for (index, text) in request.system.iter().enumerate() {
        messages.push(system_message(text, cache, cache && index == last));
    }
    for message in &request.messages {
        push_message(&mut messages, message);
    }
    Value::Array(messages)
}

fn system_message(text: &str, structured: bool, breakpoint: bool) -> Value {
    if structured {
        let mut part = json!({ "text": text, "type": "text" });
        if breakpoint {
            part["cache_control"] = json!({ "type": "ephemeral" });
        }
        json!({ "content": [part], "role": "system" })
    } else {
        json!({ "content": text, "role": "system" })
    }
}

fn push_message(messages: &mut Vec<Value>, message: &Message) {
    match message.role {
        Role::System => messages.push(json!({
            "content": joined_text(&message.content),
            "role": "system",
        })),
        Role::Assistant => messages.push(assistant_message(&message.content)),
        Role::User => push_user_message(messages, &message.content),
    }
}

/// Fans tool results out into one `{"role":"tool"}` message each.
///
/// Anthropic returned every tool result in a single user message; OpenAI-shaped
/// APIs require one message per result. The batch is still never split across
/// turns: the tool messages are emitted consecutively, in the order of the
/// `tool_calls` that produced them, before anything else from this turn.
fn push_user_message(messages: &mut Vec<Value>, content: &[ContentBlock]) {
    let mut text = String::new();
    for block in content {
        match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                // OpenAI-shaped tool messages have no error flag, so the failure
                // has to survive in the content the model reads.
                let content = if *is_error {
                    format!("Error: {content}")
                } else {
                    content.clone()
                };
                messages.push(json!({
                    "content": content,
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                }));
            }
            ContentBlock::Text { text: fragment } => text.push_str(fragment),
            ContentBlock::Reasoning { .. } | ContentBlock::ToolUse { .. } => {}
        }
    }
    if !text.is_empty() {
        messages.push(json!({ "content": text, "role": "user" }));
    }
}

fn assistant_message(content: &[ContentBlock]) -> Value {
    let mut text = String::new();
    let mut reasoning_details = Vec::new();
    let mut tool_calls = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text: fragment } => text.push_str(fragment),
            ContentBlock::Reasoning { details } => {
                reasoning_details.extend(details.iter().map(|detail| detail.raw.clone()));
            }
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                "function": { "arguments": arguments_string(input), "name": name },
                "id": id,
                "type": "function",
            })),
            // A tool result never belongs to an assistant turn.
            ContentBlock::ToolResult { .. } => {}
        }
    }

    let mut message = Map::new();
    message.insert("content".into(), json!(text));
    if !reasoning_details.is_empty() {
        message.insert("reasoning_details".into(), Value::Array(reasoning_details));
    }
    message.insert("role".into(), json!("assistant"));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Value::Object(message)
}

fn joined_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn arguments_string(input: &Value) -> String {
    serde_json::to_string(input).unwrap_or_else(|_| "{}".to_owned())
}

fn tools_json(tools: &[ToolDefinition]) -> Value {
    let mut sorted: Vec<&ToolDefinition> = tools.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    Value::Array(
        sorted
            .into_iter()
            .map(|tool| {
                json!({
                    "function": {
                        "description": tool.description,
                        "name": tool.name,
                        "parameters": tool.input_schema,
                    },
                    "type": "function",
                })
            })
            .collect(),
    )
}

/// A `tool_calls` entry being assembled from fragments that share an array index.
#[derive(Debug)]
struct PendingToolCall {
    id: String,
    arguments: String,
}

/// Normalizes chat-completion chunks into [`Event`]s.
#[derive(Debug, Default)]
struct OpenRouterEventParser {
    /// Keyed by `tool_calls[].index`, which is how fragments identify their call.
    /// `BTreeMap` keeps completion order equal to call order.
    tool_calls: BTreeMap<u64, PendingToolCall>,
    stopped: bool,
}

impl OpenRouterEventParser {
    fn has_pending_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    fn parse(&mut self, frame: &SseFrame) -> Result<Vec<Event>, ProviderError> {
        if frame.data == DONE_SENTINEL {
            return Ok(Vec::new());
        }
        let payload: Value = serde_json::from_str(&frame.data)?;
        if let Some(error) = payload.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown OpenRouter stream error");
            return Err(ProviderError::Protocol(message.to_owned()));
        }

        let mut events = Vec::new();
        let choice = payload
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first());
        if let Some(choice) = choice
            && !self.stopped
        {
            self.push_delta_events(choice.get("delta"), &mut events)?;
        }
        if let Some(usage) = payload.get("usage") {
            let usage = usage_from(usage);
            if !usage.is_empty() {
                events.push(Event::Usage(usage));
            }
        }
        if let Some(reason) = choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            && !self.stopped
        {
            self.stopped = true;
            if reason == "content_filter" {
                // A refusal supersedes any incomplete tool call. Nothing from the
                // rejected partial call may enter replay history.
                self.tool_calls.clear();
            } else {
                events.extend(self.complete_tool_calls()?);
            }
            events.push(Event::Stop(stop_reason(reason)));
        }
        Ok(events)
    }

    fn push_delta_events(
        &mut self,
        delta: Option<&Value>,
        events: &mut Vec<Event>,
    ) -> Result<(), ProviderError> {
        let Some(delta) = delta else {
            return Ok(());
        };
        if let Some(text) = delta.get("reasoning").and_then(Value::as_str)
            && !text.is_empty()
        {
            events.push(Event::ReasoningDelta(text.to_owned()));
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array)
            && !details.is_empty()
        {
            // Forwarded verbatim and in arrival order; the loop concatenates
            // consecutive batches into one reasoning block per turn.
            events.push(Event::ReasoningDetails(
                details.iter().cloned().map(ReasoningDetail::new).collect(),
            ));
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            events.push(Event::TextDelta(text.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.push_tool_call_fragment(call, events)?;
            }
        }
        Ok(())
    }

    /// Accumulates one `tool_calls[]` fragment.
    ///
    /// Fragments are identified by array `index`, never by id: `id` and
    /// `function.name` arrive on the first fragment only, and
    /// `function.arguments` streams afterwards as string fragments to be
    /// concatenated.
    fn push_tool_call_fragment(
        &mut self,
        call: &Value,
        events: &mut Vec<Event>,
    ) -> Result<(), ProviderError> {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        if let Entry::Vacant(slot) = self.tool_calls.entry(index) {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if id.is_empty() || name.is_empty() {
                return Err(ProviderError::Protocol(format!(
                    "first tool call fragment at index {index} carried no id and name"
                )));
            }
            slot.insert(PendingToolCall {
                id: id.to_owned(),
                arguments: String::new(),
            });
            events.push(Event::ToolUseStart {
                id: id.to_owned(),
                name: name.to_owned(),
            });
        }
        if let Some(fragment) = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .filter(|fragment| !fragment.is_empty())
        {
            let pending = self
                .tool_calls
                .get_mut(&index)
                .expect("the fragment's index was just inserted");
            pending.arguments.push_str(fragment);
            events.push(Event::ToolUseInputDelta(fragment.to_owned()));
        }
        Ok(())
    }

    /// Parses every accumulated argument buffer, in call order.
    ///
    /// Malformed JSON is an error: a silently empty object would hand the loop a
    /// tool call the model never made.
    fn complete_tool_calls(&mut self) -> Result<Vec<Event>, ProviderError> {
        let mut events = Vec::new();
        for (_, pending) in std::mem::take(&mut self.tool_calls) {
            let arguments = pending.arguments.trim();
            let input = if arguments.is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(arguments)?
            };
            events.push(Event::ToolUseComplete {
                id: pending.id,
                input,
            });
        }
        Ok(events)
    }
}

fn usage_from(value: &Value) -> Usage {
    let count = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: count("/prompt_tokens"),
        output_tokens: count("/completion_tokens"),
        reasoning_tokens: count("/completion_tokens_details/reasoning_tokens"),
        cached_input_tokens: count("/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: count("/cache_write_tokens"),
        cost: value.get("cost").and_then(Value::as_f64),
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests;
