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
    let mut command = Command::new(env!("CARGO_BIN_EXE_elefante"));
    command
        .args(args)
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_BASE_URL")
        .env_remove("OPENROUTER_MODEL")
        .env_remove("ELEFANTE_EFFORT");
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
