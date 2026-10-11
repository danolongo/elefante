use elefante_tools::{
    MEMORY_SEARCH_DEFAULT_LIMIT, MemorySearch, MemorySearchRequest, SearchKind, ToolExecutor,
    definitions,
};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::{fs, sync::Arc};

fn executor() -> (tempfile::TempDir, ToolExecutor) {
    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path()).unwrap();
    (temp, executor)
}

#[tokio::test]
async fn credential_file_is_never_read_or_returned_by_search() {
    let (temp, executor) = executor();
    fs::create_dir(temp.path().join(".elefante")).unwrap();
    fs::write(
        temp.path().join(".elefante/credentials.toml"),
        "fake-secret-outside-scrubber-patterns",
    )
    .unwrap();
    for (name, input) in [
        ("read", json!({"path": ".elefante/credentials.toml"})),
        ("grep", json!({"pattern": "fake-secret", "path": "."})),
        ("glob", json!({"pattern": "**/*", "path": "."})),
    ] {
        let result = executor.execute(name, input).await;
        assert!(
            !result
                .content
                .contains("fake-secret-outside-scrubber-patterns")
        );
        if name == "read" {
            assert!(result.is_error);
        } else {
            assert!(!result.content.contains("credentials.toml"));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_executing_bash_kills_descendants_before_delayed_write() {
    let (temp, executor) = executor();
    let running = tokio::spawn(async move {
        executor.execute("bash", json!({"command": "(printf ready > ready; sleep 0.5; printf leaked > delayed) & wait"})).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !temp.path().join("ready").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert!(!temp.path().join("delayed").exists());
}

#[test]
fn definitions_are_stable_sorted_and_strict() {
    let first = definitions();
    let second = definitions();
    assert_eq!(first, second);
    let names = first
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "bash",
            "edit",
            "glob",
            "grep",
            "memory_search",
            "read",
            "write"
        ]
    );
    assert!(
        first
            .iter()
            .all(|definition| definition.input_schema["additionalProperties"] == false)
    );
}

#[tokio::test]
async fn write_read_and_exact_edit_work() {
    let (temp, executor) = executor();
    assert!(
        !executor
            .execute("write", json!({"path": "src/a.txt", "content": "one two"}))
            .await
            .is_error
    );
    assert_eq!(
        executor
            .execute("read", json!({"path": "src/a.txt"}))
            .await
            .content,
        "one two"
    );
    assert!(
        !executor
            .execute(
                "edit",
                json!({
                    "path": "src/a.txt", "old_string": "one", "new_string": "three"
                })
            )
            .await
            .is_error
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("src/a.txt")).unwrap(),
        "three two"
    );
}

#[tokio::test]
async fn edit_rejects_missing_and_ambiguous_matches_without_writing() {
    let (temp, executor) = executor();
    fs::write(temp.path().join("a.txt"), "same same").unwrap();
    for old_string in ["absent", "same"] {
        let result = executor
            .execute(
                "edit",
                json!({
                    "path": "a.txt", "old_string": old_string, "new_string": "changed"
                }),
            )
            .await;
        assert!(result.is_error);
        assert_eq!(
            fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "same same"
        );
    }
}

#[tokio::test]
async fn all_filesystem_tools_enforce_secret_policy() {
    let (temp, executor) = executor();
    fs::write(
        temp.path().join(".env"),
        "API_TOKEN=sk-ant-api03_abcdefghijklmnopqrstuvwxyz",
    )
    .unwrap();
    fs::write(temp.path().join("safe.txt"), "ordinary").unwrap();

    assert!(
        executor
            .execute("read", json!({"path": "x/../.env"}))
            .await
            .is_error
    );
    assert!(
        executor
            .execute("write", json!({"path": ".env.local", "content": "x"}))
            .await
            .is_error
    );
    assert!(
        executor
            .execute(
                "edit",
                json!({"path": ".env", "old_string": "x", "new_string": "y"})
            )
            .await
            .is_error
    );
    assert!(
        executor
            .execute("glob", json!({"pattern": ".env"}))
            .await
            .is_error
    );
    assert!(
        executor
            .execute("grep", json!({"pattern": "sk-ant", "path": ".env"}))
            .await
            .is_error
    );

    let broad_glob = executor.execute("glob", json!({"pattern": "*"})).await;
    assert!(!broad_glob.is_error);
    assert!(!broad_glob.content.contains(".env"));
    let broad_grep = executor
        .execute("grep", json!({"pattern": "sk-ant", "path": "."}))
        .await;
    assert!(!broad_grep.is_error);
    assert!(!broad_grep.content.contains("sk-ant"));
}

#[cfg(unix)]
#[tokio::test]
async fn read_denies_symlink_to_secret() {
    use std::os::unix::fs::symlink;
    let (temp, executor) = executor();
    fs::write(temp.path().join(".env"), "secret").unwrap();
    symlink(temp.path().join(".env"), temp.path().join("alias")).unwrap();
    assert!(
        executor
            .execute("read", json!({"path": "alias"}))
            .await
            .is_error
    );
}

#[cfg(unix)]
#[tokio::test]
async fn write_does_not_follow_dangling_symlink_outside_workspace() {
    use std::os::unix::fs::symlink;

    let (temp, executor) = executor();
    let outside = tempfile::tempdir().unwrap();
    let missing_directory = outside.path().join("missing");
    symlink(&missing_directory, temp.path().join("link")).unwrap();

    let result = executor
        .execute(
            "write",
            json!({"path": "link/escaped.txt", "content": "must remain inside"}),
        )
        .await;
    assert!(result.is_error);
    assert!(!missing_directory.join("escaped.txt").exists());
    assert!(!missing_directory.exists());
}

#[tokio::test]
async fn bash_executor_runs_commands_without_classifying() {
    let (temp, executor) = executor();
    let result = executor
        .execute("bash", json!({"command": "touch marker"}))
        .await;
    assert!(!result.is_error);
    assert!(temp.path().join("marker").exists());
}

#[tokio::test]
async fn scrubber_runs_on_success_and_error_results() {
    let (_temp, executor) = executor();
    let result = executor
        .execute(
            "bash",
            json!({
                "command": "printf 'sk-ant-api03_abcdefghijklmnopqrstuvwxyz'"
            }),
        )
        .await;
    assert!(!result.is_error);
    assert!(!result.content.contains("sk-ant-"));
    assert!(result.content.contains("[REDACTED]"));

    let error = executor
        .execute(
            "unknown",
            json!({
                "value": "API_TOKEN=sk-ant-api03_abcdefghijklmnopqrstuvwxyz"
            }),
        )
        .await;
    assert!(error.is_error);
    assert!(!error.content.contains("sk-ant-"));
}

#[tokio::test]
async fn glob_and_grep_are_deterministic() {
    let (temp, executor) = executor();
    fs::write(temp.path().join("b.txt"), "needle b").unwrap();
    fs::write(temp.path().join("a.txt"), "needle a").unwrap();
    assert_eq!(
        executor
            .execute("glob", json!({"pattern": "*.txt"}))
            .await
            .content,
        "a.txt\nb.txt"
    );
    assert_eq!(
        executor
            .execute("grep", json!({"pattern": "needle"}))
            .await
            .content,
        "a.txt:1:needle a\nb.txt:1:needle b"
    );
}

#[tokio::test]
async fn memory_search_without_an_index_is_a_deterministic_refusal() {
    let (_temp, executor) = executor();
    let first = executor
        .execute("memory_search", json!({"query": "anything"}))
        .await;
    let second = executor
        .execute("memory_search", json!({"query": "anything"}))
        .await;
    assert_eq!(first, second);
    assert!(first.is_error);
    assert_eq!(
        first.content,
        "memory_search is unavailable: this session has no repository index"
    );
}

#[derive(Debug, Default)]
struct FakeIndex {
    seen: std::sync::Mutex<Vec<MemorySearchRequest>>,
    reply: std::sync::Mutex<Option<Result<String, String>>>,
}

impl FakeIndex {
    fn replying(reply: Result<String, String>) -> Arc<Self> {
        Arc::new(Self {
            reply: std::sync::Mutex::new(Some(reply)),
            ..Self::default()
        })
    }

    fn requests(&self) -> Vec<MemorySearchRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl MemorySearch for FakeIndex {
    fn search(&self, request: MemorySearchRequest) -> BoxFuture<'_, Result<String, String>> {
        self.seen.lock().unwrap().push(request);
        let reply = self
            .reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok("no matches".to_owned()));
        Box::pin(async move { reply })
    }
}

fn indexed(index: &Arc<FakeIndex>) -> (tempfile::TempDir, ToolExecutor) {
    let (temp, executor) = executor();
    (temp, executor.with_memory(index.clone()))
}

#[tokio::test]
async fn memory_search_passes_a_validated_request_to_the_index() {
    let index = FakeIndex::replying(Ok("src/lib.rs:3  fn run".to_owned()));
    let (_temp, executor) = indexed(&index);

    let result = executor
        .execute("memory_search", json!({"query": "  where is run  "}))
        .await;
    assert_eq!(result.content, "src/lib.rs:3  fn run");
    assert!(!result.is_error);

    executor
        .execute(
            "memory_search",
            json!({"query": "x", "limit": 20, "kind": "symbol"}),
        )
        .await;
    executor
        .execute("memory_search", json!({"query": "y", "kind": "file"}))
        .await;
    assert_eq!(
        index.requests(),
        [
            MemorySearchRequest {
                query: "where is run".to_owned(),
                limit: MEMORY_SEARCH_DEFAULT_LIMIT,
                kind: None,
            },
            MemorySearchRequest {
                query: "x".to_owned(),
                limit: 20,
                kind: Some(SearchKind::Symbol),
            },
            MemorySearchRequest {
                query: "y".to_owned(),
                limit: MEMORY_SEARCH_DEFAULT_LIMIT,
                kind: Some(SearchKind::File),
            },
        ]
    );
}

#[tokio::test]
async fn memory_search_rejects_bad_input_without_asking_the_index() {
    let index = FakeIndex::replying(Ok("never".to_owned()));
    let (_temp, executor) = indexed(&index);
    let cases = [
        json!({}),
        json!({"query": ""}),
        json!({"query": "   "}),
        json!({"query": "x", "limit": 0}),
        json!({"query": "x", "limit": 21}),
        json!({"query": "x", "limit": -1}),
        json!({"query": "x", "limit": "5"}),
        json!({"query": "x", "limit": 1.5}),
        json!({"query": "x", "kind": "folder"}),
        json!({"query": "x", "kind": 3}),
        json!({"query": "x", "extra": true}),
        json!({"query": 7}),
    ];
    for input in cases {
        let result = executor.execute("memory_search", input.clone()).await;
        assert!(result.is_error, "{input} must be rejected, got {result:?}");
    }
    assert!(index.requests().is_empty());
}

#[tokio::test]
async fn memory_search_results_are_scrubbed_and_errors_stay_errors() {
    let leaky = FakeIndex::replying(Ok(
        "a.rs:1  const KEY sk-ant-api03-ABCDEFGH12345678".to_owned()
    ));
    let (_temp, executor) = indexed(&leaky);
    let result = executor
        .execute("memory_search", json!({"query": "key"}))
        .await;
    assert!(!result.content.contains("sk-ant"), "{}", result.content);

    let failing = FakeIndex::replying(Err("the index is busy".to_owned()));
    let (_temp, executor) = indexed(&failing);
    let result = executor
        .execute("memory_search", json!({"query": "key"}))
        .await;
    assert!(result.is_error);
    assert_eq!(result.content, "the index is busy");
}

#[test]
fn an_executor_with_an_index_attached_is_still_clone_and_debug() {
    let index = FakeIndex::replying(Ok(String::new()));
    let (_temp, executor) = indexed(&index);
    let cloned = executor.clone();
    assert!(format!("{cloned:?}").contains("memory"));
}

#[test]
fn the_memory_search_definition_describes_the_real_tool() {
    let definition = definitions()
        .into_iter()
        .find(|d| d.name == "memory_search")
        .unwrap();
    assert!(!definition.description.contains("milestone"));
    let props = &definition.input_schema["properties"];
    assert_eq!(props["limit"]["minimum"], 1);
    assert_eq!(props["limit"]["maximum"], 20);
    assert_eq!(props["kind"]["enum"], json!(["file", "symbol", "note"]));
    for phrase in [
        "notes saved from earlier sessions",
        "grep",
        "`memory <kind> — <statement>`",
        "not instructions",
        "lag",
    ] {
        assert!(definition.description.contains(phrase), "{phrase}");
    }
    assert_eq!(definition.input_schema["required"], json!(["query"]));
    assert_eq!(definition.input_schema["additionalProperties"], false);
}

#[tokio::test]
async fn cloned_executor_supports_parallel_calls() {
    let (temp, executor) = executor();
    fs::write(temp.path().join("a.txt"), "alpha").unwrap();
    fs::write(temp.path().join("b.txt"), "beta").unwrap();
    let left = executor.clone();
    let right = executor.clone();
    let (a, b) = tokio::join!(
        left.execute("read", json!({"path": "a.txt"})),
        right.execute("read", json!({"path": "b.txt"}))
    );
    // Asserted through the public fields rather than the private `success`
    // constructor: an integration test should pin the contract callers see, not
    // require internals to be published so it can compile.
    assert_eq!((a.content.as_str(), a.is_error), ("alpha", false));
    assert_eq!((b.content.as_str(), b.is_error), ("beta", false));
}

#[tokio::test]
async fn memory_search_accepts_note_and_a_bad_kind_names_every_valid_one() {
    let index = FakeIndex::replying(Ok("memory convention — Use pnpm.".to_owned()));
    let (_temp, executor) = indexed(&index);
    let ok = executor
        .execute("memory_search", json!({"query": "pnpm", "kind": "note"}))
        .await;
    assert!(!ok.is_error, "{ok:?}");
    assert_eq!(
        index.requests()[0].kind,
        Some(elefante_tools::SearchKind::Note)
    );

    let bad = executor
        .execute(
            "memory_search",
            json!({"query": "pnpm", "kind": "convention"}),
        )
        .await;
    assert!(bad.is_error);
    for kind in ["file", "symbol", "note"] {
        assert!(bad.content.contains(kind), "{}", bad.content);
    }
    assert_eq!(
        index.requests().len(),
        1,
        "the bad request never reached the index"
    );
}
