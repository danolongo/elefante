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
