//! End to end through the real binary: a scripted model asks for an SSH key under the user's
//! home, and neither the file tools nor the sandboxed shell hand it over.
//!
//! The run uses a fake `HOME`, so the fixture key is never a real credential, and the debug stub
//! provider, so nothing reaches the network.

use serde_json::{Value, json};
use std::{collections::HashMap, fs, os::unix::fs::symlink, process::Command};

const SECRET: &str = "FIXTURE-SSH-PRIVATE-KEY";
const ORDINARY: &str = "FIXTURE-ORDINARY-CONTENT";

fn call(id: &str, name: &str, input: &Value) -> [Value; 2] {
    [
        json!({"ToolUseStart":{"id":id,"name":name}}),
        json!({"ToolUseComplete":{"id":id,"input":input}}),
    ]
}

/// Every tool result the model was sent, by tool call id.
fn tool_results(requests: &str) -> HashMap<String, (String, bool)> {
    let mut results = HashMap::new();
    for line in requests.lines() {
        let request: Value = serde_json::from_str(line).expect("recorded request is JSON");
        for message in request["messages"].as_array().into_iter().flatten() {
            for block in message["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_result" {
                    results.insert(
                        block["tool_use_id"].as_str().unwrap().to_owned(),
                        (
                            block["content"].as_str().unwrap().to_owned(),
                            block["is_error"].as_bool().unwrap(),
                        ),
                    );
                }
            }
        }
    }
    results
}

#[test]
fn a_scripted_tool_call_cannot_read_an_ssh_key_under_home() {
    let fixture = tempfile::tempdir().expect("fixture directory");
    let base = fixture.path().canonicalize().expect("canonical fixture");
    let home = base.join("home");
    let project = base.join("project");
    fs::create_dir_all(home.join(".ssh")).expect("fixture home");
    fs::create_dir_all(&project).expect("fixture project");
    let key = home.join(".ssh/id_ed25519");
    fs::write(&key, SECRET).expect("fixture key");
    assert_eq!(
        fs::read_to_string(&key).expect("setup: the host reads the fixture key"),
        SECRET
    );
    fs::write(project.join("ordinary.txt"), ORDINARY).expect("fixture file");
    symlink(&key, project.join("notes.txt")).expect("fixture link");
    // A glob the classifier cannot resolve, so only the OS sandbox stands between it and the key.
    let shell_read = format!("cat {}/.ss?/id_*", home.display());
    let host = Command::new("/bin/bash")
        .args(["-c", &shell_read])
        .output()
        .expect("host bash runs");
    assert_eq!(
        String::from_utf8_lossy(&host.stdout),
        SECRET,
        "setup: outside the sandbox the same command reads the key"
    );

    let mut first = Vec::new();
    first.extend(call("read-key", "read", &json!({"path":key})));
    first.extend(call("read-link", "read", &json!({"path":"notes.txt"})));
    first.extend(call("bash-key", "bash", &json!({"command":shell_read})));
    first.extend(call(
        "read-ordinary",
        "read",
        &json!({"path":"ordinary.txt"}),
    ));
    first.extend(call(
        "bash-ordinary",
        "bash",
        &json!({"command":"cat ordinary.txt"}),
    ));
    first.push(json!({"Stop":"tool_use"}));
    let requests = base.join("requests.jsonl");
    let script = base.join("script.json");
    fs::write(
        &script,
        serde_json::to_vec(&json!({
            "record_requests": requests,
            "rounds": [first, [{"TextDelta":"done"}, {"Stop":"end_turn"}]]
        }))
        .unwrap(),
    )
    .expect("stub script");

    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(&project)
        .arg("read the key")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENROUTER_BASE_URL")
        .env_remove("ELEFANTE_SANDBOX")
        .env("HOME", &home)
        .env("ELEFANTE_SOCKET", base.join("run/elefante.sock"))
        .env("ELEFANTE_IDLE_SECS", "1")
        .env("ELEFANTE_MEMORY", "0")
        .env("ELEFANTE_STUB_PROVIDER", &script)
        .env("NO_COLOR", "1")
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        !stderr.contains("sandbox:"),
        "the run must be sandboxed: {stderr}"
    );

    let recorded = fs::read_to_string(&requests).expect("recorded requests");
    let results = tool_results(&recorded);
    for id in ["read-ordinary", "bash-ordinary"] {
        let (content, is_error) = &results[id];
        assert!(
            !is_error && content.contains(ORDINARY),
            "positive control {id}: {content}"
        );
    }
    for id in ["read-key", "read-link", "bash-key"] {
        let (content, is_error) = &results[id];
        assert!(*is_error, "{id} must be refused: {content}");
    }
    let (content, _) = &results["bash-key"];
    assert!(
        !content.contains("denied by policy"),
        "the gate let the shell read run, and the sandbox refused it: {content}"
    );
    for (place, text) in [
        ("requests", recorded.as_str()),
        ("stdout", &stdout),
        ("stderr", &stderr),
    ] {
        assert!(!text.contains(SECRET), "the key reached {place}");
    }
}
