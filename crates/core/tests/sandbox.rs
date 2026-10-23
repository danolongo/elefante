//! Bounded process and real OS isolation regressions; missing isolation is a test failure.
use elefante_core::sandbox::run_process;
use elefante_guard::sandbox::Network;
use elefante_guard::{BashClassifier, Verdict};
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

fn shell(source: &str) -> Command {
    let mut command = Command::new("/bin/bash");
    command.args(["-c", source]).stdin(Stdio::null());
    command
}

#[tokio::test]
async fn captures_both_streams_and_exit_status() {
    let output = run_process(
        shell("printf out; printf err >&2; exit 7"),
        Duration::from_secs(2),
    )
    .await
    .expect("process runs");
    assert!(!output.success);
    assert!(output.content.contains("out"));
    assert!(output.content.contains("err"));
}

#[tokio::test]
async fn output_is_bounded_while_pipes_are_drained() {
    let output = run_process(
        shell("head -c 1000000 /dev/zero; printf END"),
        Duration::from_secs(5),
    )
    .await
    .expect("large process completes");
    assert!(output.success);
    assert!(output.content.len() < 34_000);
    assert!(output.content.ends_with("END"));
    assert!(output.content.contains("omitted"));
}

#[tokio::test]
async fn process_timeout_is_enforced() {
    let error = run_process(shell("sleep 30"), Duration::from_millis(30))
        .await
        .expect_err("process must time out");
    assert!(error.contains("timed out"));
}

#[tokio::test]
async fn timeout_kills_a_grandchild_before_its_delayed_write() {
    let root = tempfile::tempdir().expect("fixture root");
    let blocked = root.path().join("blocked-marker");
    let control = root.path().join("control-marker");
    let blocked_source = format!(
        "(sleep 0.2; printf marker > '{}') & sleep 1",
        blocked.display()
    );
    let error = run_process(shell(&blocked_source), Duration::from_millis(30))
        .await
        .expect_err("timed-out command must be killed");
    assert!(error.contains("timed out"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!blocked.exists(), "grandchild survived the timeout");
    let control_source = format!(
        "(sleep 0.2; printf marker > '{}') & wait",
        control.display()
    );
    assert!(
        run_process(shell(&control_source), Duration::from_secs(2))
            .await
            .expect("unsandboxed control completes")
            .success
    );
    assert_eq!(std::fs::read(control).expect("control marker"), b"marker");
}

#[tokio::test]
async fn inherited_pipe_cannot_hold_result_indefinitely() {
    let start = std::time::Instant::now();
    let output = run_process(shell("sleep 30 & printf done"), Duration::from_secs(10))
        .await
        .expect("parent exits");
    assert!(output.content.contains("done"));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn real_sandbox_preserves_workspace_writes_and_hides_secret_names() {
    use elefante_core::sandbox::SessionSandbox;
    use elefante_guard::sandbox::Network;
    let root = tempfile::tempdir().expect("fixture root");
    let root_path = root.path().canonicalize().expect("canonical fixture root");
    // A synthetic fixture only; no daemon credential or developer secret is opened.
    std::fs::write(root_path.join(".env.synthetic"), "synthetic fixture").expect("fixture");
    std::fs::create_dir(root_path.join(".GIT")).expect("metadata fixture");
    std::fs::write(root_path.join(".GIT/config"), "synthetic config").expect("metadata fixture");
    let control = run_process(
        shell(&format!(
            "cat '{}/.env.synthetic'; printf control > '{}/.GIT/config'",
            root_path.display(),
            root_path.display()
        )),
        Duration::from_secs(3),
    )
    .await
    .expect("control executes");
    assert!(control.success, "positive control: {}", control.content);
    let session = SessionSandbox::new(&root_path).expect("session setup");
    let output = session.run_shell("printf workspace > ordinary; test -f ordinary; test -z \"${OPENROUTER_API_KEY+x}\"", Network::Off).await.expect("sandbox must be available; install bubblewrap/user namespaces on Linux or enable sandbox-exec on macOS");
    assert!(output.success, "{}", output.content);
    let secret = session
        .run_shell("grep -q 'synthetic fixture' .env.synthetic", Network::Off)
        .await
        .expect("sandbox executes");
    assert!(!secret.success, "secret names must be masked");
    assert!(!secret.content.contains("synthetic fixture"));
    let metadata = session
        .run_shell("printf forbidden > .GIT/config", Network::Off)
        .await
        .expect("sandbox executes");
    assert!(!metadata.success, "Git metadata must remain read only");
}

#[tokio::test]
async fn ten_megabytes_of_sandbox_output_is_capped_with_size_and_tail() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("fixture root");
    let control = run_process(
        shell("yes X | head -c 10000000 | wc -c"),
        Duration::from_secs(5),
    )
    .await
    .expect("unsandboxed size control");
    assert!(control.success);
    assert_eq!(control.content.trim(), "10000000");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let output = session
        .run_shell("yes X | head -c 10000000; printf END", Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(output.success, "sandboxed output command failed");
    assert!(output.content.len() < 34_000);
    assert!(output.content.contains("bytes omitted"));
    assert!(output.content.ends_with("END"));
}

#[tokio::test]
async fn invalid_root_is_unavailable_and_cannot_poison_a_valid_session() {
    use elefante_core::sandbox::{Sandbox, SessionSandbox};
    use elefante_guard::sandbox::Network;
    let invalid = SessionSandbox::new(std::path::Path::new("/"))
        .expect("session can describe an unavailable sandbox");
    assert!(matches!(invalid.state(), Sandbox::Unavailable(_)));
    assert!(
        invalid
            .run_shell("true", Network::Off)
            .await
            .expect_err("invalid root refuses")
            .contains("opts out")
    );
    let root = tempfile::tempdir().expect("valid root");
    let valid = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("valid session");
    assert!(
        valid
            .run_shell("true", Network::Off)
            .await
            .expect("invalid session must not poison startup probe")
            .success
    );
}

#[tokio::test]
async fn disabled_session_keeps_bounded_runner_without_requiring_valid_git_metadata() {
    use elefante_core::sandbox::SessionSandbox;
    use elefante_guard::sandbox::Network;
    let root = tempfile::tempdir().expect("fixture root");
    std::fs::write(root.path().join(".git"), "not a gitfile").expect("invalid metadata fixture");
    let mut session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    assert!(
        session
            .run_shell("printf disabled", Network::Off)
            .await
            .is_err()
    );
    session.disable();
    let output = session
        .run_shell("printf disabled", Network::Off)
        .await
        .expect("explicit opt-out works");
    assert!(output.success);
    assert_eq!(output.content, "disabled");
}

#[test]
fn worktree_gitdir_and_commondir_are_read_only_but_external_nested_targets_are_refused() {
    use elefante_core::sandbox::SessionSandbox;
    let main = tempfile::tempdir().expect("main fixture");
    let git = main
        .path()
        .canonicalize()
        .expect("main canonical")
        .join(".git");
    for path in ["objects", "refs", "worktrees/session"] {
        std::fs::create_dir_all(git.join(path)).expect("metadata directory");
    }
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD fixture");
    std::fs::write(git.join("worktrees/session/commondir"), "../..\n")
        .expect("common directory fixture");
    let worktree = tempfile::tempdir().expect("worktree fixture");
    let root = worktree.path().canonicalize().expect("worktree canonical");
    std::fs::write(
        root.join(".git"),
        format!("gitdir: {}\n", git.join("worktrees/session").display()),
    )
    .expect("gitdir fixture");
    let session = SessionSandbox::new(&root).expect("worktree session");
    let policy = session.scan().expect("validated worktree metadata");
    assert!(policy.read_only.contains(&git));
    assert!(policy.read_only.contains(&git.join("worktrees/session")));
    let outside = tempfile::tempdir().expect("unrelated metadata");
    std::fs::create_dir(root.join("nested")).expect("nested fixture");
    std::fs::write(
        root.join("nested/.git"),
        format!("gitdir: {}\n", outside.path().display()),
    )
    .expect("external reference fixture");
    assert!(
        session
            .scan()
            .expect_err("unrelated target refuses")
            .contains("outside")
    );
}

#[tokio::test]
async fn sessions_share_root_lock_while_shared_shell_readers_remain_parallel() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("fixture root");
    let path = root.path().canonicalize().expect("canonical root");
    let first = SessionSandbox::new(&path).expect("first session");
    let second = SessionSandbox::new(&path).expect("second session");
    assert!(std::sync::Arc::ptr_eq(&first.lock, &second.lock));
    let _read = first.lock.read().await;
    let _parallel = tokio::time::timeout(Duration::from_millis(100), second.lock.read())
        .await
        .expect("shared shell read lock stays parallel");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), second.lock.write())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn host_loopback_requires_network_approval_with_positive_control() {
    use elefante_core::sandbox::SessionSandbox;
    use elefante_guard::sandbox::Network;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("host listener");
    let port = listener.local_addr().expect("listener address").port();
    let source = format!("printf control > /dev/tcp/127.0.0.1/{port}");
    assert!(
        run_process(shell(&source), Duration::from_secs(3))
            .await
            .expect("unsandboxed control")
            .success
    );
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    assert!(
        !session
            .run_shell(&source, Network::Off)
            .await
            .expect("isolated listener attempt")
            .success
    );
    assert!(
        session
            .run_shell(&source, Network::Approved)
            .await
            .expect("approved listener attempt")
            .success
    );
}

#[tokio::test]
async fn ipv6_host_loopback_requires_network_approval() {
    use elefante_core::sandbox::SessionSandbox;
    let listener = tokio::net::TcpListener::bind("[::1]:0")
        .await
        .expect("IPv6 loopback is required for this sandbox gate");
    let port = listener.local_addr().expect("listener address").port();
    let source = format!("printf control > /dev/tcp/::1/{port}");
    assert!(
        run_process(shell(&source), Duration::from_secs(3))
            .await
            .expect("unsandboxed IPv6 control")
            .success
    );
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let isolated = session
        .run_shell(&source, Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(
        !isolated.success,
        "IPv6 host listener reached without approval: {isolated:?}"
    );
    let approved = session
        .run_shell(&source, Network::Approved)
        .await
        .expect("approved IPv6 attempt");
    assert!(
        approved.success,
        "approved IPv6 listener was unreachable: {approved:?}"
    );
}

async fn udp_host_listener_control(bind: &str, host: &str) {
    use elefante_core::sandbox::SessionSandbox;
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .expect("host UDP listener is required for this sandbox gate");
    let port = socket.local_addr().expect("socket address").port();
    let source = format!("printf control > /dev/udp/{host}/{port}");
    let mut buffer = [0_u8; 32];
    assert!(
        run_process(shell(&source), Duration::from_secs(3))
            .await
            .expect("unsandboxed UDP control")
            .success
    );
    let (read, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .expect("unsandboxed datagram reaches host")
        .expect("receive control datagram");
    assert_eq!(&buffer[..read], b"control");

    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let _isolated = session
        .run_shell(&source, Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut buffer))
            .await
            .is_err(),
        "host UDP listener received a datagram with network off"
    );
    let approved = session
        .run_shell(&source, Network::Approved)
        .await
        .expect("approved UDP attempt");
    assert!(
        approved.success,
        "approved UDP command failed: {approved:?}"
    );
    let (read, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .expect("approved datagram reaches host")
        .expect("receive approved datagram");
    assert_eq!(&buffer[..read], b"control");
}

#[tokio::test]
async fn udp_ipv4_and_ipv6_cannot_reach_host_without_approval() {
    udp_host_listener_control("127.0.0.1:0", "127.0.0.1").await;
    udp_host_listener_control("[::1]:0", "::1").await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn quarantines_all_new_git_metadata_even_when_one_is_malformed() {
    use elefante_core::sandbox::SessionSandbox;
    use elefante_guard::sandbox::Network;
    let source = "mkdir -p first/.git second/.git third; printf malformed > third/.git; printf ordinary > ordinary";
    let control = tempfile::tempdir().expect("unsandboxed control root");
    let output = Command::new("/bin/bash")
        .args(["-c", source])
        .current_dir(control.path())
        .output()
        .await
        .expect("control launches");
    assert!(output.status.success());
    for name in ["first/.git", "second/.git", "third/.git"] {
        assert!(
            control.path().join(name).exists(),
            "positive control: {name}"
        );
    }

    let root = tempfile::tempdir().expect("isolated root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let error = session.run_shell(source, Network::Off).await
        .expect_err("new Git metadata must be quarantined; install bubblewrap and enable unprivileged user namespaces on Linux");
    assert!(
        error.contains("3 new Git metadata entries quarantined"),
        "{error}"
    );
    for name in ["first/.git", "second/.git", "third/.git"] {
        assert!(!root.path().join(name).exists(), "{name} remained in root");
    }
    assert_eq!(
        std::fs::read(root.path().join("ordinary")).expect("ordinary output"),
        b"ordinary"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn fresh_cargo_fixture_builds_inside_sandbox() {
    use elefante_core::sandbox::SessionSandbox;
    fn fixture(root: &std::path::Path) {
        std::fs::create_dir(root.join("src")).expect("fixture source directory");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"sandbox_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("fixture manifest");
        std::fs::write(
            root.join("src/lib.rs"),
            "#[cfg(test)] mod tests { #[test] fn works() { assert_eq!(2 + 2, 4); } }\n",
        )
        .expect("fixture source");
    }
    let control = tempfile::tempdir().expect("control crate root");
    fixture(control.path());
    let output = Command::new("cargo")
        .args(["test", "--offline", "--quiet"])
        .current_dir(control.path())
        .output()
        .await
        .expect("install the pinned Rust toolchain before running the Cargo sandbox gate");
    assert!(output.status.success(), "unsandboxed Cargo fixture failed");

    let root = tempfile::tempdir().expect("isolated crate root");
    fixture(root.path());
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let output = session
        .run_shell("cargo test --offline --quiet", Network::Off)
        .await
        .expect(
            "sandboxed Cargo failed; install bubblewrap and enable unprivileged user namespaces",
        );
    if !output.success {
        let diagnostic = session
            .run_shell(
                "R=$(rustup which rustc); printf 'rustc=%s\\n' \"$R\"; readlink -f \"$R\"; ls -ld \"$R\"; test -x \"$R\"; printf 'executable=%s\\n' \"$?\"; \"$R\" -vV",
                Network::Off,
            )
            .await
            .expect("toolchain diagnostic runs in the same sandbox");
        panic!("sandbox Cargo fixture failed: {output:?}; toolchain diagnostic: {diagnostic:?}");
    }
    assert!(output.content.contains("1 passed"), "{output:?}");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn nested_user_namespace_is_refused_with_host_control() {
    use elefante_core::sandbox::SessionSandbox;
    assert!(
        std::path::Path::new("/usr/bin/unshare").is_file(),
        "install util-linux unshare for the namespace sandbox gate"
    );
    let control = run_process(shell("/usr/bin/unshare -U true"), Duration::from_secs(3))
        .await
        .expect("unsandboxed user-namespace control");
    assert!(
        control.success,
        "enable unprivileged user namespaces before running the Linux sandbox gate"
    );
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let nested = session
        .run_shell("/usr/bin/unshare -U true", Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable unprivileged user namespaces");
    assert!(
        !nested.success,
        "nested user namespace was created: {nested:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn approved_network_cannot_reach_host_abstract_socket() {
    use elefante_core::sandbox::SessionSandbox;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    assert!(
        std::path::Path::new("/usr/bin/python3").is_file(),
        "install python3 for the abstract-socket sandbox gate"
    );
    let name = format!("elefante-test-{}", std::process::id());
    let address = SocketAddr::from_abstract_name(name.as_bytes()).expect("abstract address");
    let _listener = UnixListener::bind_addr(&address).expect("abstract listener");
    let source = format!(
        "/usr/bin/python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); s.connect(\"\\0{name}\")'"
    );
    let control = run_process(shell(&source), Duration::from_secs(3))
        .await
        .expect("unsandboxed abstract socket control");
    assert!(control.success, "host abstract socket control failed");
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let blocked = session
        .run_shell(&source, Network::Approved)
        .await
        .expect("approved sandbox executes; install bubblewrap and enable user namespaces");
    assert!(
        !blocked.success,
        "host abstract socket reached: {blocked:?}"
    );
}

#[tokio::test]
async fn former_harness_state_bypasses_reach_the_os_boundary() {
    use elefante_core::sandbox::SessionSandbox;

    let root = tempfile::tempdir().expect("fixture root");
    let root = root.path().canonicalize().expect("canonical root");
    for directory in [".elefante", "ordinary"] {
        std::fs::create_dir(root.join(directory)).expect("fixture directory");
        std::fs::write(root.join(directory).join("x"), "synthetic marker\n").expect("fixture file");
    }
    let classifier = BashClassifier::new();
    let session = SessionSandbox::new(&root).expect("session setup");
    for (blocked, allowed) in [
        ("D=.elefante; cat $D/x", "D=ordinary; cat $D/x"),
        ("cd .elefante && cat x", "cd ordinary && cat x"),
        (
            "printf '.%s/x\\n' elefante | xargs cat",
            "printf '%s\\n' ordinary/x | xargs cat",
        ),
    ] {
        assert_eq!(
            classifier.classify_in(blocked, &root).verdict,
            Verdict::Allow,
            "the OS, not the classifier, must enforce {blocked}"
        );
        let unsandboxed = run_process(
            shell(&format!("cd '{}' && {blocked}", root.display())),
            Duration::from_secs(3),
        )
        .await
        .expect("unsandboxed control executes");
        assert!(
            unsandboxed.success && unsandboxed.content.contains("synthetic marker"),
            "positive control for {blocked}: {unsandboxed:?}"
        );
        let sandboxed = session.run_shell(blocked, Network::Off).await.expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
        assert!(
            !sandboxed.success || !sandboxed.content.contains("synthetic marker"),
            "protected file leaked through {blocked}: {sandboxed:?}"
        );
        let allowed = session
            .run_shell(allowed, Network::Off)
            .await
            .expect("ordinary read executes");
        assert!(
            allowed.success && allowed.content.contains("synthetic marker"),
            "ordinary control for {blocked}: {allowed:?}"
        );
    }
}

#[tokio::test]
async fn assembled_sqlite_path_cannot_read_harness_state() {
    use elefante_core::sandbox::SessionSandbox;
    assert!(
        std::path::Path::new("/usr/bin/sqlite3").is_file(),
        "install sqlite3 to run the sandbox database gate"
    );
    let root = tempfile::tempdir().expect("fixture root");
    let root = root.path().canonicalize().expect("canonical root");
    for directory in [".elefante", "ordinary"] {
        std::fs::create_dir(root.join(directory)).expect("database fixture directory");
        let database = root.join(directory).join("memory.db");
        let output = std::process::Command::new("/usr/bin/sqlite3")
            .arg(database)
            .arg("CREATE TABLE synthetic_fixture (value TEXT);")
            .output()
            .expect("sqlite3 fixture command");
        assert!(output.status.success(), "sqlite3 fixture setup failed");
    }
    let blocked = "D=.elefante; sqlite3 \"$D/memory.db\" .schema";
    let allowed = "D=ordinary; sqlite3 \"$D/memory.db\" .schema";
    let classifier = BashClassifier::new();
    assert_eq!(
        classifier.classify_in(blocked, &root).verdict,
        Verdict::Allow,
        "assembled path must reach the OS boundary"
    );
    let host = run_process(
        shell(&format!("cd '{}' && {blocked}", root.display())),
        Duration::from_secs(3),
    )
    .await
    .expect("unsandboxed database control");
    assert!(host.success && host.content.contains("synthetic_fixture"));
    let session = SessionSandbox::new(&root).expect("session setup");
    let isolated = session
        .run_shell(blocked, Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(
        !isolated.success || !isolated.content.contains("synthetic_fixture"),
        "protected database schema leaked"
    );
    let ordinary = session
        .run_shell(allowed, Network::Off)
        .await
        .expect("ordinary database control executes");
    assert!(ordinary.success && ordinary.content.contains("synthetic_fixture"));
}

#[tokio::test]
async fn outside_root_fixture_is_inaccessible_but_workspace_remains_readable_and_writable() {
    use elefante_core::sandbox::SessionSandbox;

    let root = tempfile::tempdir().expect("fixture root");
    let root = root.path().canonicalize().expect("canonical root");
    let outside = tempfile::tempdir().expect("external fixture root");
    let outside = outside
        .path()
        .canonicalize()
        .expect("canonical external root");
    std::fs::create_dir(outside.join(".ssh")).expect("synthetic SSH-style directory");
    let secret = outside.join(".ssh/synthetic_key");
    std::fs::write(&secret, "synthetic-only-key\n").expect("synthetic fixture");
    std::fs::write(root.join("ordinary"), "synthetic-only-key\n").expect("ordinary fixture");
    let read = format!("cat '{}'", secret.display());
    let outside_write = format!("printf marker > '{}'", outside.join("created").display());
    for source in [&read, &outside_write] {
        let control = run_process(shell(source), Duration::from_secs(3))
            .await
            .expect("unsandboxed control executes");
        assert!(
            control.success,
            "positive control for {source}: {control:?}"
        );
    }
    let session = SessionSandbox::new(&root).expect("session setup");
    let hidden = session
        .run_shell(&read, Network::Off)
        .await
        .expect("sandbox executes");
    assert!(
        !hidden.success || !hidden.content.contains("synthetic-only-key"),
        "outside read leaked: {hidden:?}"
    );
    let blocked_write = session
        .run_shell(&outside_write, Network::Off)
        .await
        .expect("sandbox executes");
    assert!(
        !blocked_write.success,
        "outside write succeeded: {blocked_write:?}"
    );
    let ordinary = session
        .run_shell("cat ordinary; printf created > in-root", Network::Off)
        .await
        .expect("workspace control executes");
    assert!(
        ordinary.success && ordinary.content.contains("synthetic-only-key"),
        "workspace control: {ordinary:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("in-root")).expect("created workspace file"),
        "created"
    );
}

#[tokio::test]
async fn git_metadata_writes_fail_but_git_reads_and_host_controls_work() {
    use elefante_core::sandbox::SessionSandbox;

    let root = tempfile::tempdir().expect("fixture root");
    let root = root.path().canonicalize().expect("canonical root");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&root)
            .output()
            .expect("Git fixture command");
        assert!(
            output.status.success(),
            "Git fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("tracked"), "tracked fixture\n").expect("tracked fixture");
    git(&["add", "tracked"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    if !root.join(".GIT").exists() {
        std::fs::create_dir(root.join(".GIT")).expect("case fixture");
        std::fs::write(root.join(".GIT/config"), "# case fixture\n").expect("case config fixture");
    }
    std::fs::hard_link(root.join(".git/config"), root.join("linked-config"))
        .expect("hard link fixture");
    let session = SessionSandbox::new(&root).expect("session setup");
    for source in [
        "printf marker > .git/hooks/pre-commit",
        "printf '\n# marker\n' >> .git/config",
        "printf '\n# marker\n' >> .GIT/config",
        "printf '\n# marker\n' >> linked-config",
    ] {
        let blocked = session
            .run_shell(source, Network::Off)
            .await
            .expect("sandbox executes");
        assert!(
            !blocked.success,
            "Git metadata write succeeded: {source}: {blocked:?}"
        );
        let control = run_process(
            shell(&format!("cd '{}' && {source}", root.display())),
            Duration::from_secs(3),
        )
        .await
        .expect("host control executes");
        assert!(
            control.success,
            "positive control for {source}: {control:?}"
        );
    }
    let read = session
        .run_shell("git status --short; git log -p -1", Network::Off)
        .await
        .expect("sandbox Git read executes");
    assert!(
        read.success && read.content.contains("tracked fixture"),
        "Git reads should work: {read:?}"
    );
}

#[tokio::test]
async fn unix_socket_outside_session_is_blocked_while_private_socket_works() {
    use elefante_core::sandbox::SessionSandbox;
    use tokio::{io::AsyncWriteExt, net::UnixListener};

    let root = tempfile::tempdir().expect("fixture root");
    let outside = tempfile::tempdir().expect("outside socket fixture");
    let outside_socket = outside.path().join("listener.sock");
    let outside_listener = UnixListener::bind(&outside_socket).expect("outside listener");
    let outside_command = format!(
        "/usr/bin/curl --silent --max-time 2 --unix-socket '{}' http://localhost/",
        outside_socket.display()
    );
    let control = tokio::spawn(run_process(shell(&outside_command), Duration::from_secs(4)));
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), outside_listener.accept())
        .await
        .expect("unsandboxed connection reaches outside socket")
        .expect("accept outside control");
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfixture")
        .await
        .expect("respond to outside control");
    assert!(
        control
            .await
            .expect("control task")
            .expect("control process")
            .success
    );

    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let blocked = session
        .run_shell(&outside_command, Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(!blocked.success, "outside socket reached: {blocked:?}");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), outside_listener.accept())
            .await
            .is_err(),
        "outside listener received a sandboxed connection"
    );
    let approved = session
        .run_shell(&outside_command, Network::Approved)
        .await
        .expect("network-approved sandbox executes");
    assert!(
        !approved.success,
        "network approval reached an outside Unix socket: {approved:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), outside_listener.accept())
            .await
            .is_err(),
        "outside listener received a network-approved connection"
    );

    let private_socket = session
        .scan()
        .expect("session policy")
        .temp
        .join("fixture.sock");
    let private_listener = UnixListener::bind(&private_socket).expect("private listener");
    let private_command = format!(
        "/usr/bin/curl --silent --max-time 2 --unix-socket '{}' http://localhost/",
        private_socket.display()
    );
    let private =
        tokio::spawn(async move { session.run_shell(&private_command, Network::Off).await });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), private_listener.accept())
        .await
        .expect("private socket is reachable")
        .expect("accept private connection");
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfixture")
        .await
        .expect("respond to private control");
    let result = private
        .await
        .expect("private task")
        .expect("sandbox command");
    assert!(
        result.success && result.content.contains("fixture"),
        "{result:?}"
    );
}

#[tokio::test]
async fn synthetic_key_does_not_enter_sandboxed_process() {
    use elefante_core::sandbox::SessionSandbox;
    if std::env::var_os("ELEFANTE_SYNTHETIC_KEY_PROBE_CHILD").is_none() {
        let executable = std::env::current_exe().expect("test executable");
        let output = Command::new(executable)
            .args(["--exact", "synthetic_key_does_not_enter_sandboxed_process"])
            .env("ELEFANTE_SYNTHETIC_KEY_PROBE_CHILD", "1")
            .env("OPENROUTER_API_KEY", "synthetic-fixture-only")
            .output()
            .await
            .expect("isolated probe subprocess");
        let child_report = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stage = [
            "synthetic key was not present in the unsandboxed control",
            "synthetic key was inherited by the shell",
            "unsandboxed parent process should be inspectable",
            "sandbox exposed its host parent process",
            "synthetic key exposed in sandbox /proc",
            "fd probe cannot detect an extra descriptor",
            "sandbox inherited an extra descriptor",
        ]
        .into_iter()
        .find(|marker| child_report.contains(marker))
        .unwrap_or("unclassified child failure");
        assert!(
            output.status.success(),
            "synthetic-key child failed at {stage}; its environment remains unprinted"
        );
        return;
    }
    assert!(
        run_process(
            shell("test -n \"${OPENROUTER_API_KEY+x}\""),
            Duration::from_secs(3)
        )
        .await
        .expect("unsandboxed key-presence control")
        .success,
        "synthetic key was not present in the unsandboxed control"
    );
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let isolated = session
        .run_shell(
            "test -z \"${OPENROUTER_API_KEY+x}\" && ! env | grep -q 'OPENROUTER_API_KEY='",
            Network::Off,
        )
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(isolated.success, "synthetic key was inherited by the shell");
    assert!(!isolated.content.contains("synthetic-fixture-only"));
    assert!(
        run_process(shell("ps eww $PPID >/dev/null"), Duration::from_secs(3))
            .await
            .expect("unsandboxed parent-process control")
            .success,
        "unsandboxed parent process should be inspectable"
    );
    let parent = session
        .run_shell("ps eww $PPID >/dev/null", Network::Off)
        .await
        .expect("sandboxed parent-process attempt");
    assert!(!parent.success, "sandbox exposed its host parent process");
    #[cfg(target_os = "linux")]
    {
        let proc = session
            .run_shell(
                "for entry in /proc/[0-9]*/environ; do if grep -a -q OPENROUTER_API_KEY \"$entry\" 2>/dev/null; then exit 1; fi; done",
                Network::Off,
            )
            .await
            .expect("sandboxed process inspection");
        assert!(proc.success, "synthetic key exposed in sandbox /proc");
        let fd_probe = "for fd in /proc/$$/fd/*; do number=${fd##*/}; if test \"$number\" -gt 2 && test -L \"$fd\"; then exit 1; fi; done";
        let control = run_process(
            shell(&format!("exec 3< /dev/null; {fd_probe}")),
            Duration::from_secs(3),
        )
        .await
        .expect("unsandboxed fd control");
        assert!(
            !control.success,
            "fd probe missed an intentionally open descriptor"
        );
        let fds = session
            .run_shell(fd_probe, Network::Off)
            .await
            .expect("sandboxed fd inspection");
        assert!(fds.success, "sandbox inherited an extra descriptor");
    }
}
