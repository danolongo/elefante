//! Per-turn context: where it goes in a request, and that it never becomes history.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use elefante_core::{CancellationToken, ContextSource, RunConfig, Session, ToolGate};
use elefante_provider::{
    Capabilities, ContentBlock, Event, Message, ModelId, Provider, ProviderError, Request, Role,
    StopReason, Usage,
};
use elefante_tools::ToolExecutor;
use futures_util::{
    StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use serde_json::json;

#[derive(Default)]
struct Script {
    rounds: Mutex<VecDeque<Vec<Event>>>,
    requests: Mutex<Vec<Request>>,
}

impl Script {
    fn new(rounds: Vec<Vec<Event>>) -> Self {
        Self {
            rounds: Mutex::new(rounds.into()),
            requests: Mutex::default(),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for Script {
    fn capabilities(&self, _: &ModelId) -> Capabilities {
        Capabilities::default()
    }
    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event, ProviderError>> {
        self.requests.lock().unwrap().push(request);
        stream::iter(
            self.rounds
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .into_iter()
                .map(Ok),
        )
        .boxed()
    }
}

fn tool_round(id: &str) -> Vec<Event> {
    vec![
        Event::ToolUseStart {
            id: id.into(),
            name: "bash".into(),
        },
        Event::ToolUseComplete {
            id: id.into(),
            input: json!({"command": "printf ok"}),
        },
        Event::Stop(StopReason::ToolUse),
    ]
}

fn end(text: &str) -> Vec<Event> {
    vec![
        Event::TextDelta(text.into()),
        Event::Stop(StopReason::EndTurn),
        Event::Usage(Usage::default()),
    ]
}

/// Hands out scripted blocks and preambles, and counts how often it is asked.
#[derive(Default)]
struct Source {
    blocks: Mutex<VecDeque<Option<String>>>,
    preambles: Mutex<VecDeque<Option<String>>>,
    block_calls: AtomicUsize,
    preamble_calls: AtomicUsize,
    prompts: Mutex<Vec<String>>,
}

impl Source {
    fn new(blocks: &[Option<&str>], preambles: &[Option<&str>]) -> Arc<Self> {
        let owned = |list: &[Option<&str>]| list.iter().map(|b| b.map(str::to_owned)).collect();
        Arc::new(Self {
            blocks: Mutex::new(owned(blocks)),
            preambles: Mutex::new(owned(preambles)),
            ..Self::default()
        })
    }
}

impl ContextSource for Source {
    fn turn_block<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Option<String>> {
        self.block_calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt.to_owned());
        let next = self.blocks.lock().unwrap().pop_front().flatten();
        Box::pin(async move { next })
    }

    fn preamble(&self) -> BoxFuture<'_, Option<String>> {
        self.preamble_calls.fetch_add(1, Ordering::SeqCst);
        let next = self.preambles.lock().unwrap().pop_front().flatten();
        Box::pin(async move { next })
    }
}

async fn ask(session: &mut Session, provider: &Script, executor: &ToolExecutor, text: &str) {
    session
        .prompt(
            provider,
            executor,
            &ToolGate::default(),
            text,
            CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.clone(),
                    ContentBlock::ToolUse { name, .. } => format!("[tool_use {name}]"),
                    ContentBlock::ToolResult { content, .. } => format!("[tool_result {content}]"),
                    ContentBlock::Reasoning { .. } => "[reasoning]".to_owned(),
                })
                .collect::<String>()
        })
        .collect()
}

fn bytes(messages: &[Message]) -> String {
    serde_json::to_string(messages).unwrap()
}

#[tokio::test]
async fn the_block_follows_the_user_message_in_every_round_and_never_enters_history() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![tool_round("t1"), end("done")]);
    let source = Source::new(&[Some("BLOCK-1")], &[None]);
    let mut session = Session::new(RunConfig::new("test")).with_context(source.clone());

    ask(&mut session, &provider, &executor, "hello").await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "two rounds");
    assert_eq!(texts(&requests[0].messages), ["hello", "BLOCK-1"]);
    assert_eq!(
        texts(&requests[1].messages),
        ["hello", "BLOCK-1", "[tool_use bash]", "[tool_result ok]"],
        "the same block at the same index in the second round"
    );
    assert_eq!(requests[0].messages[1].role, Role::User, "never system");
    let history = texts(session.messages());
    assert!(
        !history.iter().any(|t| t.contains("BLOCK-1")),
        "{history:?}"
    );
}

#[tokio::test]
async fn context_is_fetched_once_per_turn_not_once_per_round() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![tool_round("t1"), tool_round("t2"), end("done")]);
    let source = Source::new(&[Some("B")], &[None]);
    let mut session = Session::new(RunConfig::new("test")).with_context(source.clone());
    ask(&mut session, &provider, &executor, "go").await;
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(source.block_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *source.prompts.lock().unwrap(),
        ["go"],
        "asked about the user's prompt"
    );
}

#[tokio::test]
async fn the_next_turn_has_a_fresh_block_and_shares_its_prefix_through_the_user_message() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![end("a1"), end("a2"), end("a3")]);
    let source = Source::new(
        &[Some("BLOCK-1"), Some("BLOCK-2"), Some("BLOCK-3")],
        &[None, None, None],
    );
    let mut session = Session::new(RunConfig::new("test")).with_context(source);
    for prompt in ["q1", "q2", "q3"] {
        ask(&mut session, &provider, &executor, prompt).await;
    }
    let requests = provider.requests();

    assert_eq!(texts(&requests[1].messages), ["q1", "a1", "q2", "BLOCK-2"]);
    assert_eq!(
        texts(&requests[2].messages),
        ["q1", "a1", "q2", "a2", "q3", "BLOCK-3"]
    );
    for request in &requests[1..] {
        assert!(
            !texts(&request.messages).iter().any(|t| t == "BLOCK-1"),
            "an earlier turn's block must not be carried forward"
        );
    }
    // Turn 2's request and turn 3's agree, byte for byte, through turn 2's user message. Only
    // what follows it — the block — differs. A block placed before the user message would have
    // moved that message and everything after it.
    assert_eq!(
        bytes(&requests[1].messages[..3]),
        bytes(&requests[2].messages[..3])
    );
    assert_eq!(
        texts(session.messages()),
        ["q1", "a1", "q2", "a2", "q3", "a3"]
    );
}

#[tokio::test]
async fn history_is_identical_with_and_without_a_context_source() {
    let run = |context: Option<Arc<Source>>| async move {
        let repo = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(repo.path()).unwrap();
        let provider = Script::new(vec![tool_round("t1"), end("a1"), end("a2")]);
        let mut session = Session::new(RunConfig::new("test"));
        if let Some(context) = context {
            session = session.with_context(context);
        }
        ask(&mut session, &provider, &executor, "q1").await;
        ask(&mut session, &provider, &executor, "q2").await;
        bytes(session.messages())
    };
    let plain = run(None).await;
    let with = run(Some(Source::new(
        &[Some("B1"), Some("B2")],
        &[Some("PRE"), None],
    )))
    .await;
    assert_eq!(plain, with, "context must be invisible to stored history");
}

#[tokio::test]
async fn without_a_source_or_with_nothing_to_say_requests_are_unchanged() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();

    let provider = Script::new(vec![end("a")]);
    let mut plain = Session::new(RunConfig::new("test"));
    ask(&mut plain, &provider, &executor, "q").await;
    assert_eq!(texts(&provider.requests()[0].messages), ["q"]);

    let provider = Script::new(vec![end("a")]);
    let mut silent =
        Session::new(RunConfig::new("test")).with_context(Source::new(&[None], &[None]));
    ask(&mut silent, &provider, &executor, "q").await;
    assert_eq!(
        texts(&provider.requests()[0].messages),
        ["q"],
        "no empty envelope"
    );
}

#[tokio::test]
async fn the_preamble_is_adopted_once_then_frozen_and_never_stored() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![end("a1"), end("a2"), end("a3"), end("a4")]);
    // Nothing to say for two turns, then a brief, then a different one that must be ignored.
    let source = Source::new(
        &[None, None, None, None],
        &[None, None, Some("BRIEF-A"), Some("BRIEF-B")],
    );
    let mut session = Session::new(RunConfig::new("test")).with_context(source.clone());
    for prompt in ["q1", "q2", "q3", "q4"] {
        ask(&mut session, &provider, &executor, prompt).await;
    }
    let requests = provider.requests();
    assert_eq!(texts(&requests[0].messages), ["q1"]);
    assert_eq!(texts(&requests[1].messages), ["q1", "a1", "q2"]);
    assert_eq!(
        texts(&requests[2].messages)[0],
        "BRIEF-A",
        "adopted when it first existed"
    );
    assert_eq!(
        texts(&requests[3].messages)[0],
        "BRIEF-A",
        "and then frozen"
    );
    assert_eq!(requests[2].messages[0].role, Role::User);
    assert_eq!(
        source.preamble_calls.load(Ordering::SeqCst),
        3,
        "not asked again once adopted"
    );
    assert!(
        !texts(session.messages())
            .iter()
            .any(|t| t.contains("BRIEF"))
    );
}

#[tokio::test]
async fn once_adopted_the_preamble_keeps_the_prefix_identical_across_turns() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![end("a1"), end("a2")]);
    let source = Source::new(&[None, None], &[Some("BRIEF"), Some("OTHER")]);
    let mut session = Session::new(RunConfig::new("test")).with_context(source);
    ask(&mut session, &provider, &executor, "q1").await;
    ask(&mut session, &provider, &executor, "q2").await;
    let requests = provider.requests();
    assert_eq!(
        bytes(&requests[0].messages[..2]),
        bytes(&requests[1].messages[..2])
    );
}

#[tokio::test]
async fn no_request_ever_carries_a_system_message_beyond_the_configured_prompt() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![tool_round("t1"), end("a1"), end("a2")]);
    let source = Source::new(&[Some("B1"), Some("B2")], &[Some("PRE")]);
    let config = RunConfig::new("test");
    let expected_system = vec![config.system_prompt.clone()];
    let mut session = Session::new(config).with_context(source);
    ask(&mut session, &provider, &executor, "q1").await;
    ask(&mut session, &provider, &executor, "q2").await;

    let requests = provider.requests();
    assert!(requests.len() >= 3);
    for request in &requests {
        assert_eq!(request.system, expected_system);
        assert!(
            request.messages.iter().all(|m| m.role != Role::System),
            "repository-derived text must never carry system authority"
        );
    }
}
