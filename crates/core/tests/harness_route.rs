use elefante_core::tools::ToolExecutor;
use elefante_guard::git::Identity;
use serde_json::json;
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("fixture Git launches");
    assert!(
        output.status.success(),
        "fixture Git: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn approved_git_writes_route_through_harness_and_read_git_stays_sandboxed() {
    let root = tempfile::tempdir().expect("fixture root");
    git(root.path(), &["init"]);
    fs::write(root.path().join("ordinary.txt"), "fixture").expect("fixture file");
    let tools = ToolExecutor::new(root.path())
        .expect("tool session")
        .with_git_identity(Identity {
            name: "Fixture".into(),
            email: "fixture@example.invalid".into(),
        })
        .expect("harness Git setup");
    let add = tools
        .execute("bash", json!({"command":"git add ordinary.txt"}))
        .await;
    assert!(!add.is_error, "{}", add.content);
    let commit = tools
        .execute("bash", json!({"command":"git commit -m fixture"}))
        .await;
    assert!(!commit.is_error, "{}", commit.content);
    let log = tools
        .execute("bash", json!({"command":"git log -1 --oneline"}))
        .await;
    assert!(!log.is_error, "{}", log.content);
    assert!(log.content.contains("fixture"));
    let push = tools.execute("bash", json!({"command":"git push"})).await;
    assert!(push.is_error);
    assert!(push.content.contains("run git write commands on their own"));
}

#[tokio::test]
async fn git_metadata_cannot_be_a_session_root() {
    use elefante_core::sandbox::{Sandbox, SessionSandbox};
    use elefante_guard::sandbox::Network;
    let root = tempfile::tempdir().expect("fixture root");
    git(root.path(), &["init"]);
    let metadata = root
        .path()
        .join(".git")
        .canonicalize()
        .expect("Git metadata root");
    let control = Command::new("/bin/bash")
        .args(["-c", "printf host > control"])
        .current_dir(&metadata)
        .output()
        .expect("unsandboxed control launches");
    assert!(
        control.status.success(),
        "unsandboxed metadata write succeeds"
    );
    let session = SessionSandbox::new(&metadata).expect("unavailable state is representable");
    assert!(matches!(session.state(), Sandbox::Unavailable(_)));
    let error = session
        .run_shell("printf forbidden > marker", Network::Off)
        .await
        .expect_err("Git metadata root must not run shell commands");
    assert!(error.contains("sandbox unavailable"), "{error}");
    assert!(!metadata.join("marker").exists());
}

#[tokio::test]
async fn sandboxed_git_metadata_write_explains_the_harness_route() {
    let root = tempfile::tempdir().expect("fixture root");
    git(root.path(), &["init"]);
    let tools = ToolExecutor::new(root.path()).expect("tool session");
    let result = tools
        .execute("bash", json!({"command":"printf marker > .git/blocked"}))
        .await;
    assert!(result.is_error, "metadata write unexpectedly worked");
    assert!(
        result
            .content
            .contains("run git write commands on their own"),
        "{}",
        result.content
    );
    assert!(!root.path().join(".git/blocked").exists());
}

#[tokio::test]
async fn git_diff_output_cannot_escape_the_workspace() {
    use elefante_core::sandbox::SessionSandbox;
    use elefante_guard::sandbox::Network;
    let root = tempfile::tempdir().expect("fixture repository");
    git(root.path(), &["init"]);
    fs::write(root.path().join("tracked"), "before\n").expect("tracked file");
    git(root.path(), &["add", "tracked"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    fs::write(root.path().join("tracked"), "after\n").expect("modified file");
    let outside = tempfile::tempdir().expect("outside fixture");
    let output = outside.path().join("diff.patch");
    let source = format!("git diff --output='{}'", output.display());
    let host = Command::new("/bin/bash")
        .args(["-c", &source])
        .current_dir(root.path())
        .output()
        .expect("unsandboxed Git control");
    assert!(host.status.success(), "Git diff control succeeds");
    assert!(output.exists(), "Git diff control writes outside");
    fs::remove_file(&output).expect("clear only the scratch control output");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("sandbox session");
    let blocked = session
        .run_shell(&source, Network::Off)
        .await
        .expect("sandbox executes");
    assert!(
        !blocked.success,
        "outside diff write succeeded: {}",
        blocked.content
    );
    assert!(!output.exists(), "outside diff file was created");
}
