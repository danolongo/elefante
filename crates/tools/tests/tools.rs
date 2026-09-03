use elefante_tools::{ToolExecutor, definitions};
use serde_json::json;
use std::fs;

fn executor() -> (tempfile::TempDir, ToolExecutor) {
    let temp = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(temp.path()).unwrap();
    (temp, executor)
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
async fn bash_gate_denies_and_escalates_without_execution() {
    let (temp, executor) = executor();
    for command in ["cat .env", "rm marker", "env"] {
        assert!(
            executor
                .execute("bash", json!({"command": command}))
                .await
                .is_error
        );
    }
    let escalated = executor
        .execute("bash", json!({"command": "touch marker"}))
        .await;
    assert!(escalated.is_error);
    assert!(escalated.content.contains("approval"));
    assert!(!temp.path().join("marker").exists());

    let redirected = executor
        .execute("bash", json!({"command": "printf changed > redirected"}))
        .await;
    assert!(redirected.is_error);
    assert!(redirected.content.contains("approval"));
    assert!(!temp.path().join("redirected").exists());
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
async fn memory_search_is_a_deterministic_m2_stub() {
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
        "memory_search is unavailable until milestone M2"
    );
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
