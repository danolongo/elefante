//! Startup failures must be named, not transport noise.
//!
//! These run the real binary so they cover argument parsing, environment
//! resolution, and exit codes together. None of them reach the network: every
//! case bails before a provider request is built.

use std::process::{Command, Output};

/// Runs the binary with a prompt argument so it never blocks reading stdin,
/// and with every relevant variable cleared so an operator's real environment
/// cannot make the test pass or fail by accident.
fn run(args: &[&str], set: &[(&str, &str)]) -> Output {
    let isolated = tempfile::tempdir().expect("isolated daemon home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_elefante"));
    command
        .args(args)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_BASE_URL")
        .env_remove("OPENROUTER_MODEL")
        .env_remove("ELEFANTE_EFFORT")
        .env_remove("ELEFANTE_STUB_PROVIDER")
        .env("HOME", isolated.path())
        .env("ELEFANTE_SOCKET", isolated.path().join("run/elefante.sock"))
        .env("ELEFANTE_IDLE_SECS", "1");
    for (key, value) in set {
        command.env(key, value);
    }
    command.output().expect("binary runs")
}

#[test]
fn missing_api_key_is_a_named_error_not_a_transport_failure() {
    let output = run(&["summarize this repository"], &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "missing credentials must not exit 0"
    );
    assert!(
        stderr.contains("OPENROUTER_API_KEY is not set"),
        "error must name the missing variable, got: {stderr}"
    );
    // The failure has to happen before any request is attempted, so the operator
    // is told what to fix instead of reading a connection or 401 error.
    assert!(
        !stderr.contains("error sending request") && !stderr.contains("401"),
        "must fail before contacting the provider, got: {stderr}"
    );
}

#[test]
fn invalid_base_url_is_a_named_error() {
    let output = run(
        &["summarize this repository"],
        &[
            ("OPENROUTER_API_KEY", "test-key"),
            ("OPENROUTER_BASE_URL", "not-a-url"),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("OPENROUTER_BASE_URL is invalid"),
        "error must name the offending variable, got: {stderr}"
    );
}

#[test]
fn an_empty_prompt_reports_usage_rather_than_calling_the_provider() {
    let output = run(&["   "], &[("OPENROUTER_API_KEY", "test-key")]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(
        stderr.contains("prompt cannot be empty") && stderr.contains("usage: elefante"),
        "empty prompt must explain itself and show usage, got: {stderr}"
    );
}

/// End-to-end against a real model: the loop must complete and the edit must land.
///
/// Ignored because it issues real, billable requests. Run it deliberately:
///
/// ```sh
/// export OPENROUTER_API_KEY="..."
/// export OPENROUTER_MODEL="..."   # optional; a cheap tool-capable slug is fine
/// cargo test -p elefante-cli -- --ignored --nocapture
/// ```
///
/// It runs in a temporary directory rather than this repository, so a model that
/// misreads the instruction cannot touch tracked files.
#[test]
#[ignore = "requires OpenRouter credentials, network, and billable requests"]
fn live_edit_lands_in_a_scratch_repository() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
    let workspace = tempfile::tempdir().expect("temp dir");
    let note = workspace.path().join("note.txt");
    std::fs::write(&note, "The status is hello.\n").expect("seed file");

    let mut command = Command::new(env!("CARGO_BIN_EXE_elefante"));
    command
        .current_dir(workspace.path())
        .env(
            "ELEFANTE_SOCKET",
            workspace.path().join("run/elefante.sock"),
        )
        .env("ELEFANTE_IDLE_SECS", "1")
        .env_remove("ELEFANTE_STUB_PROVIDER")
        .env("OPENROUTER_API_KEY", api_key)
        // Bound what this test can spend. The production defaults (16k tokens,
        // high effort) are sized for real work; a one-word edit needs neither,
        // and reserving 16k output tokens makes the test fail on any
        // credit-limited key for reasons that have nothing to do with the code.
        .args([
            "--max-tokens",
            "2048",
            "--effort",
            "low",
            "--max-rounds",
            "8",
        ])
        .arg("In note.txt, replace the word hello with goodbye. Change nothing else.");
    if let Ok(model) = std::env::var("OPENROUTER_MODEL") {
        command.env("OPENROUTER_MODEL", model);
    }
    let output = command.output().expect("binary runs");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "loop did not complete: {stderr}");

    let contents = std::fs::read_to_string(&note).expect("note is readable");
    assert!(
        contents.contains("goodbye") && !contents.contains("hello"),
        "edit did not land, file reads: {contents:?}\nstderr: {stderr}"
    );
}

/// The repository index reaches a real model, in the shape a real provider accepts.
///
/// The scratch repository is indexed *before* the daemon starts, so the first prompt cannot race
/// the background indexer. `--max-rounds 1` then makes the answer depend on context alone: a model
/// that reaches for a tool exhausts its single round and fails the run, so the only way to name a
/// function it has never seen is to have read the block and brief it was sent. A hermetic test
/// proves the request is built correctly; only a live one proves a provider accepts it.
#[test]
#[ignore = "requires OpenRouter credentials, network, and billable requests"]
fn live_memory_context_reaches_the_model() {
    let api_key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY must be set");
    let workspace = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir(workspace.path().join(".git")).expect("repository marker");
    std::fs::create_dir(workspace.path().join("src")).expect("src");
    std::fs::write(
        workspace.path().join("src/quartz.rs"),
        "//! Quartz polishing.\n\n/// Polishes quartz crystals until they gleam.\npub fn gleam_quartz_crystals() {}\n",
    )
    .expect("seed file");
    std::fs::write(
        workspace.path().join("src/other.rs"),
        "//! Unrelated helpers.\n\n/// Adds two numbers.\npub fn add_numbers() {}\n",
    )
    .expect("decoy file");

    let repo = elefante_mem::RepoId::resolve_with_home(workspace.path(), None).expect("repo");
    let store = elefante_mem::Store::open(&repo).expect("store");
    elefante_mem::indexer::refresh_blocking(
        &store,
        repo.root(),
        &elefante_mem::Counters::default(),
        &elefante_mem::indexer::IndexOptions::default(),
    )
    .expect("pre-index");
    drop(store);

    let mut command = Command::new(env!("CARGO_BIN_EXE_elefante"));
    command
        .current_dir(workspace.path())
        .env(
            "ELEFANTE_SOCKET",
            workspace.path().join("run/elefante.sock"),
        )
        .env("ELEFANTE_IDLE_SECS", "1")
        .env_remove("ELEFANTE_STUB_PROVIDER")
        .env("OPENROUTER_API_KEY", api_key)
        .args([
            "--max-tokens",
            "2048",
            "--effort",
            "low",
            "--max-rounds",
            "1",
        ])
        .arg(
            "Do not use any tools. Using only the repository index you were given, which function \
             polishes quartz crystals? Reply with just the function name.",
        );
    if let Ok(model) = std::env::var("OPENROUTER_MODEL") {
        command.env("OPENROUTER_MODEL", model);
    }
    let output = command.output().expect("binary runs");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the turn did not complete: {stderr}"
    );
    let answer = String::from_utf8_lossy(&output.stdout);
    assert!(
        answer.contains("gleam_quartz_crystals"),
        "the model did not use the index it was sent.\nanswer: {answer:?}\nstderr: {stderr}"
    );
}

#[test]
fn headless_stream_keeps_reasoning_out_of_redirected_stdout() {
    let isolated = tempfile::tempdir().expect("isolated daemon");
    let script = isolated.path().join("events.json");
    std::fs::write(&script, r#"[{"ReasoningDelta":"Let me "},{"ReasoningDelta":"think."},{"TextDelta":"The "},{"TextDelta":"answer.\n"},{"Stop":"end_turn"}]"#).unwrap();
    let output = run(
        &["answer"],
        &[
            ("ELEFANTE_STUB_PROVIDER", script.to_str().unwrap()),
            ("NO_COLOR", "1"),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"The answer.\n");
    assert_eq!(output.stderr, b"Let me think.\n");
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}
