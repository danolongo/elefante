use elefante_core::sandbox::{SessionSandbox, run_process};
use elefante_guard::sandbox::Network;
use std::{net::ToSocketAddrs, process::Stdio, time::Duration};
use tokio::process::Command;

#[test]
fn resolver_lookup_child() {
    if std::env::var_os("ELEFANTE_DNS_PROBE_CHILD").is_none() {
        return;
    }
    let addresses = ("example.com", 80)
        .to_socket_addrs()
        .expect("approved system DNS must resolve example.com")
        .collect::<Vec<_>>();
    assert!(!addresses.is_empty(), "resolver returned no addresses");
}

#[tokio::test]
async fn system_dns_requires_approval_and_then_resolves() {
    let root = tempfile::tempdir().expect("fixture root");
    let probe = root.path().join("resolver_probe");
    std::fs::copy(std::env::current_exe().expect("test executable"), &probe)
        .expect("copy resolver probe into workspace");
    let source = "ELEFANTE_DNS_PROBE_CHILD=1 ./resolver_probe --exact resolver_lookup_child";
    let mut host = Command::new("/bin/bash");
    host.args(["-c", source])
        .current_dir(root.path())
        .stdin(Stdio::null());
    let host = run_process(host, Duration::from_secs(15))
        .await
        .expect("host DNS control executes");
    assert!(
        host.success,
        "host DNS must work before the sandbox gate: {}",
        host.content
    );

    let session = SessionSandbox::new(&root.path().canonicalize().expect("canonical root"))
        .expect("session setup");
    let blocked = session.run_shell(source, Network::Off).await
        .expect("sandbox runs; install bubblewrap and enable user namespaces on Linux, or enable sandbox-exec on macOS");
    assert!(
        !blocked.success,
        "DNS resolved without approval: {}",
        blocked.content
    );
    let approved = session
        .run_shell(source, Network::Approved)
        .await
        .expect("approved sandbox runs");
    assert!(
        approved.success,
        "approved DNS failed: {}",
        approved.content
    );
}
