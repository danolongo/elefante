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
    assert!(output.content.len() <= 32 * 1024);
    assert!(output.content.ends_with("END"));
    assert!(output.content.contains("(original 1000003 bytes)"));
}

#[tokio::test]
async fn overflowing_streams_share_one_budget_and_state_their_sizes() {
    let output = run_process(
        shell("head -c 100000 /dev/zero | tr '\\0' o; head -c 200000 /dev/zero | tr '\\0' e >&2; exit 3"),
        Duration::from_secs(5),
    )
    .await
    .expect("both streams overflow");
    assert!(!output.success);
    assert!(
        output.content.len() <= 32 * 1024,
        "{}",
        output.content.len()
    );
    assert!(output.content.contains("(original 100000 bytes)"));
    assert!(output.content.contains("(original 200000 bytes)"));
    let small = run_process(
        shell("printf small; head -c 200000 /dev/zero | tr '\\0' e >&2"),
        Duration::from_secs(5),
    )
    .await
    .expect("stderr overflows");
    assert!(small.content.starts_with("small"));
    assert!(small.content.len() <= 32 * 1024);
    assert!(
        small.content.matches('e').count() > 30_000,
        "stderr did not receive the unused stdout budget"
    );
}

#[tokio::test]
async fn process_timeout_is_enforced() {
    let error = run_process(shell("sleep 30"), Duration::from_millis(30))
        .await
        .expect_err("process must time out");
    assert!(
        error.starts_with("command timed out after 0.03 s"),
        "{error}"
    );
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
    assert!(output.content.len() <= 32 * 1024);
    assert!(output.content.contains("(original 10000003 bytes)"));
    assert!(output.content.ends_with("END"));
}

#[tokio::test]
async fn invalid_root_is_unavailable_and_cannot_poison_a_valid_session() {
    use elefante_core::sandbox::{Sandbox, SessionSandbox};
    use elefante_guard::sandbox::Network;
    let invalid = SessionSandbox::new(std::path::Path::new("/"))
        .expect("session can describe an unavailable sandbox");
    assert!(matches!(invalid.state(), Sandbox::Unavailable(_)));
    let error = invalid
        .run_shell("true", Network::Off)
        .await
        .expect_err("invalid root refuses");
    assert!(error.starts_with("sandbox unavailable: "), "{error}");
    assert!(
        !error.contains("ELEFANTE_SANDBOX=0"),
        "opting out cannot fix an invalid root: {error}"
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

#[tokio::test]
async fn disabled_session_forwards_host_home_and_puts_toolchains_first() {
    use elefante_core::sandbox::{Sandbox, SessionSandbox};
    use std::os::unix::fs::PermissionsExt;
    let source =
        "elefante-fixture-tool; printf '|%s|%s|%s' \"$HOME\" \"$CARGO_HOME\" \"$RUSTUP_HOME\"";
    if let Some(base) = std::env::var_os("ELEFANTE_DISABLED_FIXTURE_BASE") {
        let base = std::path::PathBuf::from(base);
        let workspace = base.join("workspace");
        let session = SessionSandbox::new(&workspace).expect("disabled session");
        assert!(matches!(session.state(), Sandbox::Disabled));
        let output = session
            .run_shell(source, Network::Off)
            .await
            .expect("disabled shell runs");
        assert!(output.success, "{output:?}");
        let home = base.join("home");
        assert_eq!(
            output.content,
            format!(
                "tool|{}|{}|{}",
                home.display(),
                home.join(".cargo").display(),
                home.join("rustup").display()
            )
        );
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let base = fixture.path().canonicalize().expect("canonical fixture");
    let tools = base.join("home/.cargo/bin");
    std::fs::create_dir_all(&tools).expect("tool directory");
    std::fs::create_dir_all(base.join("home/rustup")).expect("rustup directory");
    std::fs::create_dir(base.join("workspace")).expect("workspace");
    let tool = tools.join("elefante-fixture-tool");
    std::fs::write(&tool, "#!/bin/sh\nprintf tool\n").expect("tool fixture");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("tool mode");
    let child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "disabled_session_forwards_host_home_and_puts_toolchains_first",
        ])
        .env("ELEFANTE_DISABLED_FIXTURE_BASE", &base)
        .env("ELEFANTE_SANDBOX", "0")
        .env("HOME", base.join("home"))
        .env("CARGO_HOME", base.join("home/.cargo"))
        .env("RUSTUP_HOME", base.join("home/rustup"))
        .output()
        .await
        .expect("disabled fixture child");
    assert!(
        child.status.success(),
        "disabled fixture child failed: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
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
    let source = "mkdir -p first/.git second/.git third target/.git; printf malformed > third/.git; printf ordinary > ordinary";
    let names = ["first/.git", "second/.git", "third/.git", "target/.git"];
    // Quarantine moves entries under HOME, so a child with a fake HOME runs the session.
    let Some(base) = std::env::var_os("ELEFANTE_QUARANTINE_FIXTURE_BASE") else {
        let control = tempfile::tempdir().expect("unsandboxed control root");
        let output = Command::new("/bin/bash")
            .args(["-c", source])
            .current_dir(control.path())
            .output()
            .await
            .expect("control launches");
        assert!(output.status.success());
        for name in names {
            assert!(
                control.path().join(name).exists(),
                "positive control: {name}"
            );
        }
        let fixture = tempfile::tempdir().expect("quarantine fixture");
        let base = fixture.path().canonicalize().expect("canonical fixture");
        for directory in ["home", "workspace"] {
            std::fs::create_dir(base.join(directory)).expect("fixture directory");
        }
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "quarantines_all_new_git_metadata_even_when_one_is_malformed",
            ])
            .env("ELEFANTE_QUARANTINE_FIXTURE_BASE", &base)
            .env("HOME", base.join("home"))
            .output()
            .await
            .expect("quarantine fixture child");
        assert!(
            child.status.success(),
            "quarantine child failed: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    };
    let base = std::path::PathBuf::from(base);
    let root = base.join("workspace");
    let session = SessionSandbox::new(&root).expect("session setup");
    let error = session.run_shell(source, Network::Off).await
        .expect_err("new Git metadata must be quarantined; install bubblewrap and enable unprivileged user namespaces on Linux");
    assert!(
        error.contains("4 new Git metadata entries quarantined"),
        "{error}"
    );
    for name in names {
        assert!(!root.join(name).exists(), "{name} remained in root");
    }
    let quarantined: usize = std::fs::read_dir(base.join("home/.elefante/quarantine"))
        .expect("quarantine under the fake HOME")
        .map(|session| {
            std::fs::read_dir(session.expect("quarantine entry").path())
                .expect("session quarantine")
                .count()
        })
        .sum();
    assert_eq!(quarantined, 4);
    assert_eq!(
        std::fs::read(root.join("ordinary")).expect("ordinary output"),
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
        ("cat .elef*/x", "cat ordin*/x"),
        ("grep -r marker .elef*", "grep -r marker ordin*"),
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
    assert!(
        std::path::Path::new("/usr/bin/curl").is_file(),
        "install curl at /usr/bin/curl; the socket gate needs it as its client"
    );

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

async fn assert_protected_socket_inaccessible(
    session: &elefante_core::sandbox::SessionSandbox,
    path: &std::path::Path,
) {
    use tokio::{io::AsyncWriteExt, net::UnixListener};
    assert!(
        std::path::Path::new("/usr/bin/curl").is_file(),
        "install curl at /usr/bin/curl; the socket gate needs it as its client"
    );
    let listener = UnixListener::bind(path).expect("synthetic service socket");
    let command = format!(
        "/usr/bin/curl --silent --max-time 2 --unix-socket '{}' http://localhost/",
        path.display()
    );
    let control = tokio::spawn(run_process(shell(&command), Duration::from_secs(4)));
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("unsandboxed service connection reaches fixture")
        .expect("accept host control");
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nfixture")
        .await
        .expect("respond to host control");
    assert!(
        control
            .await
            .expect("control task")
            .expect("control process")
            .success
    );
    for network in [Network::Off, Network::Approved] {
        let result = session
            .run_shell(&command, network)
            .await
            .expect("sandbox runs");
        assert!(
            !result.success,
            "protected socket reached under {network:?}: {result:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "protected listener accepted a sandboxed connection under {network:?}"
        );
    }
}

#[tokio::test]
async fn configured_daemon_socket_paths_reject_connections() {
    use elefante_core::sandbox::SessionSandbox;
    let child_mode = std::env::var_os("ELEFANTE_SOCKET_FIXTURE_CHILD").is_some();
    let fixture = (!child_mode).then(|| {
        #[cfg(target_os = "macos")]
        let fixture = tempfile::tempdir_in("/private/tmp");
        #[cfg(not(target_os = "macos"))]
        let fixture = tempfile::tempdir();
        fixture.expect("synthetic service fixture")
    });
    let base = fixture.as_ref().map_or_else(
        || {
            std::path::PathBuf::from(
                std::env::var_os("ELEFANTE_SOCKET_FIXTURE_BASE").expect("fixture base"),
            )
        },
        |fixture| {
            fixture
                .path()
                .canonicalize()
                .expect("canonical fixture base")
        },
    );
    let home = base.join("home");
    let runtime = base.join("runtime");
    let override_dir = base.join("override");
    for directory in [&home, &runtime, &override_dir] {
        std::fs::create_dir_all(directory).expect("synthetic directory");
    }
    let override_socket = override_dir.join("custom.sock");
    if !child_mode {
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "configured_daemon_socket_paths_reject_connections",
            ])
            .env("ELEFANTE_SOCKET_FIXTURE_CHILD", "1")
            .env("ELEFANTE_SOCKET_FIXTURE_BASE", &base)
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("ELEFANTE_SOCKET", &override_socket)
            .env_remove("OPENROUTER_API_KEY")
            .output()
            .await
            .expect("isolated service fixture child");
        assert!(
            child.status.success(),
            "synthetic service child failed: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }
    let harness_run = home.join(".elefante/run");
    std::fs::create_dir_all(&harness_run).expect("synthetic harness run directory");
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).expect("workspace fixture");
    let session = SessionSandbox::new(&workspace.canonicalize().expect("canonical workspace"))
        .expect("sandbox session");
    let paths = [
        harness_run.join("elefante.sock"),
        runtime.join("elefante.sock"),
        override_socket,
    ];
    let policy = session.scan().expect("session policy");
    for path in &paths {
        assert!(
            policy.protected.contains(path),
            "synthetic socket was not protected"
        );
        assert_protected_socket_inaccessible(&session, path).await;
        // The daemon writes its token beside its socket.
        let token = path.with_file_name("token");
        assert!(
            policy.protected.contains(&token),
            "synthetic token was not protected: {}",
            token.display()
        );
        assert_protected_file_unreadable(&session, &token).await;
    }
}

async fn assert_protected_file_unreadable(
    session: &elefante_core::sandbox::SessionSandbox,
    path: &std::path::Path,
) {
    std::fs::write(path, "synthetic token\n").expect("synthetic token file");
    let command = format!("cat '{}'", path.display());
    let control = run_process(shell(&command), Duration::from_secs(3))
        .await
        .expect("unsandboxed control executes");
    assert!(
        control.success && control.content.contains("synthetic token"),
        "positive control for {}: {control:?}",
        path.display()
    );
    let sandboxed = session
        .run_shell(&command, Network::Off)
        .await
        .expect("sandbox runs");
    assert!(
        !sandboxed.success || !sandboxed.content.contains("synthetic token"),
        "protected token leaked: {sandboxed:?}"
    );
}

fn synthetic_child_failure_stage(output: &std::process::Output) -> String {
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
        "sandbox /proc environ check inspected nothing",
        "host /proc environ control did not find the synthetic key",
        "synthetic key exposed in sandbox /proc",
        "host process inherited an extra descriptor",
        "fd probe did not identify the intentionally retained FD 3",
        "sandbox inherited an extra descriptor",
    ]
    .into_iter()
    .find(|marker| child_report.contains(marker))
    .unwrap_or("unclassified child failure");
    match child_report
        .split("open_fd=")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .filter(|value| !value.is_empty() && value.len() <= 3)
    {
        Some(number) => format!("{stage} (fd {number})"),
        None => stage.to_owned(),
    }
}

/// Builds a probe that prints the first descriptor above stderr. Linux lists `/proc/self/fd`.
/// Seatbelt denies reads of `/dev/fd`, so macOS tests each descriptor with `fcntl`.
async fn build_fd_probe(root: &std::path::Path) -> std::path::PathBuf {
    assert!(
        std::path::Path::new("/usr/bin/cc").is_file(),
        "install a C compiler for the inherited-fd gate"
    );
    let source = root.join("fd_probe.c");
    let binary = root.join("fd_probe");
    std::fs::write(
        &source,
        r#"
#include <stdio.h>
#ifdef __linux__
#include <dirent.h>
#include <stdlib.h>
#include <unistd.h>
int main(void) {
    DIR *entries = opendir("/proc/self/fd");
    if (!entries) return 2;
    int own = dirfd(entries);
    struct dirent *entry;
    while ((entry = readdir(entries))) {
        char *end;
        long fd = strtol(entry->d_name, &end, 10);
        if (*end == '\0' && fd > 2 && fd != own) {
            char link[64], target[512] = "";
            snprintf(link, sizeof link, "/proc/self/fd/%ld", fd);
            ssize_t length = readlink(link, target, sizeof target - 1);
            if (length > 0) target[length] = '\0';
            printf("open_fd=%ld target=%s\n", fd, target);
            closedir(entries);
            return 1;
        }
    }
    closedir(entries);
    return 0;
}
#else
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>
int main(void) {
    int limit = getdtablesize();
    if (limit > 65536) limit = 65536;
    for (int fd = 3; fd < limit; fd++) {
        if (fcntl(fd, F_GETFD) != -1) {
            char target[1024] = "";
            struct stat info;
            fcntl(fd, F_GETPATH, target);
            fstat(fd, &info);
            printf("open_fd=%d mode=%o target=%s\n", fd, info.st_mode & S_IFMT, target);
            return 1;
        }
    }
    return 0;
}
#endif
"#,
    )
    .expect("write fd fixture source");
    let compiled = Command::new("/usr/bin/cc")
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .output()
        .await
        .expect("fd fixture compiler runs");
    assert!(
        compiled.status.success(),
        "fd fixture compilation failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    binary
}

/// Runs the fd probe on the host with a clean control and a retained-FD control, then in the
/// sandbox. Each failure message carries the probe's `open_fd=` line.
async fn assert_only_standard_descriptors(
    session: &elefante_core::sandbox::SessionSandbox,
    root: &std::path::Path,
) {
    build_fd_probe(root).await;
    let mut clean_command = shell("./fd_probe");
    clean_command.current_dir(root);
    let clean_control = run_process(clean_command, Duration::from_secs(3))
        .await
        .expect("clean host fd control");
    assert!(
        clean_control.success,
        "host process inherited an extra descriptor: {}",
        clean_control.content
    );
    let mut retained_command = shell("exec 3< /dev/null; exec ./fd_probe");
    retained_command.current_dir(root);
    let control = run_process(retained_command, Duration::from_secs(3))
        .await
        .expect("unsandboxed fd control");
    assert!(
        !control.success && control.content.contains("open_fd=3"),
        "fd probe did not identify the intentionally retained FD 3: {}",
        control.content
    );
    let fds = session
        .run_shell("./fd_probe", Network::Off)
        .await
        .expect("sandboxed fd inspection");
    assert!(
        fds.success,
        "sandbox inherited an extra descriptor: {}",
        fds.content
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
        let stage = synthetic_child_failure_stage(&output);
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
        let scan = "for entry in /proc/[0-9]*/environ; do if grep -a -q OPENROUTER_API_KEY \"$entry\" 2>/dev/null; then exit 1; fi; done";
        let host = run_process(shell(scan), Duration::from_secs(5))
            .await
            .expect("host /proc environ control");
        assert!(
            !host.success,
            "host /proc environ control did not find the synthetic key"
        );
        let readable = session
            .run_shell("grep -a -q TERM=dumb /proc/$$/environ", Network::Off)
            .await
            .expect("sandboxed environ readability check");
        assert!(
            readable.success,
            "sandbox /proc environ check inspected nothing: {readable:?}"
        );
        let proc = session
            .run_shell(scan, Network::Off)
            .await
            .expect("sandboxed process inspection");
        assert!(proc.success, "synthetic key exposed in sandbox /proc");
    }
    assert_only_standard_descriptors(&session, root.path()).await;
}

#[tokio::test]
async fn planted_temp_symlinks_cannot_redirect_daemon_writes() {
    use elefante_core::sandbox::SessionSandbox;
    let outside = tempfile::tempdir().expect("outside fixture directory");
    let victim = outside.path().join("victim");
    std::fs::write(&victim, "outside fixture").expect("outside fixture");
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let temp = session.scan().expect("session policy").temp;
    // The names match the program files earlier daemons wrote into the sandbox's TMPDIR.
    let plant = format!(
        "for i in $(seq 0 3000); do ln -s '{0}' \"$TMPDIR/seccomp-0-$i\"; ln -s '{0}' \"$TMPDIR/seccomp-1-$i\"; done",
        victim.display()
    );
    let planted = session
        .run_shell(&plant, Network::Off)
        .await
        .expect("sandboxed plant runs");
    assert!(
        planted.success,
        "sandbox could not plant symlinks: {planted:?}"
    );
    assert_eq!(
        std::fs::read_link(temp.join("seccomp-0-0")).expect("planted link persists"),
        victim,
        "planted link does not point at the outside fixture"
    );
    let next = session
        .run_shell("true", Network::Off)
        .await
        .expect("second sandboxed command runs");
    assert!(next.success, "second sandboxed command failed: {next:?}");
    assert_eq!(
        std::fs::read_to_string(&victim).expect("outside fixture remains"),
        "outside fixture",
        "daemon wrote through a planted symlink"
    );
}

#[tokio::test]
async fn descriptors_the_daemon_inherited_never_reach_a_command() {
    use elefante_core::sandbox::SessionSandbox;
    if std::env::var_os("ELEFANTE_STRAY_FD_CHILD").is_none() {
        let executable = std::env::current_exe().expect("test executable");
        // The child starts with fd 7 open and inheritable, as a daemon might from its launcher.
        let output = Command::new("/bin/bash")
            .args(["-c", r#"exec 7</dev/null; exec "$0" "$@""#])
            .arg(executable)
            .args([
                "--exact",
                "descriptors_the_daemon_inherited_never_reach_a_command",
            ])
            .env("ELEFANTE_STRAY_FD_CHILD", "1")
            .output()
            .await
            .expect("stray-descriptor child");
        assert!(
            output.status.success(),
            "stray-descriptor child failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let root = tempfile::tempdir().expect("fixture root");
    build_fd_probe(root.path()).await;
    let mut host_command = shell("./fd_probe");
    host_command.current_dir(root.path());
    let host = run_process(host_command, Duration::from_secs(3))
        .await
        .expect("host fd probe runs");
    assert!(
        !host.success && host.content.contains("open_fd=7"),
        "the stray descriptor was not inherited by a host child: {}",
        host.content
    );
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let sandboxed = session
        .run_shell("./fd_probe", Network::Off)
        .await
        .expect("sandboxed fd probe runs");
    assert!(
        sandboxed.success,
        "a command received a descriptor the daemon inherited: {}",
        sandboxed.content
    );
}

#[tokio::test]
async fn concurrent_sandboxed_command_leaks_no_descriptor() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("fixture root");
    build_fd_probe(root.path()).await;
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let sleeper = session.run_shell("printf ready > ready; sleep 3", Network::Off);
    let probes = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("sleeping sandboxed command never started");
        let mut host_command = shell("./fd_probe");
        host_command.current_dir(root.path());
        let host = run_process(host_command, Duration::from_secs(3))
            .await
            .expect("host fd probe runs");
        let sandboxed = session
            .run_shell("./fd_probe", Network::Off)
            .await
            .expect("sandboxed fd probe runs");
        (host, sandboxed)
    };
    let (slept, (host, sandboxed)) = tokio::join!(sleeper, probes);
    assert!(
        slept.expect("sleeping sandboxed command runs").success,
        "sleeping sandboxed command failed"
    );
    assert!(
        host.success,
        "host process inherited a descriptor from a concurrent sandboxed command: {}",
        host.content
    );
    assert!(
        sandboxed.success,
        "sandbox inherited a descriptor from a concurrent sandboxed command: {}",
        sandboxed.content
    );
}

#[tokio::test]
async fn synthetic_pem_is_hidden_while_ordinary_text_is_readable() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("fixture root");
    let root = root.path().canonicalize().expect("canonical root");
    for name in ["synthetic.pem", "ordinary.txt"] {
        std::fs::write(root.join(name), "synthetic-pem-marker\n").expect("synthetic file");
    }
    let mut host = shell("cat synthetic.pem");
    host.current_dir(&root);
    let control = run_process(host, Duration::from_secs(3))
        .await
        .expect("host read control");
    assert!(control.success && control.content.contains("synthetic-pem-marker"));
    let session = SessionSandbox::new(&root).expect("session setup");
    let secret = session
        .run_shell("cat synthetic.pem", Network::Off)
        .await
        .expect("sandbox executes; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(!secret.success || !secret.content.contains("synthetic-pem-marker"));
    let ordinary = session
        .run_shell("cat ordinary.txt", Network::Off)
        .await
        .expect("ordinary read executes");
    assert!(ordinary.success && ordinary.content.contains("synthetic-pem-marker"));
}

#[tokio::test]
async fn synthetic_cargo_credentials_are_hidden_from_shell_reads() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("fixture root");
    let root = root.path().canonicalize().expect("canonical root");
    std::fs::create_dir(root.join(".cargo")).expect("synthetic Cargo directory");
    std::fs::write(
        root.join(".cargo/credentials.toml"),
        "synthetic-cargo-marker\n",
    )
    .expect("synthetic credential fixture");
    std::fs::write(root.join("ordinary.txt"), "synthetic-cargo-marker\n")
        .expect("ordinary fixture");
    let source = "CARGO_HOME=$PWD/.cargo; cat \"$CARGO_HOME/credentials.toml\"";
    let mut host = shell(source);
    host.current_dir(&root);
    let control = run_process(host, Duration::from_secs(3))
        .await
        .expect("unsandboxed Cargo fixture control");
    assert!(control.success && control.content.contains("synthetic-cargo-marker"));
    let session = SessionSandbox::new(&root).expect("sandbox session");
    let blocked = session
        .run_shell(source, Network::Off)
        .await
        .expect("sandbox runs");
    assert!(
        !blocked.content.contains("synthetic-cargo-marker"),
        "synthetic credential leaked"
    );
    let ordinary = session
        .run_shell("cat ordinary.txt", Network::Off)
        .await
        .expect("ordinary read");
    assert!(ordinary.success && ordinary.content.contains("synthetic-cargo-marker"));
}

#[tokio::test]
async fn home_root_is_unavailable_and_never_runs_a_marker_command() {
    use elefante_core::sandbox::{Sandbox, SessionSandbox};
    let home = std::fs::canonicalize(std::env::var_os("HOME").expect("HOME for root validation"))
        .expect("canonical HOME");
    let fixture = tempfile::tempdir().expect("scratch marker root");
    let marker = fixture.path().join("marker");
    let source = format!("printf marker > '{}'", marker.display());
    let control = run_process(shell(&source), Duration::from_secs(3))
        .await
        .expect("host marker control");
    assert!(control.success && marker.exists());
    std::fs::remove_file(&marker).expect("clear the exact scratch marker");
    let session = SessionSandbox::new(&home).expect("unavailable session is representable");
    assert!(matches!(session.state(), Sandbox::Unavailable(_)));
    let error = session
        .run_shell(&source, Network::Off)
        .await
        .expect_err("HOME-rooted session must not execute");
    assert!(error.contains("sandbox unavailable"), "{error}");
    assert!(
        !marker.exists(),
        "HOME-rooted sandbox ran the marker command"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn private_loopback_works_without_reaching_the_host() {
    use elefante_core::sandbox::SessionSandbox;
    assert!(
        std::path::Path::new("/usr/bin/python3").is_file(),
        "install python3 for the Linux private-loopback gate"
    );
    let source = r#"python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(1); c=socket.socket(); c.connect(s.getsockname()); a,_=s.accept(); c.sendall(b"ok"); assert a.recv(2)==b"ok"'"#;
    let control = run_process(shell(source), Duration::from_secs(3))
        .await
        .expect("host loopback control");
    assert!(control.success, "host loopback control: {control:?}");
    let root = tempfile::tempdir().expect("fixture root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let private = session
        .run_shell(source, Network::Off)
        .await
        .expect("sandbox runs; install bubblewrap and enable user namespaces on Linux");
    assert!(private.success, "private loopback failed: {private:?}");
    // The paired host-listener test proves this same Off policy cannot reach host loopback.
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn pid_namespace_kills_setsid_orphans_after_the_command() {
    use elefante_core::sandbox::SessionSandbox;
    assert!(
        std::path::Path::new("/usr/bin/setsid").is_file(),
        "install util-linux setsid for the Linux lifetime gate"
    );
    let control = tempfile::tempdir().expect("host control root");
    let source = "setsid /bin/bash -c 'printf ready > ready; sleep 0.2; printf marker > orphan' </dev/null >/dev/null 2>&1 & for i in $(seq 1 100); do test -f ready && break; sleep 0.01; done; test -f ready";
    let mut host = shell(source);
    host.current_dir(control.path());
    let host = run_process(host, Duration::from_secs(2))
        .await
        .expect("host setsid control executes");
    assert!(host.success, "host setsid control: {host:?}");
    tokio::time::timeout(Duration::from_secs(2), async {
        while !control.path().join("orphan").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("host orphan wrote its marker after detaching");
    assert_eq!(
        std::fs::read(control.path().join("orphan")).expect("host orphan ran"),
        b"marker"
    );

    let root = tempfile::tempdir().expect("sandbox root");
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let result = session
        .run_shell(source, Network::Off)
        .await
        .expect("sandbox runs; install bubblewrap and enable user namespaces on Linux");
    assert!(result.success, "sandbox command failed: {result:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !root.path().join("orphan").exists(),
        "setsid orphan survived the PID namespace"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn seccomp_refuses_mount_with_a_disposable_namespace_control() {
    use elefante_core::sandbox::SessionSandbox;
    for program in ["/usr/bin/unshare", "/usr/bin/mount", "/usr/bin/umount"] {
        assert!(
            std::path::Path::new(program).is_file(),
            "install util-linux for the Linux mount gate: {program}"
        );
    }
    let root = tempfile::tempdir().expect("scratch mount root");
    std::fs::create_dir(root.path().join("mnt")).expect("mount target");
    let control = format!(
        "unshare -Ur -m /bin/sh -c 'mount -t tmpfs tmpfs \"$1\"; umount \"$1\"' sh '{}'",
        root.path().join("mnt").display()
    );
    let host = run_process(shell(&control), Duration::from_secs(5))
        .await
        .expect("disposable mount namespace control");
    assert!(
        host.success,
        "enable unprivileged user namespaces and tmpfs mounts for this gate: {host:?}"
    );

    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let blocked = session
        .run_shell("mount -t tmpfs tmpfs mnt", Network::Off)
        .await
        .expect("sandbox runs; install bubblewrap and enable user namespaces on Linux");
    assert!(
        !blocked.success,
        "sandbox mounted tmpfs despite seccomp: {blocked:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn seccomp_returns_enosys_for_io_uring_setup_with_a_host_control() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("scratch syscall probe root");
    let source = root.path().join("io_uring_probe.c");
    let binary = root.path().join("io_uring_probe");
    std::fs::write(
        &source,
        r#"
#include <errno.h>
#include <linux/io_uring.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
int main(void) {
    struct io_uring_params params = {0};
    int fd = syscall(__NR_io_uring_setup, 1, &params);
    if (fd < 0) { printf("errno=%d\n", errno); return 2; }
    close(fd);
    puts("io_uring_setup succeeded");
    return 0;
}
"#,
    )
    .expect("write syscall probe source");
    let compiled = Command::new("/usr/bin/cc")
        .args([
            "-o",
            binary.to_str().expect("UTF-8 fixture path"),
            source.to_str().expect("UTF-8 fixture path"),
        ])
        .output()
        .await
        .expect("install a C compiler for the Linux syscall gate");
    assert!(
        compiled.status.success(),
        "syscall fixture compilation failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let host = Command::new(&binary)
        .output()
        .await
        .expect("host syscall control runs");
    assert!(
        host.status.success(),
        "host io_uring_setup unavailable: {}; the runtime gate cannot claim a positive control",
        String::from_utf8_lossy(&host.stdout)
    );
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("sandbox session");
    let blocked = session
        .run_shell("./io_uring_probe", Network::Off)
        .await
        .expect("sandbox runs");
    assert!(
        !blocked.success && blocked.content.contains("errno=38"),
        "sandbox did not return ENOSYS: {blocked:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn seccomp_returns_eperm_for_process_vm_readv_without_network() {
    use elefante_core::sandbox::SessionSandbox;
    let root = tempfile::tempdir().expect("scratch syscall probe root");
    let source = root.path().join("vm_read_probe.c");
    let binary = root.path().join("vm_read_probe");
    std::fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <sys/uio.h>
#include <unistd.h>
int main(void) {
    char source[] = "fixture", target[sizeof source];
    struct iovec local = { target, sizeof target }, remote = { source, sizeof source };
    if (process_vm_readv(getpid(), &local, 1, &remote, 1, 0) < 0) {
        printf("errno=%d\n", errno);
        return 2;
    }
    puts("process_vm_readv succeeded");
    return 0;
}
"#,
    )
    .expect("write syscall probe source");
    let compiled = Command::new("/usr/bin/cc")
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .output()
        .await
        .expect("install a C compiler for the Linux syscall gate");
    assert!(
        compiled.status.success(),
        "syscall fixture compilation failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let host = Command::new(&binary)
        .output()
        .await
        .expect("host syscall control runs");
    assert!(
        host.status.success(),
        "host process_vm_readv unavailable: {}; the EPERM gate cannot claim a positive control",
        String::from_utf8_lossy(&host.stdout)
    );
    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("sandbox session");
    let blocked = session
        .run_shell("./vm_read_probe", Network::Off)
        .await
        .expect("sandbox runs");
    assert!(
        !blocked.success && blocked.content.contains("errno=1\n"),
        "sandbox did not return EPERM for process_vm_readv: {blocked:?}"
    );
}

#[tokio::test]
async fn enclosing_repository_hooks_path_inside_the_root_is_read_only() {
    use elefante_core::sandbox::SessionSandbox;
    let fixture = tempfile::tempdir().expect("fixture repository");
    let repository = fixture.path().canonicalize().expect("canonical repository");
    for args in [
        &["init", "-q"][..],
        &["config", "core.hooksPath", "sub/.husky"],
    ] {
        let output = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&repository)
            .output()
            .expect("Git fixture command");
        assert!(output.status.success(), "Git fixture failed: {output:?}");
    }
    let root = repository.join("sub");
    std::fs::create_dir_all(root.join(".husky")).expect("hooks fixture");
    let control = run_process(
        shell(&format!(
            "cd '{}' && printf host > .husky/host",
            root.display()
        )),
        Duration::from_secs(3),
    )
    .await
    .expect("host control executes");
    assert!(control.success, "host write to .husky: {control:?}");
    let session = SessionSandbox::new(&root).expect("session setup");
    let blocked = session
        .run_shell("printf hook > .husky/pre-commit", Network::Off)
        .await
        .expect("sandbox executes");
    assert!(!blocked.success, "sandboxed hook write: {blocked:?}");
    assert!(!root.join(".husky/pre-commit").exists());
    let ordinary = session
        .run_shell("printf ordinary > ordinary", Network::Off)
        .await
        .expect("sandbox executes");
    assert!(ordinary.success, "ordinary in-root write: {ordinary:?}");
}

#[tokio::test]
async fn in_root_hooks_path_is_read_only_and_a_missing_one_refuses() {
    use elefante_core::sandbox::SessionSandbox;
    let fixture = tempfile::tempdir().expect("fixture root");
    let root = fixture.path().canonicalize().expect("canonical root");
    for args in [&["init", "-q"][..], &["config", "core.hooksPath", ".husky"]] {
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
    }
    let session = SessionSandbox::new(&root).expect("session setup");
    let hooks = root.join(".husky");
    let missing = session
        .run_shell("printf ordinary > ordinary", Network::Off)
        .await
        .expect_err("a missing in-root hooksPath target refuses");
    assert!(
        missing.to_ascii_lowercase().contains("core.hookspath")
            && missing.contains(&hooks.display().to_string()),
        "{missing}"
    );
    assert!(!root.join("ordinary").exists());

    std::fs::create_dir(&hooks).expect("hooks fixture");
    let control = run_process(
        shell(&format!(
            "cd '{}' && printf host > .husky/host",
            root.display()
        )),
        Duration::from_secs(3),
    )
    .await
    .expect("host control executes");
    assert!(control.success, "host write to .husky: {control:?}");
    let blocked = session
        .run_shell("printf hook > .husky/pre-commit", Network::Off)
        .await
        .expect("sandbox executes");
    assert!(!blocked.success, "sandboxed hook write: {blocked:?}");
    assert!(!hooks.join("pre-commit").exists());
    let ordinary = session
        .run_shell("printf ordinary > ordinary", Network::Off)
        .await
        .expect("sandbox executes");
    assert!(ordinary.success, "ordinary in-root write: {ordinary:?}");
    assert_eq!(
        std::fs::read(root.join("ordinary")).expect("ordinary output"),
        b"ordinary"
    );
}
