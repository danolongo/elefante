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

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
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
        }
        if capabilities.effort {
            if let Some(effort) = request.effort {
                body.insert("reasoning".into(), json!({ "effort": effort }));
            }
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
                    body,
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
        if let Some(choice) = choice {
            if !self.stopped {
                self.push_delta_events(choice.get("delta"), &mut events)?;
            }
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
        {
            if !self.stopped {
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
        if let Some(text) = delta.get("reasoning").and_then(Value::as_str) {
            if !text.is_empty() {
                events.push(Event::ReasoningDelta(text.to_owned()));
            }
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            if !details.is_empty() {
                // Forwarded verbatim and in arrival order; the loop concatenates
                // consecutive batches into one reasoning block per turn.
                events.push(Event::ReasoningDetails(
                    details.iter().cloned().map(ReasoningDetail::new).collect(),
                ));
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                events.push(Event::TextDelta(text.to_owned()));
            }
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
mod tests {
    use std::{
        fmt::Write as _,
        io::{Read, Write as _},
        net::{TcpListener, TcpStream},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use futures_util::StreamExt;

    use super::*;
    use crate::Effort;

    #[test]
    fn request_is_cached_sorted_and_carries_reasoning_effort() {
        let provider = OpenRouterProvider::new("test-key");
        let request = Request {
            model: ModelId::from("anthropic/claude-opus-5"),
            max_tokens: 2048,
            system: vec!["shared prefix".to_owned(), "stable system".to_owned()],
            tools: vec![
                ToolDefinition::new("write", "write", json!({ "type": "object" })),
                ToolDefinition::new("read", "read", json!({ "type": "object" })),
            ],
            messages: vec![Message::text(Role::User, "hello")],
            effort: Some(Effort::XHigh),
        };
        let body = provider.request_body(&request);

        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["stream"], true);
        assert_eq!(body["usage"]["include"], true);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["tools"][1]["function"]["name"], "write");
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(body["messages"][2]["content"], "hello");

        // Key order is stable, so an identical request is byte-identical on the wire.
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            serde_json::to_string(&provider.request_body(&request)).unwrap()
        );
    }

    #[test]
    fn unknown_model_slug_has_no_model_capabilities_and_no_reasoning_field() {
        let provider = OpenRouterProvider::new("test-key");
        let model = ModelId::from("some-vendor/experimental-42b");
        let capabilities = provider.capabilities(&model);

        assert!(!capabilities.adaptive_thinking);
        assert!(!capabilities.effort);
        assert!(!capabilities.prompt_caching);
        assert!(!capabilities.structured_output);
        assert!(!capabilities.cache_metrics);
        // Independent of the model: the chat-completions array takes a system
        // message at any position.
        assert!(capabilities.mid_conversation_system);

        let mut request = Request::new(model, 128, vec![Message::text(Role::User, "hello")]);
        request.system = vec!["stable system".to_owned()];
        request.effort = Some(Effort::High);
        let body = provider.request_body(&request);

        assert!(body.get("reasoning").is_none());
        // No caching either, so the system block stays a plain string.
        assert_eq!(body["messages"][0]["content"], "stable system");
    }

    #[test]
    fn non_reasoning_model_keeps_caching_but_receives_no_reasoning_field() {
        let provider = OpenRouterProvider::new("test-key");
        let model = ModelId::from("anthropic/claude-3-5-haiku");
        let capabilities = provider.capabilities(&model);
        assert!(!capabilities.effort);
        assert!(capabilities.prompt_caching);

        let mut request = Request::new(model, 128, vec![Message::text(Role::User, "hello")]);
        request.effort = Some(Effort::High);
        assert!(provider.request_body(&request).get("reasoning").is_none());
    }

    #[test]
    fn tool_results_fan_out_into_consecutive_tool_messages() {
        let provider = OpenRouterProvider::new("test-key");
        let request = Request::new(
            "anthropic/claude-opus-5",
            256,
            vec![
                Message::text(Role::User, "inspect the readme"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::ToolUse {
                            id: "call-1".to_owned(),
                            name: "read".to_owned(),
                            input: json!({ "path": "README.md" }),
                        },
                        ContentBlock::ToolUse {
                            id: "call-2".to_owned(),
                            name: "grep".to_owned(),
                            input: json!({ "pattern": "todo" }),
                        },
                    ],
                ),
                Message::new(
                    Role::User,
                    vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call-1".to_owned(),
                            content: "contents".to_owned(),
                            is_error: false,
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "call-2".to_owned(),
                            content: "no such file".to_owned(),
                            is_error: true,
                        },
                        ContentBlock::Text {
                            text: "carry on".to_owned(),
                        },
                    ],
                ),
            ],
        );
        let body = provider.request_body(&request);
        let messages = body["messages"].as_array().unwrap();

        assert_eq!(messages.len(), 5);
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"README.md\"}"
        );
        assert_eq!(messages[1]["tool_calls"][1]["id"], "call-2");
        // Both results land consecutively, in the order of the calls that made them.
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-1");
        assert_eq!(messages[2]["content"], "contents");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-2");
        assert_eq!(messages[3]["content"], "Error: no such file");
        assert_eq!(messages[4]["role"], "user");
        assert_eq!(messages[4]["content"], "carry on");
    }

    #[test]
    fn parser_ignores_the_done_sentinel_and_keep_alive_comments() {
        let mut decoder = SseDecoder::default();
        let mut parser = OpenRouterEventParser::default();
        let body = concat!(
            ": OPENROUTER PROCESSING\n\n",
            ": OPENROUTER PROCESSING\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut events = Vec::new();
        for frame in decoder.push(body.as_bytes()).unwrap() {
            events.extend(parser.parse(&frame).unwrap());
        }

        assert_eq!(
            events,
            vec![
                Event::TextDelta("hi".to_owned()),
                Event::Stop(StopReason::EndTurn),
            ]
        );
        assert!(decoder.finish().unwrap().is_none());
    }

    #[test]
    fn parser_accumulates_tool_calls_by_index() {
        let mut parser = OpenRouterEventParser::default();
        let events = parse_all(
            &mut parser,
            &[
                // `id` and `name` arrive on the first fragment of each index only.
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"call-1","type":"function","function":{"name":"read","arguments":""}},
                    {"index":1,"id":"call-2","type":"function","function":{"name":"grep","arguments":"{\"pattern\""}}
                ]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":"{\"path\":\"a\\\"b\","}}
                ]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":1,"function":{"arguments":":\"todo\"}"}},
                    {"index":0,"function":{"arguments":"\"body\":\"line\\n2\"}"}}
                ]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );

        assert_eq!(
            events[0],
            Event::ToolUseStart {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
            }
        );
        assert_eq!(
            events[1],
            Event::ToolUseStart {
                id: "call-2".to_owned(),
                name: "grep".to_owned(),
            }
        );
        let completions: Vec<&Event> = events
            .iter()
            .filter(|event| matches!(event, Event::ToolUseComplete { .. }))
            .collect();
        assert_eq!(
            completions,
            vec![
                &Event::ToolUseComplete {
                    id: "call-1".to_owned(),
                    input: json!({ "path": "a\"b", "body": "line\n2" }),
                },
                &Event::ToolUseComplete {
                    id: "call-2".to_owned(),
                    input: json!({ "pattern": "todo" }),
                },
            ]
        );
        assert_eq!(events.last(), Some(&Event::Stop(StopReason::ToolUse)));
    }

    #[test]
    fn parser_rejects_a_tool_fragment_that_never_declared_id_and_name() {
        let mut parser = OpenRouterEventParser::default();
        let error = parser
            .parse(&frame(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"function":{"arguments":"{}"}}]}}]}"#,
            ))
            .unwrap_err();
        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn malformed_tool_arguments_are_an_error() {
        let mut parser = OpenRouterEventParser::default();
        parser
            .parse(&frame(
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{"}}]}}]}"#,
            ))
            .unwrap();
        let error = parser
            .parse(&frame(
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            ))
            .unwrap_err();
        // Never a silent `{}`: the loop would run a call the model never made.
        assert!(matches!(error, ProviderError::InvalidJson(_)));
    }

    #[test]
    fn parser_ignores_unknown_future_fields_and_chunks() {
        let mut parser = OpenRouterEventParser::default();
        let events = parse_all(
            &mut parser,
            &[
                r#"{"id":"gen-1","object":"chat.completion.chunk","created":1,"model":"anthropic/claude-opus-5"}"#,
                r#"{"choices":[{"index":0,"delta":{"role":"assistant","annotations":[{"future":true}]},"logprobs":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"native_finish_reason":"end_turn"}]}"#,
            ],
        );
        assert!(events.is_empty());
    }

    #[test]
    fn parser_maps_finish_reasons() {
        for (wire, expected) in [
            ("stop", StopReason::EndTurn),
            ("tool_calls", StopReason::ToolUse),
            ("length", StopReason::MaxTokens),
            ("content_filter", StopReason::Refusal),
            ("error", StopReason::Other("error".to_owned())),
        ] {
            let mut parser = OpenRouterEventParser::default();
            let events = parse_all(
                &mut parser,
                &[&format!(
                    r#"{{"choices":[{{"index":0,"delta":{{}},"finish_reason":"{wire}"}}]}}"#
                )],
            );
            assert_eq!(events, vec![Event::Stop(expected)]);
        }
    }

    #[test]
    fn a_refusal_discards_an_incomplete_tool_call() {
        let mut parser = OpenRouterEventParser::default();
        let events = parse_all(
            &mut parser,
            &[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]}"#,
            ],
        );
        assert_eq!(events.last(), Some(&Event::Stop(StopReason::Refusal)));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::ToolUseComplete { .. }))
        );
        assert!(!parser.has_pending_tool_calls());
    }

    #[test]
    fn usage_reports_cache_hits_reasoning_tokens_and_cost() {
        let mut parser = OpenRouterEventParser::default();
        let events = parse_all(
            &mut parser,
            &[r#"{"choices":[],"usage":{
                "prompt_tokens":1500,
                "completion_tokens":42,
                "prompt_tokens_details":{"cached_tokens":1200},
                "completion_tokens_details":{"reasoning_tokens":30},
                "cache_write_tokens":260,
                "cost":0.001234
            }}"#],
        );
        assert_eq!(
            events,
            vec![Event::Usage(Usage {
                input_tokens: 1500,
                output_tokens: 42,
                reasoning_tokens: 30,
                cached_input_tokens: 1200,
                cache_write_tokens: 260,
                cost: Some(0.001_234),
            })]
        );
    }

    #[test]
    fn parser_separates_reasoning_display_from_replay_state() {
        let mut parser = OpenRouterEventParser::default();
        let events = parse_all(
            &mut parser,
            &[r#"{"choices":[{"index":0,"delta":{
                "reasoning":"weighing options",
                "reasoning_details":[
                    {"type":"reasoning.text","text":"weighing options","format":"anthropic-claude-v1","index":0,"signature":"opaque-signature"},
                    {"type":"reasoning.encrypted","data":"opaque-redacted-data","index":1}
                ]}}]}"#],
        );

        assert_eq!(
            events[0],
            Event::ReasoningDelta("weighing options".to_owned())
        );
        let Event::ReasoningDetails(details) = &events[1] else {
            panic!("expected reasoning details, got {:?}", events[1]);
        };
        assert_eq!(details[0].kind, "reasoning.text");
        assert_eq!(details[0].display_text(), Some("weighing options"));
        assert_eq!(details[1].kind, "reasoning.encrypted");
        assert_eq!(details[1].display_text(), None);
    }

    #[tokio::test]
    async fn raw_http_stream_survives_split_frames_and_reports_cache_reads() {
        let first = text_response(0, 1);
        let second = text_response(121, 2);
        let (base_url, requests, server) =
            spawn_server(vec![split_every(&first, 7), split_every(&second, 5)]);
        let provider = OpenRouterProvider::new("secret-test-key")
            .with_base_url(&base_url)
            .unwrap();
        let mut request = Request::new(
            "anthropic/claude-opus-5",
            256,
            vec![Message::text(Role::User, "same prompt")],
        );
        request.system = vec!["stable prefix".to_owned()];
        request.effort = Some(Effort::High);

        let first_events = collect(provider.stream(request.clone())).await;
        let second_events = collect(provider.stream(request)).await;
        let first_request = requests.recv().unwrap();
        let second_request = requests.recv().unwrap();
        server.join().unwrap();

        assert_eq!(first_request.body, second_request.body);
        assert_eq!(first_request.path, "/chat/completions");
        assert_eq!(
            first_request.authorization.as_deref(),
            Some("Bearer secret-test-key")
        );
        assert!(
            first_events
                .iter()
                .any(|event| matches!(event, Event::ReasoningDelta(text) if text == "thinking-1"))
        );
        assert!(
            second_events
                .iter()
                .any(|event| matches!(event, Event::ReasoningDelta(text) if text == "thinking-2"))
        );
        assert!(second_events.iter().any(|event| {
            matches!(event, Event::Usage(usage) if usage.cached_input_tokens == 121 && usage.cost == Some(0.0012))
        }));
        // Usage arrives on the final chunk, after the stop.
        assert!(
            second_events
                .iter()
                .any(|event| matches!(event, Event::Stop(StopReason::EndTurn)))
        );
        assert!(matches!(second_events.last(), Some(Event::Usage(_))));
    }

    #[tokio::test]
    async fn tool_continuation_replays_reasoning_details_and_orders_tool_messages() {
        let first = tool_call_response();
        let second = text_response(87, 2);
        let (base_url, requests, server) =
            spawn_server(vec![split_every(&first, 3), split_every(&second, 11)]);
        let provider = OpenRouterProvider::new("secret-test-key")
            .with_base_url(&base_url)
            .unwrap();
        let mut request = Request::new(
            "anthropic/claude-opus-5",
            256,
            vec![Message::text(Role::User, "inspect the readme")],
        );
        request.tools = vec![
            ToolDefinition::new("read", "read a file", json!({ "type": "object" })),
            ToolDefinition::new("grep", "search files", json!({ "type": "object" })),
        ];
        request.effort = Some(Effort::High);

        let first_events = collect(provider.stream(request.clone())).await;

        // Mirrors the loop: consecutive reasoning batches become one block, in
        // arrival order, and tool calls follow in completion order.
        let mut reasoning = Vec::new();
        let mut tool_names = Vec::new();
        let mut assistant_content = Vec::new();
        for event in &first_events {
            match event {
                Event::ReasoningDetails(details) => reasoning.extend(details.clone()),
                Event::ToolUseStart { name, .. } => tool_names.push(name.clone()),
                Event::ToolUseComplete { id, input } => {
                    assistant_content.push(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: tool_names.remove(0),
                        input: input.clone(),
                    });
                }
                _ => {}
            }
        }
        assistant_content.insert(0, ContentBlock::Reasoning { details: reasoning });

        request
            .messages
            .push(Message::new(Role::Assistant, assistant_content));
        request.messages.push(Message::new(
            Role::User,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call-1".to_owned(),
                    content: "contents".to_owned(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call-2".to_owned(),
                    content: "3 matches".to_owned(),
                    is_error: false,
                },
            ],
        ));
        let _second_events = collect(provider.stream(request)).await;
        let _first_request = requests.recv().unwrap();
        let second_request = requests.recv().unwrap();
        server.join().unwrap();

        let body: Value = serde_json::from_slice(&second_request.body).unwrap();
        let messages = body["messages"].as_array().unwrap();

        // The reasoning blocks go back unmodified and unreordered.
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[1]["reasoning_details"],
            reasoning_details_fixture()
        );
        assert_eq!(
            messages[1]["tool_calls"][0]["function"],
            json!({ "arguments": "{\"path\":\"README.md\"}", "name": "read" })
        );
        assert_eq!(messages[1]["tool_calls"][1]["id"], "call-2");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-1");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-2");
    }

    /// Opt in with `OPENROUTER_API_KEY` and `OPENROUTER_MODEL`.
    #[tokio::test]
    #[ignore = "requires OpenRouter credentials, network, and billable requests"]
    async fn live_second_identical_request_reads_prompt_cache() {
        let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| "anthropic/claude-opus-5".to_owned());
        let provider = OpenRouterProvider::new(api_key);
        let stable = "This is stable provider integration test context. ".repeat(300);
        let mut request = Request::new(
            model,
            64,
            vec![Message::text(Role::User, "Reply with the word ready.")],
        );
        request.system = vec![stable];
        request.effort = Some(Effort::Low);

        let _first = collect(provider.stream(request.clone())).await;
        let second = collect(provider.stream(request)).await;
        let cache_reads: u64 = second
            .iter()
            .filter_map(|event| match event {
                Event::Usage(usage) => Some(usage.cached_input_tokens),
                _ => None,
            })
            .sum();
        assert!(cache_reads > 0, "second request did not read prompt cache");
    }

    fn frame(data: &str) -> SseFrame {
        SseFrame {
            event: None,
            data: data.to_owned(),
        }
    }

    fn parse_all(parser: &mut OpenRouterEventParser, chunks: &[&str]) -> Vec<Event> {
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.parse(&frame(chunk)).unwrap());
        }
        events
    }

    async fn collect(mut stream: BoxStream<'static, Result<Event, ProviderError>>) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    }

    fn reasoning_details_fixture() -> Value {
        json!([
            {
                "type": "reasoning.text",
                "text": "safe summary",
                "format": "anthropic-claude-v1",
                "index": 0,
                "signature": "opaque-signature",
            },
            {
                "type": "reasoning.encrypted",
                "data": "opaque-redacted-data",
                "format": "anthropic-claude-v1",
                "index": 1,
            },
        ])
    }

    fn sse_body(chunks: &[Value]) -> Vec<u8> {
        let mut response = String::new();
        for chunk in chunks {
            // Keep-alives arrive between real chunks and must not become frames.
            response.push_str(": OPENROUTER PROCESSING\n\n");
            write!(response, "data: {chunk}\n\n").unwrap();
        }
        response.push_str("data: [DONE]\n\n");
        response.into_bytes()
    }

    fn text_response(cached: u64, turn: u8) -> Vec<u8> {
        sse_body(&[
            json!({
                "id": "gen-1",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "reasoning": format!("thinking-{turn}"),
                        "reasoning_details": [{
                            "type": "reasoning.text",
                            "text": format!("thinking-{turn}"),
                            "index": 0,
                            "signature": format!("signature-{turn}"),
                        }],
                    },
                    "finish_reason": null,
                }],
            }),
            json!({
                "id": "gen-1",
                "choices": [{ "index": 0, "delta": { "content": "ready" }, "finish_reason": null }],
            }),
            json!({
                "id": "gen-1",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            }),
            json!({
                "id": "gen-1",
                "choices": [],
                "usage": {
                    "prompt_tokens": 150,
                    "completion_tokens": 3,
                    "prompt_tokens_details": { "cached_tokens": cached },
                    "completion_tokens_details": { "reasoning_tokens": 7 },
                    "cost": 0.0012,
                },
            }),
        ])
    }

    fn tool_call_response() -> Vec<u8> {
        let details = reasoning_details_fixture();
        sse_body(&[
            json!({
                "id": "gen-2",
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant", "reasoning": "safe summary", "reasoning_details": [details[0]] },
                    "finish_reason": null,
                }],
            }),
            json!({
                "id": "gen-2",
                "choices": [{
                    "index": 0,
                    "delta": { "reasoning_details": [details[1]] },
                    "finish_reason": null,
                }],
            }),
            json!({
                "id": "gen-2",
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [
                        { "index": 0, "id": "call-1", "type": "function", "function": { "name": "read", "arguments": "{\"path\":" } },
                        { "index": 1, "id": "call-2", "type": "function", "function": { "name": "grep", "arguments": "" } },
                    ]},
                    "finish_reason": null,
                }],
            }),
            json!({
                "id": "gen-2",
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [
                        { "index": 1, "function": { "arguments": "{\"pattern\":\"todo\"}" } },
                        { "index": 0, "function": { "arguments": "\"README.md\"}" } },
                    ]},
                    "finish_reason": null,
                }],
            }),
            json!({
                "id": "gen-2",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
            }),
            json!({
                "id": "gen-2",
                "choices": [],
                "usage": { "prompt_tokens": 100, "completion_tokens": 15, "cost": 0.0004 },
            }),
        ])
    }

    fn split_every(bytes: &[u8], width: usize) -> Vec<Vec<u8>> {
        bytes.chunks(width).map(<[u8]>::to_vec).collect()
    }

    struct CapturedRequest {
        path: String,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    fn spawn_server(
        responses: Vec<Vec<Vec<u8>>>,
    ) -> (
        String,
        mpsc::Receiver<CapturedRequest>,
        thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                sender.send(request).unwrap();
                let content_length: usize = response.iter().map(Vec::len).sum();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                for chunk in response {
                    stream.write_all(&chunk).unwrap();
                    stream.flush().unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
            }
        });
        (format!("http://{address}"), receiver, server)
    }

    fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "connection closed before HTTP headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let (content_length, path, authorization) = {
            let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let request_line = headers.lines().next().unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap().to_owned();
            let authorization = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            });
            (content_length, path, authorization)
        };
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "connection closed before HTTP body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        CapturedRequest {
            path,
            authorization,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }
}
