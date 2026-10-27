//! Unit tests for the `OpenRouter` transport.
//!
//! These live beside the module rather than in `tests/` because they cover
//! internals — request construction, URL resolution, the SSE event parser,
//! and capability profiles. Reaching them from an integration test would mean
//! making implementation details public purely to test them.

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
fn web_search_is_opt_in_and_pins_exa_with_the_requested_result_count() {
    let provider = OpenRouterProvider::new("test-key");
    let mut request = Request::new(
        "some-vendor/model",
        256,
        vec![Message::text(Role::User, "rust release")],
    );
    let original = serde_json::to_vec(&provider.request_body(&request)).unwrap();
    assert_eq!(original, br#"{"max_tokens":256,"messages":[{"content":"rust release","role":"user"}],"model":"some-vendor/model","stream":true,"usage":{"include":true}}"#);
    assert!(
        serde_json::to_value(&request)
            .unwrap()
            .get("web_search")
            .is_none()
    );
    let legacy = serde_json::to_value(&request).unwrap();
    let decoded: Request = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.web_search, None);

    for max_results in [1, 3, 10] {
        request.web_search = Some(crate::WebSearch { max_results });
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["web_search"], json!({ "max_results": max_results }));
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);
        let mut body = provider.request_body(&request);
        assert_eq!(
            body.as_object_mut().unwrap().remove("plugins"),
            Some(json!([
                { "id": "web", "engine": "exa", "max_results": max_results }
            ]))
        );
        assert_eq!(serde_json::to_vec(&body).unwrap(), original);
    }
    request.web_search = None;
    assert_eq!(
        serde_json::to_vec(&provider.request_body(&request)).unwrap(),
        original
    );
}

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
        response_schema: None,
        web_search: None,
    };
    let body = provider.request_body(&request);

    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["stream"], true);
    assert_eq!(body["usage"]["include"], true);
    assert_eq!(body["provider"]["require_parameters"], true);
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
fn auto_router_gets_reasoning_but_not_caching_and_covers_the_beta_slug() {
    let provider = OpenRouterProvider::new("test-key");

    for slug in ["openrouter/auto", "openrouter/auto-beta"] {
        let capabilities = provider.capabilities(&ModelId::from(slug));
        assert!(capabilities.adaptive_thinking, "{slug} declares reasoning");
        assert!(capabilities.effort, "{slug} declares reasoning_effort");
        assert!(
            capabilities.structured_output,
            "{slug} declares structured_outputs"
        );
        // Auto's pricing carries no input_cache_read, so caching is unknown
        // rather than absent; we do not send breakpoints on a guess.
        assert!(!capabilities.prompt_caching, "{slug} has no cache pricing");
    }

    let request = Request {
        model: ModelId::from("openrouter/auto"),
        max_tokens: 1024,
        system: vec!["stable system".to_owned()],
        tools: vec![ToolDefinition::new(
            "read",
            "read",
            json!({ "type": "object" }),
        )],
        messages: vec![Message::text(Role::User, "hello")],
        effort: Some(Effort::High),
        response_schema: None,
        web_search: None,
    };
    let body = provider.request_body(&request);

    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["provider"]["require_parameters"], true);
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none(),
        "no cache breakpoint without declared caching"
    );
}

#[test]
fn a_long_error_body_is_truncated_but_keeps_its_opening() {
    let html = format!(
        "<!DOCTYPE html><title>Not Found</title>{}",
        "x".repeat(50_000)
    );
    let truncated = truncate_error_body(&html);

    assert!(truncated.starts_with("<!DOCTYPE html><title>Not Found</title>"));
    assert!(truncated.len() < 800, "kept {} bytes", truncated.len());
    assert!(truncated.contains("bytes total"), "reports the real size");
    assert_eq!(truncate_error_body("short body"), "short body");
}

#[test]
fn default_base_url_resolves_to_the_versioned_completions_path() {
    // Regression: every other test overrides the base URL to reach the mock
    // server, so nothing exercised the shipped default. Without a trailing
    // slash `Url::join` drops `v1` and the real endpoint 404s.
    let url = OpenRouterProvider::new("test-key")
        .completions_url()
        .unwrap();
    assert_eq!(
        url.as_str(),
        "https://openrouter.ai/api/v1/chat/completions"
    );
}

#[test]
fn an_overridden_base_url_without_a_trailing_slash_keeps_its_last_segment() {
    let url = OpenRouterProvider::new("test-key")
        .with_base_url("https://gateway.example/proxy/v9")
        .unwrap()
        .completions_url()
        .unwrap();
    assert_eq!(
        url.as_str(),
        "https://gateway.example/proxy/v9/chat/completions"
    );
}

#[test]
fn require_parameters_is_sent_only_alongside_tools() {
    let provider = OpenRouterProvider::new("test-key");
    let request = Request {
        model: ModelId::from("anthropic/claude-opus-5"),
        max_tokens: 1024,
        system: Vec::new(),
        tools: Vec::new(),
        messages: vec![Message::text(Role::User, "hello")],
        effort: None,
        response_schema: None,
        web_search: None,
    };

    assert!(
        provider.request_body(&request).get("provider").is_none(),
        "a request with no tools has no parameter to be dropped"
    );
}

fn schema_request(model: &str) -> Request {
    Request {
        model: ModelId::from(model),
        max_tokens: 1024,
        system: Vec::new(),
        tools: Vec::new(),
        messages: vec![Message::text(Role::User, "hello")],
        effort: None,
        response_schema: Some(crate::ResponseSchema {
            name: "memories".to_owned(),
            schema: json!({ "type": "object", "properties": {} }),
        }),
        web_search: None,
    }
}

#[test]
fn a_response_schema_is_sent_strictly_to_a_model_that_declares_structured_output() {
    let provider = OpenRouterProvider::new("test-key");
    let body = provider.request_body(&schema_request("anthropic/claude-opus-5"));

    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["name"], "memories");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["type"],
        "object"
    );
    assert_eq!(body["provider"]["require_parameters"], true);
}

#[test]
fn a_response_schema_is_withheld_from_a_model_that_does_not_declare_support() {
    let provider = OpenRouterProvider::new("test-key");
    let body = provider.request_body(&schema_request("some-vendor/experimental-42b"));

    assert!(body.get("response_format").is_none());
    assert!(
        body.get("provider").is_none(),
        "nothing was withheld from routing, so routing is not constrained"
    );
}

#[test]
fn a_request_without_a_schema_serialises_exactly_as_before() {
    let provider = OpenRouterProvider::new("test-key");
    let mut request = schema_request("anthropic/claude-opus-5");
    request.response_schema = None;
    let body = provider.request_body(&request);
    assert!(body.get("response_format").is_none());

    let json = serde_json::to_value(&request).unwrap();
    assert!(
        json.get("response_schema").is_none(),
        "recorded requests and fixtures gain no field when it is unused"
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
    assert_eq!(events.len(), 0);
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
        .unwrap_or_else(|_| "anthropic/claude-sonnet-5".to_owned());
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
