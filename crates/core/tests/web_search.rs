use elefante_core::{
    GateDecision, ToolGate,
    tools::{ToolExecutor, WebSearch, WebSearchRequest, definitions},
};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct StubSearch {
    calls: AtomicUsize,
    reply: String,
}

impl WebSearch for StubSearch {
    fn search(&self, request: WebSearchRequest) -> BoxFuture<'_, Result<String, String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!((1..=10).contains(&request.max_results));
        let reply = self.reply.clone();
        Box::pin(async move { Ok(reply) })
    }
}

#[tokio::test]
async fn web_search_validates_queries_before_backend_and_caps_results() {
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(StubSearch {
        calls: AtomicUsize::new(0),
        reply: format!("HEAD 1. https://example.org\n{}TAIL", "x".repeat(70_000)),
    });
    let tools = ToolExecutor::new(root.path())
        .unwrap()
        .with_web_search(backend.clone());
    for input in [
        json!({"query":""}),
        json!({"query":"x".repeat(301)}),
        json!({"query":"https://example.org/path"}),
        json!({"query":"www.example.org"}),
        json!({"query":"SECRET_TOKEN=synthetic"}),
        json!({"query":"rust","max_results":0}),
        json!({"query":"rust","max_results":11}),
    ] {
        assert!(tools.execute("web_search", input).await.is_error);
    }
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    let result = tools
        .execute(
            "web_search",
            json!({"query":"Rust release","max_results":3}),
        )
        .await;
    assert!(!result.is_error);
    assert!(result.content.len() <= 32 * 1024);
    assert!(result.content.contains("HEAD") && result.content.contains("TAIL"));
    assert!(result.content.contains("original "));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(definitions().last().unwrap().name, "write");
    assert_eq!(definitions().len(), 8);
}

#[tokio::test]
async fn web_search_without_a_backend_has_a_fixed_refusal() {
    let root = tempfile::tempdir().unwrap();
    let tools = ToolExecutor::new(root.path()).unwrap();
    let result = tools
        .execute("web_search", json!({"query":"Rust release"}))
        .await;
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "web_search is unavailable: this session has no search backend"
    );
}

#[test]
fn the_thirty_first_search_escalates_across_executor_clones() {
    let root = tempfile::tempdir().unwrap();
    let tools = ToolExecutor::new(root.path()).unwrap();
    let clone = tools.clone();
    let gate = ToolGate::default();
    for _ in 0..30 {
        assert_eq!(
            gate.check_with_tools("web_search", &json!({"query":"rust"}), &tools),
            GateDecision::Allow
        );
    }
    assert_eq!(
        gate.check_with_tools("web_search", &json!({"query":"rust"}), &clone),
        GateDecision::Escalate {
            rule: "web-search-cap",
            command: "rust".into(),
            network: false
        },
    );
}

#[test]
fn parallel_search_reservations_cannot_exceed_the_session_cap() {
    let root = tempfile::tempdir().unwrap();
    let tools = ToolExecutor::new(root.path()).unwrap();
    let gate = ToolGate::default();
    let decisions = std::thread::scope(|scope| {
        let handles = (0..64)
            .map(|_| {
                let tools = tools.clone();
                let gate = gate.clone();
                scope.spawn(move || {
                    gate.check_with_tools("web_search", &json!({"query":"rust"}), &tools)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| **decision == GateDecision::Allow)
            .count(),
        30
    );
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| matches!(
                decision,
                GateDecision::Escalate {
                    rule: "web-search-cap",
                    ..
                }
            ))
            .count(),
        34
    );
}

#[test]
fn rejected_searches_do_not_consume_the_session_cap() {
    let root = tempfile::tempdir().unwrap();
    let tools = ToolExecutor::new(root.path()).unwrap();
    let gate = ToolGate::default();
    for _ in 0..40 {
        for input in [
            json!({"query":""}),
            json!({"query":"https://example.org"}),
            json!({"query":"SECRET_TOKEN=synthetic"}),
        ] {
            assert_eq!(
                gate.check_with_tools("web_search", &input, &tools),
                GateDecision::Allow
            );
        }
    }
    for _ in 0..30 {
        assert_eq!(
            gate.check_with_tools("web_search", &json!({"query":"rust"}), &tools),
            GateDecision::Allow
        );
    }
    assert!(matches!(
        gate.check_with_tools("web_search", &json!({"query":"rust"}), &tools),
        GateDecision::Escalate {
            rule: "web-search-cap",
            ..
        }
    ));
}
