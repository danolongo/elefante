use elefante_core::{GateDecision, ToolGate, sandbox::Sandbox};
use serde_json::json;

#[test]
fn bash_gate_routes_writes_and_network_requests_before_execution() {
    let root = tempfile::tempdir().unwrap();
    let gate = ToolGate::default();
    let active = Sandbox::Seatbelt;
    assert_eq!(
        gate.check_in("bash", &json!({"command":"ls"}), root.path(), &active),
        GateDecision::Allow,
    );
    assert_eq!(
        gate.check_in(
            "bash",
            &json!({"command":"git add -A"}),
            root.path(),
            &active
        ),
        GateDecision::Escalate {
            rule: "git-write",
            command: "git add -A".into(),
            network: false,
        },
    );
    assert_eq!(
        gate.check_in("bash", &json!({"command":"git push"}), root.path(), &active),
        GateDecision::Deny {
            rule: "git-write-refused"
        },
    );
    for command in ["ls", "curl example.com"] {
        assert_eq!(
            gate.check_in(
                "bash",
                &json!({"command":command,"network":true}),
                root.path(),
                &active
            ),
            GateDecision::Escalate {
                rule: "network-requested",
                command: command.into(),
                network: true,
            },
        );
    }
    assert!(matches!(
        gate.check_in(
            "bash",
            &json!({"command":"rm -rf /","network":true}),
            root.path(),
            &active
        ),
        GateDecision::Deny { .. },
    ));
}

#[test]
fn disabled_requires_approval_and_unavailable_reaches_fail_closed_runner() {
    let root = tempfile::tempdir().unwrap();
    let gate = ToolGate::default();
    for command in ["ls", "git add -A", "rm -rf /"] {
        assert_eq!(
            gate.check_in(
                "bash",
                &json!({"command":command}),
                root.path(),
                &Sandbox::Disabled
            ),
            GateDecision::Escalate {
                rule: "sandbox-off",
                command: command.into(),
                network: false,
            },
        );
    }
    assert_eq!(
        gate.check_in(
            "bash",
            &json!({"command":"ls"}),
            root.path(),
            &Sandbox::Unavailable("fixture".into())
        ),
        GateDecision::Allow,
    );
}

#[test]
fn harness_git_writes_need_an_identity_before_approval() {
    let root = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root.path())
        .arg("init")
        .output()
        .expect("fixture Git launches");
    assert!(status.status.success(), "fixture Git init failed");
    let gate = ToolGate::default();
    let input = json!({"command":"git add -A"});
    let anonymous = elefante_core::tools::ToolExecutor::new(root.path()).unwrap();
    assert!(!anonymous.has_git_identity());
    assert_eq!(
        gate.check_with_tools("bash", &input, &anonymous),
        GateDecision::Deny {
            rule: "git-identity-missing"
        },
    );
    // Shell commands other than harness writes are unaffected by the missing identity.
    assert_eq!(
        gate.check_with_tools("bash", &json!({"command":"ls"}), &anonymous),
        GateDecision::Allow,
    );
    let identified = elefante_core::tools::ToolExecutor::new(root.path())
        .unwrap()
        .with_git_identity(elefante_guard::git::Identity {
            name: "Fixture".into(),
            email: "fixture@example.invalid".into(),
        })
        .unwrap();
    assert!(identified.has_git_identity());
    assert_eq!(
        gate.check_with_tools("bash", &input, &identified),
        GateDecision::Escalate {
            rule: "git-write",
            command: "git add -A".into(),
            network: false,
        },
    );
}
