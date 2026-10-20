use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use elefante_core::tools::ToolExecutor;
use elefante_core::{LoopError, OutputEvent, RunConfig, run_agent_loop};
use elefante_provider::{
    Capabilities, ContentBlock, Event, ModelId, Provider, ProviderError, ReasoningDetail, Request,
    StopReason, Usage,
};
use futures_util::{StreamExt, stream};
use serde_json::json;

/// The exact reasoning blocks the scripted provider emits. The provider
/// requires this sequence back unmodified and unreordered, so the same value
/// is used to script the stream and to assert what the next request carries.
fn scripted_reasoning() -> Vec<ReasoningDetail> {
    vec![
        ReasoningDetail {
            kind: "reasoning.text".to_owned(),
            raw: json!({
                "type": "reasoning.text",
                "text": "focused edit summary",
                "signature": "opaque-signature",
                "index": 0,
            }),
        },
        ReasoningDetail {
            kind: "reasoning.encrypted".to_owned(),
            raw: json!({
                "type": "reasoning.encrypted",
                "data": "opaque-redacted-payload",
                "index": 1,
            }),
        },
    ]
}

struct ScriptedProvider {
    rounds: Mutex<VecDeque<Vec<Event>>>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl ScriptedProvider {
    fn new(rounds: Vec<Vec<Event>>) -> (Self, Arc<Mutex<Vec<Request>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                rounds: Mutex::new(rounds.into()),
                requests: Arc::clone(&requests),
            },
            requests,
        )
    }
}

impl Provider for ScriptedProvider {
    fn capabilities(&self, _model: &ModelId) -> Capabilities {
        Capabilities {
            adaptive_thinking: true,
            effort: true,
            prompt_caching: true,
            mid_conversation_system: true,
            structured_output: true,
            cache_metrics: true,
        }
    }

    fn stream(
        &self,
        request: Request,
    ) -> futures_util::stream::BoxStream<'static, Result<Event, ProviderError>> {
        self.requests.lock().unwrap().push(request);
        let events = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .expect("script has one response per request");
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
}

fn scripted_edit_rounds() -> Vec<Vec<Event>> {
    vec![
        vec![
            Event::ReasoningDelta("I will make the focused edit.".to_owned()),
            Event::ReasoningDetails(scripted_reasoning()),
            Event::ToolUseStart {
                id: "edit-1".to_owned(),
                name: "edit".to_owned(),
            },
            Event::ToolUseComplete {
                id: "edit-1".to_owned(),
                input: json!({
                    "path": "note.txt", "old_string": "before\n", "new_string": "after\n"
                }),
            },
            Event::ToolUseStart {
                id: "bad-2".to_owned(),
                name: "not_a_tool".to_owned(),
            },
            Event::ToolUseComplete {
                id: "bad-2".to_owned(),
                input: json!({}),
            },
            Event::Stop(StopReason::ToolUse),
        ],
        vec![
            Event::TextDelta("Done.".to_owned()),
            Event::Stop(StopReason::EndTurn),
        ],
    ]
}

#[tokio::test]
async fn scripted_provider_edits_repo_and_pairs_all_parallel_results() {
    let repository = tempfile::tempdir().unwrap();
    std::fs::write(repository.path().join("note.txt"), "before\n").unwrap();
    let executor = ToolExecutor::new(repository.path()).unwrap();
    let (provider, requests) = ScriptedProvider::new(scripted_edit_rounds());
    let mut output = Vec::new();

    let outcome = run_agent_loop(
        &provider,
        &executor,
        "Change before to after.",
        RunConfig::new("scripted-model"),
        |event| output.push(event),
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(repository.path().join("note.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(outcome.stop_reason, StopReason::EndTurn);
    assert_eq!(outcome.text, "Done.");
    assert!(output.iter().any(
        |event| matches!(event, OutputEvent::ReasoningDelta(text) if text.contains("focused"))
    ));
    assert_eq!(
        output
            .iter()
            .filter(|event| matches!(event, OutputEvent::ReasoningDelta(_)))
            .count(),
        1,
        "replay-only reasoning details must never reach the renderer"
    );
    assert!(
        output
            .iter()
            .any(|event| matches!(event, OutputEvent::TextDelta(text) if text == "Done."))
    );

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let result_messages = requests[1]
        .messages
        .iter()
        .filter(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .collect::<Vec<_>>();
    assert_eq!(result_messages.len(), 1, "all results share one message");
    assert_eq!(result_messages[0].content.len(), 2);
    let assistant_message = &requests[1].messages[1];
    let ContentBlock::Reasoning { details } = &assistant_message.content[0] else {
        panic!("reasoning must lead the replayed assistant turn");
    };
    assert_eq!(
        details,
        &scripted_reasoning(),
        "reasoning blocks must replay unmodified and unreordered"
    );
    assert!(matches!(
        &assistant_message.content[1],
        ContentBlock::ToolUse { id, .. } if id == "edit-1"
    ));
    assert!(matches!(
        &result_messages[0].content[0],
        ContentBlock::ToolResult {
            tool_use_id,
            is_error: false,
            ..
        } if tool_use_id == "edit-1"
    ));
    assert!(matches!(
        &result_messages[0].content[1],
        ContentBlock::ToolResult {
            tool_use_id,
            is_error: true,
            ..
        } if tool_use_id == "bad-2"
    ));
}

#[tokio::test]
async fn refusal_stops_without_dispatching_tools() {
    let repository = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repository.path()).unwrap();
    let (provider, requests) = ScriptedProvider::new(vec![vec![Event::Stop(StopReason::Refusal)]]);

    let outcome = run_agent_loop(
        &provider,
        &executor,
        "Do something disallowed.",
        RunConfig::new("scripted-model"),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Refusal);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refusal_discards_an_incomplete_tool_call() {
    let repository = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repository.path()).unwrap();
    let (provider, requests) = ScriptedProvider::new(vec![vec![
        Event::ToolUseStart {
            id: "unfinished".to_owned(),
            name: "write".to_owned(),
        },
        Event::Stop(StopReason::Refusal),
    ]]);

    let outcome = run_agent_loop(
        &provider,
        &executor,
        "Do something disallowed.",
        RunConfig::new("scripted-model"),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.stop_reason, StopReason::Refusal);
    assert_eq!(outcome.text, "");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stream_without_stop_is_a_protocol_error() {
    let repository = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repository.path()).unwrap();
    let (provider, _) = ScriptedProvider::new(vec![vec![Event::TextDelta("partial".to_owned())]]);

    let result = run_agent_loop(
        &provider,
        &executor,
        "Prompt",
        RunConfig::new("scripted-model"),
        |_| {},
    )
    .await;

    assert!(matches!(result, Err(LoopError::Protocol(_))));
}

#[tokio::test]
async fn usage_is_summed_across_rounds_and_survives_arriving_after_stop() {
    let repository = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repository.path()).unwrap();

    // OpenRouter reports usage on the chunk *after* finish_reason, so each round
    // here places Usage behind Stop. A loop that stopped draining at Stop would
    // silently report zero tokens and no cost.
    let (provider, _) = ScriptedProvider::new(vec![vec![
        Event::TextDelta("first".to_owned()),
        Event::Stop(StopReason::EndTurn),
        Event::Usage(Usage {
            input_tokens: 100,
            output_tokens: 10,
            reasoning_tokens: 4,
            cached_input_tokens: 60,
            cache_write_tokens: 40,
            cost: Some(0.25),
        }),
    ]]);

    let outcome = run_agent_loop(
        &provider,
        &executor,
        "Prompt",
        RunConfig::new("scripted-model"),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(outcome.usage.cached_input_tokens, 60);
    assert_eq!(outcome.usage.reasoning_tokens, 4);
    assert_eq!(outcome.usage.cost, Some(0.25));
}

#[test]
fn usage_accumulate_sums_tokens_and_keeps_unreported_cost_distinct_from_zero() {
    let mut total = Usage::default();
    assert_eq!(total.cost, None);

    // A report with no cost must not invent one: "not reported" and "reported as
    // zero" are different facts, and M5 attribution depends on telling them apart.
    total.accumulate(Usage {
        input_tokens: 10,
        cached_input_tokens: 5,
        ..Usage::default()
    });
    assert_eq!(total.input_tokens, 10);
    assert_eq!(total.cost, None);

    total.accumulate(Usage {
        input_tokens: 7,
        cost: Some(0.5),
        ..Usage::default()
    });
    total.accumulate(Usage {
        cost: Some(0.25),
        ..Usage::default()
    });
    assert_eq!(total.input_tokens, 17);
    assert_eq!(total.cached_input_tokens, 5);
    assert_eq!(total.cost, Some(0.75));
}
