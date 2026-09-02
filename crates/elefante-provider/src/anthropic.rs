//! Raw HTTP Anthropic Messages API implementation.

use async_stream::try_stream;
use futures_util::{StreamExt, stream::BoxStream};
use reqwest::{Client, Url, header};
use serde_json::{Map, Value, json};

use crate::{
    Capabilities, ContentBlock, Event, Message, ModelId, Provider, ProviderError, Request, Role,
    StopReason, ToolDefinition, Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

#[derive(Clone, Debug)]
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: Url,
}

impl AnthropicProvider {
    /// Creates a client using Anthropic's public API origin.
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time `DEFAULT_BASE_URL` constant is invalid.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: Url::parse(DEFAULT_BASE_URL).expect("default Anthropic URL is valid"),
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

    fn messages_url(&self) -> Result<Url, ProviderError> {
        self.base_url
            .join("v1/messages")
            .map_err(|error| ProviderError::Configuration(error.to_string()))
    }

    fn request_body(&self, request: &Request) -> Value {
        let capabilities = self.capabilities(&request.model);
        let mut body = Map::new();
        body.insert("max_tokens".into(), json!(request.max_tokens));
        body.insert("messages".into(), messages_json(&request.messages));
        body.insert("model".into(), json!(request.model.as_str()));
        body.insert("stream".into(), Value::Bool(true));

        if !request.system.is_empty() {
            body.insert(
                "system".into(),
                system_json(&request.system, capabilities.prompt_caching),
            );
        }
        if !request.tools.is_empty() {
            body.insert(
                "tools".into(),
                tools_json(
                    &request.tools,
                    capabilities.prompt_caching,
                    capabilities.structured_output,
                ),
            );
        }
        if capabilities.adaptive_thinking {
            body.insert(
                "thinking".into(),
                json!({ "display": "summarized", "type": "adaptive" }),
            );
        }
        if capabilities.effort {
            if let Some(effort) = request.effort {
                body.insert("output_config".into(), json!({ "effort": effort }));
            }
        }
        Value::Object(body)
    }
}

impl Provider for AnthropicProvider {
    fn capabilities(&self, model: &ModelId) -> Capabilities {
        let model = model.as_str().to_ascii_lowercase();
        let adaptive = model.contains("opus-5")
            || model.contains("sonnet-5")
            || model.contains("opus-4-8")
            || model.contains("opus-4.8")
            || model.contains("opus-4-7")
            || model.contains("opus-4.7");
        let mid_conversation_system =
            model.contains("opus-5") || model.contains("opus-4-8") || model.contains("opus-4.8");

        Capabilities {
            adaptive_thinking: adaptive,
            effort: adaptive,
            prompt_caching: true,
            mid_conversation_system,
            structured_output: true,
            cache_metrics: true,
        }
    }

    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event, ProviderError>> {
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let url = self.messages_url();
        let body = self.request_body(&request);

        Box::pin(try_stream! {
            let url = url?;
            let response = client
                .post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", API_VERSION)
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
            let mut parser = AnthropicEventParser::default();
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
            if parser.block.is_some() {
                Err(ProviderError::Protocol(
                    "stream ended during a content block".to_owned(),
                ))?;
            }
        })
    }
}

fn system_json(system: &[String], cache: bool) -> Value {
    let last = system.len().saturating_sub(1);
    Value::Array(
        system
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let mut block = json!({ "text": text, "type": "text" });
                if cache && index == last {
                    block["cache_control"] = json!({ "type": "ephemeral" });
                }
                block
            })
            .collect(),
    )
}

fn tools_json(tools: &[ToolDefinition], cache: bool, strict: bool) -> Value {
    let mut sorted: Vec<&ToolDefinition> = tools.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));
    let last = sorted.len().saturating_sub(1);
    Value::Array(
        sorted
            .into_iter()
            .enumerate()
            .map(|(index, tool)| {
                let mut value = json!({
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                    "name": tool.name,
                });
                if strict {
                    value["strict"] = Value::Bool(true);
                }
                if cache && index == last {
                    value["cache_control"] = json!({ "type": "ephemeral" });
                }
                value
            })
            .collect(),
    )
}

fn messages_json(messages: &[Message]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| {
                json!({
                    "content": message.content.iter().map(content_json).collect::<Vec<_>>(),
                    "role": match message.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::System => "system",
                    },
                })
            })
            .collect(),
    )
}

fn content_json(content: &ContentBlock) -> Value {
    match content {
        ContentBlock::Text { text } => json!({ "text": text, "type": "text" }),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => json!({
            "signature": signature,
            "thinking": thinking,
            "type": "thinking",
        }),
        ContentBlock::RedactedThinking { data } => {
            json!({ "data": data, "type": "redacted_thinking" })
        }
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "id": id, "input": input, "name": name, "type": "tool_use" })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "content": content,
            "is_error": is_error,
            "tool_use_id": tool_use_id,
            "type": "tool_result",
        }),
    }
}

#[derive(Debug)]
struct SseFrame {
    event: Option<String>,
    data: String,
}

#[derive(Default)]
struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, ProviderError> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some((end, delimiter_len)) = find_frame_end(&self.pending) {
            let raw = self.pending.drain(..end).collect::<Vec<_>>();
            self.pending.drain(..delimiter_len);
            if let Some(frame) = parse_sse_frame(&raw)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Option<SseFrame>, ProviderError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let raw = std::mem::take(&mut self.pending);
        parse_sse_frame(&raw)
    }
}

fn find_frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let line_feed = bytes
        .windows(2)
        .position(|part| part == b"\n\n")
        .map(|position| (position, 2));
    let carriage_return = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|position| (position, 4));
    match (line_feed, carriage_return) {
        (Some(left), Some(right)) => Some(if left.0 < right.0 { left } else { right }),
        (Some(frame), None) | (None, Some(frame)) => Some(frame),
        (None, None) => None,
    }
}

fn parse_sse_frame(bytes: &[u8]) -> Result<Option<SseFrame>, ProviderError> {
    let frame = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::Protocol(format!("SSE was not UTF-8: {error}")))?;
    let mut event = None;
    let mut data = Vec::new();
    for raw_line in frame.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseFrame {
        event,
        data: data.join("\n"),
    }))
}

#[derive(Default)]
struct AnthropicEventParser {
    block: Option<ActiveBlock>,
    stopped: bool,
}

enum ActiveBlock {
    Thinking { thinking: String, signature: String },
    RedactedThinking { data: String },
    Tool(ToolInput),
    Other,
}

struct ToolInput {
    id: String,
    initial: Value,
    partial: String,
}

impl AnthropicEventParser {
    // Keeping the finite wire-event mapping together makes protocol state transitions auditable.
    #[allow(clippy::too_many_lines)]
    fn parse(&mut self, frame: &SseFrame) -> Result<Vec<Event>, ProviderError> {
        if frame.data == "[DONE]" {
            return Ok(Vec::new());
        }
        let payload: Value = serde_json::from_str(&frame.data)?;
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .or(frame.event.as_deref())
            .ok_or_else(|| ProviderError::Protocol("SSE event has no type".to_owned()))?;
        let mut events = Vec::new();

        match kind {
            "message_start" => {
                if let Some(reason) = payload
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str)
                {
                    let reason = stop_reason(reason);
                    self.stopped = true;
                    events.push(Event::Stop(reason));
                    return Ok(events);
                }
                if let Some(usage) = payload.pointer("/message/usage") {
                    let usage = usage_from(usage);
                    if !usage.is_empty() {
                        events.push(Event::Usage(usage));
                    }
                }
            }
            "content_block_start" if !self.stopped => {
                if self.block.is_some() {
                    return Err(ProviderError::Protocol(
                        "overlapping content blocks".to_owned(),
                    ));
                }
                let block = payload.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("thinking") => {
                        let thinking = required_str(block, "thinking")?.to_owned();
                        let signature = required_str(block, "signature")?.to_owned();
                        if !thinking.is_empty() {
                            events.push(Event::ThinkingDelta(thinking.clone()));
                        }
                        self.block = Some(ActiveBlock::Thinking {
                            thinking,
                            signature,
                        });
                    }
                    Some("redacted_thinking") => {
                        self.block = Some(ActiveBlock::RedactedThinking {
                            data: required_str(block, "data")?.to_owned(),
                        });
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                events.push(Event::TextDelta(text.to_owned()));
                            }
                        }
                        self.block = Some(ActiveBlock::Other);
                    }
                    Some("tool_use") => {
                        let id = required_str(block, "id")?.to_owned();
                        let name = required_str(block, "name")?.to_owned();
                        events.push(Event::ToolUseStart {
                            id: id.clone(),
                            name,
                        });
                        self.block = Some(ActiveBlock::Tool(ToolInput {
                            id,
                            initial: block.get("input").cloned().unwrap_or(Value::Null),
                            partial: String::new(),
                        }));
                    }
                    _ => self.block = Some(ActiveBlock::Other),
                }
            }
            "content_block_delta" if !self.stopped => {
                let delta = payload.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        let text = required_str(delta, "thinking")?;
                        let Some(ActiveBlock::Thinking { thinking, .. }) = self.block.as_mut()
                        else {
                            return Err(ProviderError::Protocol(
                                "thinking delta outside thinking block".to_owned(),
                            ));
                        };
                        thinking.push_str(text);
                        events.push(Event::ThinkingDelta(text.to_owned()));
                    }
                    Some("signature_delta") => {
                        let value = required_str(delta, "signature")?;
                        let Some(ActiveBlock::Thinking { signature, .. }) = self.block.as_mut()
                        else {
                            return Err(ProviderError::Protocol(
                                "signature delta outside thinking block".to_owned(),
                            ));
                        };
                        signature.push_str(value);
                    }
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            events.push(Event::TextDelta(text.to_owned()));
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = required_str(delta, "partial_json")?;
                        let Some(ActiveBlock::Tool(tool)) = self.block.as_mut() else {
                            return Err(ProviderError::Protocol(
                                "tool input delta outside tool block".to_owned(),
                            ));
                        };
                        tool.partial.push_str(partial);
                        events.push(Event::ToolUseInputDelta(partial.to_owned()));
                    }
                    _ => {}
                }
            }
            "content_block_stop" if !self.stopped => {
                let block = self.block.take().ok_or_else(|| {
                    ProviderError::Protocol("content block stopped before it started".to_owned())
                })?;
                match block {
                    ActiveBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        if signature.is_empty() {
                            return Err(ProviderError::Protocol(
                                "thinking block ended without a signature".to_owned(),
                            ));
                        }
                        events.push(Event::ThinkingBlockComplete {
                            thinking,
                            signature,
                        });
                    }
                    ActiveBlock::RedactedThinking { data } => {
                        events.push(Event::RedactedThinkingBlockComplete { data });
                    }
                    ActiveBlock::Tool(tool) => {
                        let input = if tool.partial.is_empty() {
                            tool.initial
                        } else {
                            serde_json::from_str(&tool.partial)?
                        };
                        events.push(Event::ToolUseComplete { id: tool.id, input });
                    }
                    ActiveBlock::Other => {}
                }
            }
            "message_delta" => {
                if let Some(usage) = payload.get("usage") {
                    let usage = usage_from(usage);
                    if !usage.is_empty() {
                        events.push(Event::Usage(usage));
                    }
                }
                if let Some(reason) = payload
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                {
                    if !self.stopped {
                        self.stopped = true;
                        if reason == "refusal" {
                            // A refusal supersedes any incomplete content block. Nothing from the
                            // rejected partial block may enter replay history.
                            self.block = None;
                        }
                        events.push(Event::Stop(stop_reason(reason)));
                    }
                }
            }
            "error" => {
                let message = payload
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown Anthropic stream error");
                return Err(ProviderError::Protocol(message.to_owned()));
            }
            // Anthropic may add event types without changing the API version. Unknown events
            // (along with ping/message_stop and content received after a terminal stop) contain no
            // normalized state we can safely act on, so ignore them for compatibility.
            _ => {}
        }
        Ok(events)
    }
}

fn required_str<'a>(object: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    object.get(key).and_then(Value::as_str).ok_or_else(|| {
        ProviderError::Protocol(format!("Anthropic event missing string field `{key}`"))
    })
}

fn usage_from(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_input_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        "stop_sequence" => StopReason::StopSequence,
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
    fn request_is_adaptive_cached_sorted_and_has_no_budget_tokens() {
        let provider = AnthropicProvider::new("test-key");
        let request = Request {
            model: ModelId::from("claude-opus-5"),
            max_tokens: 2048,
            system: vec!["stable system".to_owned()],
            tools: vec![
                ToolDefinition::new("write", "write", json!({ "type": "object" })),
                ToolDefinition::new("read", "read", json!({ "type": "object" })),
            ],
            messages: vec![Message::text(Role::User, "hello")],
            effort: Some(Effort::XHigh),
        };
        let body = provider.request_body(&request);

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][1]["name"], "write");
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(
            !serde_json::to_string(&body)
                .unwrap()
                .contains("budget_tokens")
        );
    }

    #[test]
    fn unsupported_model_does_not_receive_thinking_or_effort() {
        let provider = AnthropicProvider::new("test-key");
        let mut request = Request::new(
            "claude-3-5-haiku-latest",
            128,
            vec![Message::text(Role::User, "hello")],
        );
        request.effort = Some(Effort::High);
        let body = provider.request_body(&request);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn sse_decoder_handles_split_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"event: ping\r\nda").unwrap().is_empty());
        let frames = decoder
            .push(b"ta: {\"type\":\r\ndata: \"ping\"}\r\n\r\n")
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("ping"));
        assert_eq!(frames[0].data, "{\"type\":\n\"ping\"}");
    }

    #[test]
    fn sse_decoder_chooses_the_first_mixed_line_ending_boundary() {
        let mut decoder = SseDecoder::default();
        let frames = decoder
            .push(b"event: ping\r\ndata: first\r\n\r\nevent: ping\ndata: second\n\n")
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "first");
        assert_eq!(frames[1].data, "second");
    }

    #[test]
    fn parser_streams_thinking_and_parses_tool_input_as_json() {
        let mut parser = AnthropicEventParser::default();
        let frames = [
            r#"{"type":"content_block_start","content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"consider"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"signed"}}"#,
            r#"{"type":"content_block_stop"}"#,
            r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"tool-1","name":"write","input":{}}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\\\"b\","}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"\"body\":\"line\\n2\"}"}}"#,
            r#"{"type":"content_block_stop"}"#,
        ];
        let mut events = Vec::new();
        for data in frames {
            events.extend(
                parser
                    .parse(&SseFrame {
                        event: None,
                        data: data.to_owned(),
                    })
                    .unwrap(),
            );
        }
        assert_eq!(events[0], Event::ThinkingDelta("consider".to_owned()));
        assert_eq!(
            events.last(),
            Some(&Event::ToolUseComplete {
                id: "tool-1".to_owned(),
                input: json!({ "path": "a\"b", "body": "line\n2" }),
            })
        );
    }

    #[test]
    fn parser_preserves_complete_signed_and_redacted_thinking_blocks() {
        let mut parser = AnthropicEventParser::default();
        let frames = [
            r#"{"type":"content_block_start","content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"short summary"}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"opaque-signature"}}"#,
            r#"{"type":"content_block_stop"}"#,
            r#"{"type":"content_block_start","content_block":{"type":"redacted_thinking","data":"opaque-redacted-data"}}"#,
            r#"{"type":"content_block_stop"}"#,
        ];
        let mut events = Vec::new();
        for data in frames {
            events.extend(
                parser
                    .parse(&SseFrame {
                        event: None,
                        data: data.to_owned(),
                    })
                    .unwrap(),
            );
        }

        assert_eq!(
            events,
            vec![
                Event::ThinkingDelta("short summary".to_owned()),
                Event::ThinkingBlockComplete {
                    thinking: "short summary".to_owned(),
                    signature: "opaque-signature".to_owned(),
                },
                Event::RedactedThinkingBlockComplete {
                    data: "opaque-redacted-data".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn parser_preserves_signed_thinking_block_with_empty_display_text() {
        let mut parser = AnthropicEventParser::default();
        let frames = [
            r#"{"type":"content_block_start","content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"signed-empty-display"}}"#,
            r#"{"type":"content_block_stop"}"#,
        ];
        let mut events = Vec::new();
        for data in frames {
            events.extend(
                parser
                    .parse(&SseFrame {
                        event: None,
                        data: data.to_owned(),
                    })
                    .unwrap(),
            );
        }
        assert_eq!(
            events,
            vec![Event::ThinkingBlockComplete {
                thinking: String::new(),
                signature: "signed-empty-display".to_owned(),
            }]
        );
    }

    #[test]
    fn malformed_tool_json_is_an_error() {
        let mut parser = AnthropicEventParser::default();
        parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"1","name":"read","input":{}}}"#.to_owned(),
            })
            .unwrap();
        parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#.to_owned(),
            })
            .unwrap();
        let error = parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"content_block_stop"}"#.to_owned(),
            })
            .unwrap_err();
        assert!(matches!(error, ProviderError::InvalidJson(_)));
    }

    #[test]
    fn refusal_stops_before_content_when_announced_at_message_start() {
        let mut parser = AnthropicEventParser::default();
        let refusal = parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"message_start","message":{"stop_reason":"refusal","content":[{"type":"text","text":"do not expose"}]}}"#.to_owned(),
            })
            .unwrap();
        let content = parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"do not expose"}}"#.to_owned(),
            })
            .unwrap();
        assert_eq!(refusal, vec![Event::Stop(StopReason::Refusal)]);
        assert!(content.is_empty());
    }

    #[test]
    fn refusal_discards_an_incomplete_tool_block() {
        let mut parser = AnthropicEventParser::default();
        parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"1","name":"read","input":{}}}"#.to_owned(),
            })
            .unwrap();
        let events = parser
            .parse(&SseFrame {
                event: None,
                data: r#"{"type":"message_delta","delta":{"stop_reason":"refusal"}}"#.to_owned(),
            })
            .unwrap();
        assert_eq!(events, vec![Event::Stop(StopReason::Refusal)]);
        assert!(parser.block.is_none());
    }

    #[test]
    fn unknown_future_sse_events_are_ignored() {
        let mut parser = AnthropicEventParser::default();
        let events = parser
            .parse(&SseFrame {
                event: Some("content_block_annotation".to_owned()),
                data: r#"{"type":"content_block_annotation","annotation":{"future":true}}"#
                    .to_owned(),
            })
            .unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn raw_http_stream_survives_split_frames_and_reports_cache_reads() {
        let first = sse_response(0, 1);
        let second = sse_response(121, 2);
        let (base_url, requests, server) =
            spawn_server(vec![split_every(&first, 7), split_every(&second, 5)]);
        let provider = AnthropicProvider::new("secret-test-key")
            .with_base_url(&base_url)
            .unwrap();
        let mut request = Request::new(
            "claude-opus-5",
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
        assert_eq!(first_request.path, "/v1/messages");
        assert_eq!(first_request.api_key.as_deref(), Some("secret-test-key"));
        assert!(
            first_events
                .iter()
                .any(|event| matches!(event, Event::ThinkingDelta(text) if text == "thinking-1"))
        );
        assert!(
            second_events
                .iter()
                .any(|event| matches!(event, Event::ThinkingDelta(text) if text == "thinking-2"))
        );
        assert!(second_events.iter().any(|event| {
            matches!(event, Event::Usage(Usage { cache_read_input_tokens, .. }) if *cache_read_input_tokens == 121)
        }));
        assert!(matches!(
            second_events.last(),
            Some(Event::Stop(StopReason::EndTurn))
        ));
    }

    #[tokio::test]
    async fn tool_continuation_replays_signed_thinking_blocks_exactly() {
        let first = tool_use_sse_response();
        let second = sse_response(87, 2);
        let (base_url, requests, server) =
            spawn_server(vec![split_every(&first, 3), split_every(&second, 11)]);
        let provider = AnthropicProvider::new("secret-test-key")
            .with_base_url(&base_url)
            .unwrap();
        let mut request = Request::new(
            "claude-opus-5",
            256,
            vec![Message::text(Role::User, "inspect the readme")],
        );
        request.tools = vec![ToolDefinition::new(
            "read",
            "read a file",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false,
            }),
        )];
        request.effort = Some(Effort::High);

        let first_events = collect(provider.stream(request.clone())).await;
        let mut assistant_content = Vec::new();
        let mut tool_name = None;
        for event in &first_events {
            match event {
                Event::ThinkingBlockComplete {
                    thinking,
                    signature,
                } => assistant_content.push(ContentBlock::Thinking {
                    thinking: thinking.clone(),
                    signature: signature.clone(),
                }),
                Event::RedactedThinkingBlockComplete { data } => {
                    assistant_content.push(ContentBlock::RedactedThinking { data: data.clone() });
                }
                Event::ToolUseStart { id: _, name } => tool_name = Some(name.clone()),
                Event::ToolUseComplete { id, input } => {
                    assistant_content.push(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: tool_name.take().unwrap(),
                        input: input.clone(),
                    });
                }
                Event::ThinkingDelta(_)
                | Event::TextDelta(_)
                | Event::ToolUseInputDelta(_)
                | Event::Usage(_)
                | Event::Stop(_) => {}
            }
        }
        assert_eq!(
            first_events
                .iter()
                .filter_map(|event| match event {
                    Event::ThinkingDelta(summary) => Some(summary.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["safe summary"]
        );

        request
            .messages
            .push(Message::new(Role::Assistant, assistant_content));
        request.messages.push(Message::new(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_owned(),
                content: "contents".to_owned(),
                is_error: false,
            }],
        ));
        let _second_events = collect(provider.stream(request)).await;
        let _first_request = requests.recv().unwrap();
        let second_request = requests.recv().unwrap();
        server.join().unwrap();

        let body: Value = serde_json::from_slice(&second_request.body).unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {
                    "type": "thinking",
                    "thinking": "safe summary",
                    "signature": "opaque-signature",
                },
                {
                    "type": "redacted_thinking",
                    "data": "opaque-redacted-data",
                },
                {
                    "type": "tool_use",
                    "id": "tool-1",
                    "name": "read",
                    "input": { "path": "README.md" },
                },
            ])
        );
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "tool-1");
    }

    /// Opt in with `ANTHROPIC_API_KEY` and `ANTHROPIC_TEST_MODEL`.
    #[tokio::test]
    #[ignore = "requires Anthropic credentials, network, and billable requests"]
    async fn live_second_identical_request_reads_prompt_cache() {
        let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set");
        let model = std::env::var("ANTHROPIC_TEST_MODEL")
            .expect("ANTHROPIC_TEST_MODEL must name an adaptive-thinking model");
        let provider = AnthropicProvider::new(api_key);
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
                Event::Usage(usage) => Some(usage.cache_read_input_tokens),
                _ => None,
            })
            .sum();
        assert!(cache_reads > 0, "second request did not read prompt cache");
    }

    async fn collect(mut stream: BoxStream<'static, Result<Event, ProviderError>>) -> Vec<Event> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event.unwrap());
        }
        events
    }

    fn sse_response(cache_read: u64, turn: u8) -> Vec<u8> {
        let frames = [
            (
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "stop_reason": null,
                        "usage": {
                            "input_tokens": 150,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": cache_read,
                        },
                    },
                }),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "content_block": { "type": "thinking", "thinking": "", "signature": "" },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": format!("thinking-{turn}"),
                    },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "delta": { "type": "signature_delta", "signature": format!("signature-{turn}") },
                }),
            ),
            (
                "content_block_stop",
                json!({ "type": "content_block_stop" }),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 3 },
                }),
            ),
            ("message_stop", json!({ "type": "message_stop" })),
        ];
        let mut response = String::new();
        for (event, data) in frames {
            write!(response, "event: {event}\r\ndata: {data}\r\n\r\n").unwrap();
        }
        response.into_bytes()
    }

    fn tool_use_sse_response() -> Vec<u8> {
        let frames = [
            (
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "stop_reason": null,
                        "usage": { "input_tokens": 100, "output_tokens": 0 },
                    },
                }),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "content_block": { "type": "thinking", "thinking": "", "signature": "" },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "delta": { "type": "thinking_delta", "thinking": "safe summary" },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "delta": { "type": "signature_delta", "signature": "opaque-signature" },
                }),
            ),
            (
                "content_block_stop",
                json!({ "type": "content_block_stop" }),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "content_block": {
                        "type": "redacted_thinking",
                        "data": "opaque-redacted-data",
                    },
                }),
            ),
            (
                "content_block_stop",
                json!({ "type": "content_block_stop" }),
            ),
            (
                "content_block_start",
                json!({
                    "type": "content_block_start",
                    "content_block": {
                        "type": "tool_use",
                        "id": "tool-1",
                        "name": "read",
                        "input": {},
                    },
                }),
            ),
            (
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"path\":\"README.md\"}",
                    },
                }),
            ),
            (
                "content_block_stop",
                json!({ "type": "content_block_stop" }),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": { "output_tokens": 15 },
                }),
            ),
            ("message_stop", json!({ "type": "message_stop" })),
        ];
        let mut response = String::new();
        for (event, data) in frames {
            write!(response, "event: {event}\r\ndata: {data}\r\n\r\n").unwrap();
        }
        response.into_bytes()
    }

    fn split_every(bytes: &[u8], width: usize) -> Vec<Vec<u8>> {
        bytes.chunks(width).map(<[u8]>::to_vec).collect()
    }

    struct CapturedRequest {
        path: String,
        api_key: Option<String>,
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
        let (content_length, path, api_key) = {
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
            let api_key = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-api-key")
                    .then(|| value.trim().to_owned())
            });
            (content_length, path, api_key)
        };
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "connection closed before HTTP body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        CapturedRequest {
            path,
            api_key,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }
}
