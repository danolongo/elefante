use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use elefante_core::tools::ToolExecutor;
use elefante_core::{
    ApprovalOutcome, ApprovalSink, CancellationToken, OutputEvent, RunConfig, Session, ToolGate,
};
use elefante_provider::{
    Capabilities, ContentBlock, Event, ModelId, Provider, ProviderError, Request, StopReason, Usage,
};
use futures_util::{
    StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

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

fn tool(id: &str, command: &str) -> Vec<Event> {
    vec![
        Event::ToolUseStart {
            id: id.into(),
            name: "bash".into(),
        },
        Event::ToolUseComplete {
            id: id.into(),
            input: json!({"command": command}),
        },
    ]
}

fn end(text: &str) -> Vec<Event> {
    vec![
        Event::TextDelta(text.into()),
        Event::Stop(StopReason::EndTurn),
        Event::Usage(Usage {
            input_tokens: 5,
            output_tokens: 2,
            cost: Some(0.1),
            ..Usage::default()
        }),
    ]
}

#[tokio::test]
async fn history_survives_and_usage_is_per_turn_and_cumulative() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![end("first answer"), end("second answer")]);
    let mut session = Session::new(RunConfig::new("test"));
    for prompt in ["first question", "second question"] {
        let mut events = Vec::new();
        let outcome = session
            .prompt(
                &provider,
                &executor,
                &ToolGate::default(),
                prompt,
                CancellationToken::new(),
                |event| events.push(event),
            )
            .await
            .unwrap();
        assert_eq!(outcome.usage.input_tokens, 5);
        assert!(matches!(events.first(), Some(OutputEvent::TurnStarted)));
        assert!(
            matches!(events.last(), Some(OutputEvent::TurnFinished { usage, .. }) if usage.input_tokens == 5)
        );
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[1].messages.len(), 3);
    assert!(
        matches!(&requests[1].messages[1].content[0], ContentBlock::Text { text } if text == "first answer")
    );
    assert_eq!(session.messages().len(), 4);
    assert_eq!(session.usage().input_tokens, 10);
    assert_eq!(session.usage().cost, Some(0.2));
}

#[tokio::test]
async fn bash_gate_denies_and_escalates_without_execution() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("marker"), "keep").unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = Vec::new();
    for (id, command) in [
        ("deny", "rm marker"),
        ("ask", "touch created"),
        ("redirect", "printf changed > redirected"),
    ] {
        round.extend(tool(id, command));
    }
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("done")]);
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "test",
            CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("marker")).unwrap(),
        "keep"
    );
    assert!(!repo.path().join("created").exists());
    assert!(!repo.path().join("redirected").exists());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, OutputEvent::ApprovalRequested { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                OutputEvent::ApprovalResolved {
                    approved: false,
                    ..
                }
            ))
            .count(),
        2
    );
    assert!(
        session.messages()[2]
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
    );
}

struct ControlledApproval {
    entered: Notify,
    release: Semaphore,
    outcome: ApprovalOutcome,
}

struct PublishedApproval(Mutex<Vec<String>>);

impl ApprovalSink for PublishedApproval {
    fn request<'a>(
        &'a self,
        id: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome> {
        assert!(
            self.0
                .lock()
                .unwrap()
                .iter()
                .any(|published| published == id),
            "approval sink called before event publication"
        );
        Box::pin(async { ApprovalOutcome::Denied })
    }
}

#[tokio::test]
async fn approval_event_is_published_before_sink_is_called() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = tool("first", "touch first");
    round.extend(tool("second", "touch second"));
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("done")]);
    let approvals = Arc::new(PublishedApproval(Mutex::new(Vec::new())));
    let gate = ToolGate::new(approvals.clone());
    let mut session = Session::new(RunConfig::new("test"));
    session
        .prompt(
            &provider,
            &executor,
            &gate,
            "test",
            CancellationToken::new(),
            |event| {
                if let OutputEvent::ApprovalRequested { approval_id, .. } = event {
                    approvals.0.lock().unwrap().push(approval_id);
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(approvals.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn multiple_approvals_are_serialized_without_blocking_safe_tools() {
    check_approval_queue(false).await;
}

#[tokio::test]
async fn cancelling_first_approval_never_publishes_queued_approval() {
    check_approval_queue(true).await;
}

async fn check_approval_queue(cancel_first: bool) {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = tool("first", "touch first");
    round.extend(tool("second", "touch second"));
    round.extend(tool("safe", "printf safe"));
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("done")]);
    let approvals = Arc::new(ControlledApproval {
        entered: Notify::new(),
        release: Semaphore::new(0),
        outcome: ApprovalOutcome::Approved,
    });
    let gate = ToolGate::new(approvals.clone());
    let cancel = CancellationToken::new();
    let safe_finished = Notify::new();
    let mut events = Vec::new();
    let mut session = Session::new(RunConfig::new("test"));
    let run = session.prompt(
        &provider,
        &executor,
        &gate,
        "test",
        cancel.clone(),
        |event| {
            if matches!(&event, OutputEvent::ToolFinished { id, .. } if id == "safe") {
                safe_finished.notify_one();
            }
            events.push(event);
        },
    );
    let control = async {
        approvals.entered.notified().await;
        safe_finished.notified().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(40),
                approvals.entered.notified()
            )
            .await
            .is_err()
        );
        if cancel_first {
            cancel.cancel();
        } else {
            approvals.release.add_permits(1);
            approvals.entered.notified().await;
            approvals.release.add_permits(1);
        }
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run, control)
    })
    .await
    .unwrap();
    result.unwrap();
    let mut pending = None;
    let mut requested = 0;
    for event in &events {
        match event {
            OutputEvent::ApprovalRequested { approval_id, .. } => {
                assert!(
                    pending.replace(approval_id).is_none(),
                    "overlapping approvals"
                );
                requested += 1;
            }
            OutputEvent::ApprovalResolved { approval_id, .. } => {
                assert_eq!(pending.take(), Some(approval_id));
            }
            _ => {}
        }
    }
    assert!(pending.is_none());
    assert_eq!(requested, if cancel_first { 1 } else { 2 });
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, OutputEvent::ToolFinished { .. }))
            .count(),
        3
    );
    assert_eq!(repo.path().join("first").exists(), !cancel_first);
    assert_eq!(repo.path().join("second").exists(), !cancel_first);
}

#[cfg(unix)]
#[tokio::test]
async fn credential_transform_and_symlink_never_reach_execution() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".elefante")).unwrap();
    std::fs::write(
        repo.path().join(".elefante/credentials.toml"),
        "fake-secret-outside-scrubber-patterns",
    )
    .unwrap();
    std::os::unix::fs::symlink(".elefante/credentials.toml", repo.path().join("alias")).unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = tool(
        "credential",
        "sed s/fake/changed/ .elefante/credentials.toml",
    );
    round.extend(tool("alias", "head alias"));
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("done")]);
    let mut session = Session::new(RunConfig::new("test"));
    session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "test",
            CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
    assert!(session.messages()[2].content.iter().all(|block| matches!(block, ContentBlock::ToolResult { content, is_error: true, .. } if content.contains("denied by policy") && !content.contains("outside-scrubber-patterns"))));
}

#[cfg(unix)]
#[tokio::test]
async fn interrupting_running_descendant_preserves_pairing_and_prevents_late_write() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = tool(
        "running",
        "(printf ready > ready; sleep 0.5; printf leaked > delayed) & wait",
    );
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round]);
    let gate = ToolGate::new(Arc::new(ControlledApproval {
        entered: Notify::new(),
        release: Semaphore::new(1),
        outcome: ApprovalOutcome::Approved,
    }));
    let mut session = Session::new(RunConfig::new("test"));
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let run = session.prompt(
        &provider,
        &executor,
        &gate,
        "test",
        cancel.clone(),
        |event| events.push(event),
    );
    let interrupt = async {
        while !repo.path().join("ready").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run, interrupt)
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().stop_reason, StopReason::Interrupted);
    assert!(
        matches!(&session.messages()[2].content[0], ContentBlock::ToolResult { tool_use_id, is_error: true, content } if tool_use_id == "running" && content == "interrupted")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, OutputEvent::ToolFinished { .. }))
            .count(),
        1
    );
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(!repo.path().join("delayed").exists());
}

impl ApprovalSink for ControlledApproval {
    fn request<'a>(
        &'a self,
        _: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome> {
        Box::pin(async move {
            self.entered.notify_one();
            tokio::select! {
                () = cancel.cancelled() => ApprovalOutcome::Cancelled,
                permit = self.release.acquire() => { permit.unwrap().forget(); self.outcome }
            }
        })
    }
}

/// Waits a fixed time, as a person deciding would, and then approves.
struct SlowApproval(Duration);

impl ApprovalSink for SlowApproval {
    fn request<'a>(
        &'a self,
        _: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome> {
        Box::pin(async move {
            tokio::time::sleep(self.0).await;
            ApprovalOutcome::Approved
        })
    }
}

fn finished_duration(events: &[OutputEvent], call: &str) -> u64 {
    events
        .iter()
        .find_map(|event| match event {
            OutputEvent::ToolFinished {
                id, duration_ms, ..
            } if id == call => Some(*duration_ms),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool.finished for {call}"))
}

#[tokio::test]
async fn waiting_for_approval_is_not_counted_in_a_tools_duration() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = tool("ask", "printf x > out");
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("done")]);
    let gate = ToolGate::new(Arc::new(SlowApproval(Duration::from_millis(600))));
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    let began = Instant::now();
    session
        .prompt(
            &provider,
            &executor,
            &gate,
            "test",
            CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();
    assert!(
        began.elapsed() >= Duration::from_millis(600),
        "the approval wait happened"
    );
    assert!(repo.path().join("out").exists(), "and the command then ran");
    let reported = finished_duration(&events, "ask");
    assert!(
        reported < 400,
        "reported {reported} ms, which includes the wait"
    );
}

#[tokio::test]
async fn a_call_that_never_ran_reports_zero() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("marker"), "keep").unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = Vec::new();
    for (id, command) in [("deny", "rm marker"), ("ask", "touch created")] {
        round.extend(tool(id, command));
    }
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("done")]);
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "test",
            CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();
    assert!(!repo.path().join("created").exists());
    assert_eq!(finished_duration(&events, "deny"), 0, "denied by policy");
    assert_eq!(finished_duration(&events, "ask"), 0, "denied by the user");
}

#[tokio::test]
async fn approval_suspends_then_executes_or_denies() {
    for outcome in [ApprovalOutcome::Approved, ApprovalOutcome::Denied] {
        let repo = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(repo.path()).unwrap();
        let mut round = tool("ask", "touch marker");
        round.push(Event::Stop(StopReason::ToolUse));
        let provider = Script::new(vec![round, end("done")]);
        let approvals = Arc::new(ControlledApproval {
            entered: Notify::new(),
            release: Semaphore::new(0),
            outcome,
        });
        let gate = ToolGate::new(approvals.clone());
        let mut session = Session::new(RunConfig::new("test"));
        let mut events = Vec::new();
        let run = session.prompt(
            &provider,
            &executor,
            &gate,
            "test",
            CancellationToken::new(),
            |event| events.push(event),
        );
        let answer = async {
            approvals.entered.notified().await;
            assert!(!repo.path().join("marker").exists());
            approvals.release.add_permits(1);
        };
        let (result, ()) = tokio::join!(run, answer);
        result.unwrap();
        assert_eq!(
            repo.path().join("marker").exists(),
            outcome == ApprovalOutcome::Approved
        );
        assert!(events.iter().any(|event| matches!(event, OutputEvent::ApprovalResolved { approved, .. } if *approved == (outcome == ApprovalOutcome::Approved))));
    }
}

#[tokio::test]
async fn cancellation_keeps_completed_results_and_pairs_pending_approval() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let mut round = vec![
        Event::ToolUseStart {
            id: "ready".into(),
            name: "write".into(),
        },
        Event::ToolUseComplete {
            id: "ready".into(),
            input: json!({"path":"ready", "content":"saved"}),
        },
    ];
    round.extend(tool("pending", "touch forbidden"));
    round.push(Event::Stop(StopReason::ToolUse));
    let provider = Script::new(vec![round, end("resumed")]);
    let approvals = Arc::new(ControlledApproval {
        entered: Notify::new(),
        release: Semaphore::new(0),
        outcome: ApprovalOutcome::Approved,
    });
    let gate = ToolGate::new(approvals);
    let mut session = Session::new(RunConfig::new("test"));
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let result = session
        .prompt(
            &provider,
            &executor,
            &gate,
            "test",
            cancel.clone(),
            |event| {
                if matches!(&event, OutputEvent::ToolFinished { id, .. } if id == "ready") {
                    cancel.cancel();
                }
                events.push(event);
            },
        )
        .await;
    assert_eq!(result.unwrap().stop_reason, StopReason::Interrupted);
    assert!(!repo.path().join("forbidden").exists());
    let results = &session.messages()[2].content;
    assert_eq!(results.len(), 2);
    assert!(
        matches!(&results[0], ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "ready")
    );
    assert!(
        matches!(&results[1], ContentBlock::ToolResult { tool_use_id, is_error: true, content } if tool_use_id == "pending" && content == "interrupted")
    );
    // Interrupted while still waiting for approval: it never ran, so it has no duration.
    assert_eq!(finished_duration(&events, "pending"), 0);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                OutputEvent::ApprovalResolved {
                    approved: false,
                    ..
                }
            ))
            .count(),
        1
    );
    session
        .prompt(
            &provider,
            &executor,
            &gate,
            "continue",
            CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(provider.requests.lock().unwrap()[1].messages.len(), 4);
}

/// Runs one scripted tool call against a temp workspace and returns every event and the session.
async fn run_tool(name: &str, input: &str, files: &[(&str, &str)]) -> (Vec<OutputEvent>, Session) {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    for (path, content) in files {
        std::fs::write(repo.path().join(path), content).unwrap();
    }
    let provider = Script::new(vec![
        vec![
            Event::ToolUseStart {
                id: "c".into(),
                name: name.into(),
            },
            Event::ToolUseInputDelta(input.into()),
            Event::Stop(StopReason::ToolUse),
        ],
        end("done"),
    ]);
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "go",
            CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();
    (events, session)
}

fn previews(events: &[OutputEvent]) -> (String, String) {
    let started = events.iter().find_map(|event| match event {
        OutputEvent::ToolStarted { input_preview, .. } => Some(input_preview.clone()),
        _ => None,
    });
    let finished = events.iter().find_map(|event| match event {
        OutputEvent::ToolFinished { output_preview, .. } => Some(output_preview.clone()),
        _ => None,
    });
    (started.unwrap(), finished.unwrap())
}

#[tokio::test]
async fn a_read_is_previewed_as_its_path_alone_while_the_model_gets_the_whole_file() {
    let (events, session) = run_tool(
        "read",
        "{\"path\":\"notes.txt\"}",
        &[("notes.txt", "one\ntwo\nthree\nfour\n")],
    )
    .await;
    assert_eq!(previews(&events), ("notes.txt".to_owned(), String::new()));
    assert!(matches!(&session.messages()[2].content[0],
        ContentBlock::ToolResult { content, .. } if content == "one\ntwo\nthree\nfour\n"));
}

#[tokio::test]
async fn a_large_write_is_previewed_as_valid_json_with_its_content_clipped() {
    let content = "é".repeat(3000);
    let input = serde_json::json!({"path": "big.txt", "content": content}).to_string();
    let (events, session) = run_tool("write", &input, &[]).await;
    let (preview, _) = previews(&events);
    let parsed: serde_json::Value =
        serde_json::from_str(&preview).expect("the preview is still JSON");
    assert_eq!(parsed["path"], "big.txt");
    let shown = parsed["content"].as_str().unwrap();
    assert!(
        shown.len() <= 512
            && shown.ends_with("\n…")
            && content.starts_with(&shown[..shown.len() - "\n…".len()])
    );
    assert!(matches!(&session.messages()[2].content[0],
        ContentBlock::ToolResult { content, .. } if content == "write complete"));
}

#[tokio::test]
async fn a_small_input_is_previewed_unchanged() {
    let (events, _) = run_tool("grep", "{\"pattern\":\"fn\"}", &[]).await;
    assert_eq!(previews(&events).0, "{\"pattern\":\"fn\"}");
}

#[tokio::test]
async fn a_failed_read_keeps_its_error_so_the_user_sees_why() {
    let (events, _) = run_tool("read", "{\"path\":\"missing.txt\"}", &[]).await;
    let (input, output) = previews(&events);
    assert_eq!(input, "missing.txt");
    assert!(output.starts_with("read failed:"), "{output}");
}

#[tokio::test]
async fn a_read_without_a_string_path_falls_back_to_its_raw_input() {
    let (events, _) = run_tool("read", "{\"path\":7}", &[]).await;
    assert_eq!(previews(&events).0, "{\"path\":7}");
}

#[tokio::test]
async fn tool_started_waits_for_complete_arguments_and_previews_are_bounded() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let text = "é".repeat(3000);
    std::fs::write(repo.path().join("large"), &text).unwrap();
    let provider = Script::new(vec![
        vec![
            Event::ToolUseStart {
                id: "grep".into(),
                name: "grep".into(),
            },
            Event::ToolUseInputDelta("{\"pattern\":".into()),
            Event::TextDelta("before dispatch".into()),
            Event::ToolUseInputDelta("\"é\"}".into()),
            Event::Stop(StopReason::ToolUse),
        ],
        end("done"),
    ]);
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "grep",
            CancellationToken::new(),
            |event| events.push(event),
        )
        .await
        .unwrap();
    assert!(matches!(&events[1], OutputEvent::TextDelta(_)));
    assert!(
        matches!(&events[2], OutputEvent::ToolStarted { input_preview, .. } if input_preview == "{\"pattern\":\"é\"}")
    );
    assert!(events.iter().any(
        |event| matches!(event, OutputEvent::ToolFinished { output_preview, .. }
        if output_preview.len() <= 2048 && output_preview.ends_with("\n…"))
    ));
    assert!(
        matches!(&session.messages()[2].content[0], ContentBlock::ToolResult { content, .. } if content.len() > 2048 && content.contains(&text))
    );
}

#[tokio::test]
async fn abnormal_end_emits_error_then_finished_and_retains_reported_usage() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![vec![Event::Usage(Usage {
        input_tokens: 7,
        ..Usage::default()
    })]]);
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    assert!(
        session
            .prompt(
                &provider,
                &executor,
                &ToolGate::default(),
                "test",
                CancellationToken::new(),
                |event| events.push(event)
            )
            .await
            .is_err()
    );
    assert!(matches!(
        &events[events.len() - 2],
        OutputEvent::Error { .. }
    ));
    assert!(
        matches!(&events[events.len()-1], OutputEvent::TurnFinished { usage, .. } if usage.input_tokens == 7)
    );
    assert_eq!(session.usage().input_tokens, 7);
}

#[test]
fn interrupted_stop_reason_has_the_frozen_wire_spelling() {
    assert_eq!(
        serde_json::to_value(StopReason::Interrupted).unwrap(),
        json!("interrupted")
    );
}

#[tokio::test]
async fn cancellation_during_provider_stream_preserves_partial_text_and_usage() {
    let repo = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(repo.path()).unwrap();
    let provider = Script::new(vec![
        vec![
            Event::Usage(Usage {
                input_tokens: 3,
                ..Usage::default()
            }),
            Event::TextDelta("partial".into()),
            Event::Stop(StopReason::EndTurn),
        ],
        end("resumed"),
    ]);
    let mut session = Session::new(RunConfig::new("test"));
    let cancel = CancellationToken::new();
    let outcome = session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "test",
            cancel.clone(),
            |event| {
                if matches!(event, OutputEvent::TextDelta(_)) {
                    cancel.cancel();
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Interrupted);
    assert_eq!(outcome.text, "partial");
    assert_eq!(outcome.usage.input_tokens, 3);
    assert!(
        matches!(&session.messages()[1].content[0], ContentBlock::Text { text } if text == "partial")
    );
    session
        .prompt(
            &provider,
            &executor,
            &ToolGate::default(),
            "continue",
            CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
}

struct CountedApproval {
    asked: std::sync::atomic::AtomicUsize,
    outcome: ApprovalOutcome,
}

impl ApprovalSink for CountedApproval {
    fn request<'a>(
        &'a self,
        _: &'a str,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { self.outcome })
    }
}

#[derive(Debug, Default)]
struct CountedSearch(std::sync::atomic::AtomicUsize);

impl elefante_core::tools::WebSearch for CountedSearch {
    fn search(
        &self,
        _: elefante_core::tools::WebSearchRequest,
    ) -> BoxFuture<'_, Result<String, String>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("1. https://example.org".to_owned()) })
    }
}

fn searches(ids: &[&str]) -> Vec<Event> {
    let mut round = Vec::new();
    for id in ids {
        round.push(Event::ToolUseStart {
            id: (*id).into(),
            name: "web_search".into(),
        });
        round.push(Event::ToolUseComplete {
            id: (*id).into(),
            input: json!({"query": "rust"}),
        });
    }
    round.push(Event::Stop(StopReason::ToolUse));
    round
}

#[tokio::test]
async fn the_first_search_asks_in_every_call_and_one_approval_opens_the_session() {
    use std::sync::atomic::Ordering::SeqCst;
    let repo = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountedSearch::default());
    let executor = ToolExecutor::new(repo.path())
        .unwrap()
        .with_web_search(backend.clone());
    let provider = Script::new(vec![
        searches(&["a", "b"]),
        end("one"),
        searches(&["c"]),
        end("two"),
    ]);
    let approvals = Arc::new(CountedApproval {
        asked: 0.into(),
        outcome: ApprovalOutcome::Approved,
    });
    let gate = ToolGate::new(approvals.clone());
    let mut session = Session::new(RunConfig::new("test"));
    let mut events = Vec::new();
    for prompt in ["first", "second"] {
        session
            .prompt(
                &provider,
                &executor,
                &gate,
                prompt,
                CancellationToken::new(),
                |event| events.push(event),
            )
            .await
            .unwrap();
    }
    // Both searches of the first batch asked; the later one did not.
    assert_eq!(approvals.asked.load(SeqCst), 2);
    assert_eq!(backend.0.load(SeqCst), 3);
    assert!(events.iter().any(|event| matches!(event, OutputEvent::ApprovalRequested { rule: "web-search-first", command, .. } if command == "rust")));
}

#[tokio::test]
async fn a_denied_search_takes_no_slot_and_approves_nothing() {
    use std::sync::atomic::Ordering::SeqCst;
    let repo = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountedSearch::default());
    let executor = ToolExecutor::new(repo.path())
        .unwrap()
        .with_web_search(backend.clone());
    let provider = Script::new(vec![
        searches(&["a"]),
        end("one"),
        searches(&["b"]),
        end("two"),
    ]);
    let approvals = Arc::new(CountedApproval {
        asked: 0.into(),
        outcome: ApprovalOutcome::Denied,
    });
    let gate = ToolGate::new(approvals.clone());
    let mut session = Session::new(RunConfig::new("test"));
    for prompt in ["first", "second"] {
        session
            .prompt(
                &provider,
                &executor,
                &gate,
                prompt,
                CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
    }
    assert_eq!(approvals.asked.load(SeqCst), 2);
    assert_eq!(backend.0.load(SeqCst), 0);
    assert!(!executor.web_search_approved());
    // 29 reservations remain after the next approval; an unused slot would leave 28.
    executor.approve_web_search();
    for _ in 0..29 {
        assert!(executor.reserve_web_search());
    }
    assert!(!executor.reserve_web_search());
}

#[tokio::test]
async fn a_cancelled_search_approval_runs_nothing_and_the_next_search_asks_again() {
    use std::sync::atomic::Ordering::SeqCst;
    let repo = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountedSearch::default());
    let executor = ToolExecutor::new(repo.path())
        .unwrap()
        .with_web_search(backend.clone());
    let provider = Script::new(vec![
        searches(&["a"]),
        end("one"),
        searches(&["b"]),
        end("two"),
    ]);
    let approvals = Arc::new(CountedApproval {
        asked: 0.into(),
        outcome: ApprovalOutcome::Cancelled,
    });
    let gate = ToolGate::new(approvals.clone());
    let mut session = Session::new(RunConfig::new("test"));
    for prompt in ["first", "second"] {
        session
            .prompt(
                &provider,
                &executor,
                &gate,
                prompt,
                CancellationToken::new(),
                |_| {},
            )
            .await
            .unwrap();
    }
    assert_eq!(approvals.asked.load(SeqCst), 2);
    assert_eq!(backend.0.load(SeqCst), 0);
    assert!(!executor.web_search_approved());
}
