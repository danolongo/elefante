//! CI-only controls for macOS host services. They must not run against a developer's session.
#![cfg(target_os = "macos")]

use elefante_core::sandbox::SessionSandbox;
use elefante_guard::sandbox::Network;
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};
use tokio::{io::AsyncWriteExt as _, process::Command as AsyncCommand};

fn require_disposable_runner() {
    assert!(
        std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")
            && std::env::var("ELEFANTE_MAC_HOST_GATE").as_deref() == Ok("1"),
        "CI-only host-service gate: run on a disposable macOS Actions runner with ELEFANTE_MAC_HOST_GATE=1; never run these ignored tests on a developer Mac"
    );
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("synthetic host-service fixture");
    let root = temp.path().canonicalize().expect("canonical fixture root");
    (temp, root)
}

fn session(root: &Path) -> SessionSandbox {
    SessionSandbox::new(root).expect("sandbox session; enable sandbox-exec on macOS")
}

/// Returns the cleared environment the sandboxed attempt receives, so each host control differs
/// from its sandboxed attempt only by the Seatbelt profile. Session tool directories stay off
/// `PATH`; every control names its binary by absolute path.
fn environment(session: &SessionSandbox) -> Vec<(OsString, OsString)> {
    session
        .scan()
        .expect("session policy")
        .environment(std::iter::empty(), &[], true)
        .expect("sandbox environment")
}

/// How long a host service may take to act on a cold CI runner.
const SERVICE_WAIT: Duration = Duration::from_secs(30);

async fn host(environment: &[(OsString, OsString)], program: &str, args: &[&str]) -> Output {
    let mut command = AsyncCommand::new(program);
    command
        .args(args)
        .env_clear()
        .envs(environment.iter().cloned())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("host control timed out on the macOS CI runner")
        .expect("host control launches")
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn marker_appears(path: &Path) {
    tokio::time::timeout(SERVICE_WAIT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("host service unavailable on the macOS runner: synthetic marker never appeared");
}

struct KeychainCleanup(PathBuf);

impl Drop for KeychainCleanup {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(&self.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct ClipboardCleanup;

impl Drop for ClipboardCleanup {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct LaunchJobCleanup(String);

impl Drop for LaunchJobCleanup {
    fn drop(&mut self) {
        let _ = Command::new("/bin/launchctl")
            .arg("remove")
            .arg(&self.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test]
#[ignore = "CI-only synthetic macOS host-service gate"]
async fn sandbox_cannot_read_a_synthetic_keychain_item() {
    require_disposable_runner();
    let (_temp, root) = fixture();
    let sandbox = session(&root);
    let env = environment(&sandbox);
    let keychain = root.join("fixture.keychain-db");
    let path = keychain.to_str().expect("UTF-8 scratch path");
    let _cleanup = KeychainCleanup(keychain.clone());
    assert!(
        host(
            &env,
            "/usr/bin/security",
            &["create-keychain", "-p", "fixture-password", path]
        )
        .await
        .status
        .success(),
        "synthetic keychain creation failed on the macOS runner"
    );
    assert!(
        host(
            &env,
            "/usr/bin/security",
            &[
                "add-generic-password",
                "-a",
                "elefante-fixture",
                "-s",
                "elefante-fixture",
                "-w",
                "synthetic-item",
                "-A",
                path
            ]
        )
        .await
        .status
        .success(),
        "synthetic keychain item setup failed on the macOS runner"
    );
    let args = [
        "find-generic-password",
        "-a",
        "elefante-fixture",
        "-s",
        "elefante-fixture",
        "-w",
        path,
    ];
    let control = host(&env, "/usr/bin/security", &args).await;
    assert!(
        control.status.success(),
        "host keychain lookup control unavailable"
    );
    assert_eq!(control.stdout, b"synthetic-item\n");
    let command = format!(
        "/usr/bin/security find-generic-password -a elefante-fixture -s elefante-fixture -w {}",
        quote(path)
    );
    let blocked = sandbox
        .run_shell(&command, Network::Off)
        .await
        .expect("sandbox runs");
    assert!(
        !blocked.success,
        "sandbox reached the synthetic keychain item"
    );
    assert!(
        !blocked.content.contains("synthetic-item"),
        "synthetic item appeared in sandbox output"
    );
    assert!(
        host(&env, "/usr/bin/security", &["delete-keychain", path])
            .await
            .status
            .success(),
        "synthetic keychain cleanup failed"
    );
}

#[tokio::test]
#[ignore = "CI-only synthetic macOS host-service gate"]
async fn sandbox_cannot_read_the_synthetic_clipboard() {
    require_disposable_runner();
    let (_temp, root) = fixture();
    let sandbox = session(&root);
    let env = environment(&sandbox);
    let marker = b"elefante-ci-clipboard-fixture";
    let _cleanup = ClipboardCleanup;
    let mut copy = AsyncCommand::new("/usr/bin/pbcopy")
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("pbcopy host control launches");
    copy.stdin
        .take()
        .expect("pbcopy stdin")
        .write_all(marker)
        .await
        .expect("write synthetic clipboard");
    assert!(
        tokio::time::timeout(Duration::from_secs(15), copy.wait())
            .await
            .expect("pbcopy timed out on the macOS CI runner")
            .expect("pbcopy host control exits")
            .success(),
        "CI runner has no working pasteboard"
    );
    let control = host(&env, "/usr/bin/pbpaste", &[]).await;
    assert!(
        control.status.success(),
        "CI runner pasteboard cannot be read"
    );
    assert_eq!(
        control.stdout, marker,
        "CI runner pasteboard did not retain synthetic content"
    );
    let blocked = sandbox
        .run_shell("/usr/bin/pbpaste", Network::Off)
        .await
        .expect("sandbox runs");
    assert!(!blocked.success, "sandbox accessed the synthetic clipboard");
    assert!(
        !blocked.content.contains("elefante-ci-clipboard-fixture"),
        "synthetic clipboard content appeared in sandbox output"
    );
    assert!(
        host(&env, "/usr/bin/pbcopy", &[]).await.status.success(),
        "synthetic clipboard cleanup failed"
    );
}

#[tokio::test]
#[ignore = "CI-only synthetic macOS host-service gate"]
async fn sandbox_cannot_launch_a_scratch_app() {
    require_disposable_runner();
    let (_temp, root) = fixture();
    let sandbox = session(&root);
    let env = environment(&sandbox);
    let app = root.join("Fixture.app");
    let contents = app.join("Contents");
    let executable = contents.join("MacOS/Fixture");
    let marker = root.join("app-marker");
    fs::create_dir_all(executable.parent().expect("app executable parent"))
        .expect("scratch app directories");
    fs::write(contents.join("Info.plist"), format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Fixture</string>
<key>CFBundleIdentifier</key><string>dev.elefante.sandbox-fixture.{}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>"#, std::process::id())).expect("scratch app plist");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf marker > {}\n",
            quote(marker.to_str().expect("UTF-8 marker path"))
        ),
    )
    .expect("scratch app executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
        .expect("executable scratch app");
    let app_path = app.to_str().expect("UTF-8 app path");
    assert!(
        host(&env, "/usr/bin/open", &["-a", app_path])
            .await
            .status
            .success(),
        "LaunchServices cannot open a scratch app on the CI runner"
    );
    marker_appears(&marker).await;
    fs::remove_file(&marker).expect("clear only the scratch app marker");
    let command = format!("/usr/bin/open -a {}", quote(app_path));
    let blocked = sandbox
        .run_shell(&command, Network::Off)
        .await
        .expect("sandbox runs");
    assert!(!blocked.success, "sandbox launched the scratch app");
    tokio::time::sleep(SERVICE_WAIT).await;
    assert!(
        !marker.exists(),
        "scratch app marker appeared after sandboxed launch"
    );
}

#[tokio::test]
#[ignore = "CI-only synthetic macOS host-service gate"]
async fn sandbox_cannot_submit_a_launchd_job() {
    require_disposable_runner();
    let (_temp, root) = fixture();
    let sandbox = session(&root);
    let env = environment(&sandbox);
    let label = format!("dev.elefante.sandbox-fixture.{}", std::process::id());
    let _cleanup = LaunchJobCleanup(label.clone());
    let marker = root.join("launchd-marker");
    let payload = format!(
        "printf marker > {}",
        quote(marker.to_str().expect("UTF-8 marker path"))
    );
    let control = host(
        &env,
        "/bin/launchctl",
        &["submit", "-l", &label, "--", "/bin/sh", "-c", &payload],
    )
    .await;
    assert!(
        control.status.success(),
        "launchctl submit unavailable on the macOS CI runner"
    );
    marker_appears(&marker).await;
    let removed = host(&env, "/bin/launchctl", &["remove", &label]).await;
    assert!(
        removed.status.success(),
        "cannot remove the synthetic launchd job before the denial test"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while host(&env, "/bin/launchctl", &["list", &label])
            .await
            .status
            .success()
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("synthetic launchd job remained registered after removal");
    fs::remove_file(&marker).expect("clear only the scratch launchd marker");
    let command = format!(
        "/bin/launchctl submit -l {label} -- /bin/sh -c {}",
        quote(&payload)
    );
    let blocked = sandbox
        .run_shell(&command, Network::Off)
        .await
        .expect("sandbox runs");
    assert!(!blocked.success, "sandbox submitted a launchd job");
    assert!(
        !host(&env, "/bin/launchctl", &["list", &label])
            .await
            .status
            .success(),
        "sandbox registered a launchd job despite command failure"
    );
    tokio::time::sleep(SERVICE_WAIT).await;
    assert!(
        !host(&env, "/bin/launchctl", &["list", &label])
            .await
            .status
            .success(),
        "sandbox registered a delayed launchd job"
    );
    assert!(
        !marker.exists(),
        "launchd marker appeared after sandboxed submission"
    );
}

#[tokio::test]
#[ignore = "CI-only synthetic macOS host-service gate"]
async fn sandbox_cannot_send_a_finder_apple_event() {
    require_disposable_runner();
    let (_temp, root) = fixture();
    let sandbox = session(&root);
    let env = environment(&sandbox);
    let script = "tell application \"Finder\" to get version";
    let control = host(&env, "/usr/bin/osascript", &["-e", script]).await;
    assert!(
        control.status.success(),
        "Finder Apple Events unavailable on the macOS CI runner"
    );
    assert!(!control.stdout.is_empty(), "Finder returned no version");
    let blocked = sandbox
        .run_shell(
            "/usr/bin/osascript -e 'tell application \"Finder\" to get version'",
            Network::Off,
        )
        .await
        .expect("sandbox runs");
    assert!(!blocked.success, "sandbox sent a Finder Apple Event");
}
