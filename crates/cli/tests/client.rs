//! Exercise the real headless binary against a deterministic protocol peer.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::Command,
    thread,
    time::Duration,
};

#[test]
fn sibling_daemon_startup_failure_reports_captured_stderr() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("elefante");
    std::fs::copy(env!("CARGO_BIN_EXE_elefante"), &cli).unwrap();
    let daemon = directory.path().join("elefante-srv");
    std::fs::write(
        &daemon,
        "#!/bin/sh\necho 'daemon startup detail' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(cli)
        .env("PATH", "")
        .env("ELEFANTE_SOCKET", directory.path().join("socket"))
        .env_remove("ELEFANTE_EFFORT")
        .arg("hello")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon startup detail"), "{stderr}");
    assert!(!stderr.contains("within 2 seconds"), "{stderr}");
}

fn receive(peer: &mut BufReader<UnixStream>, method: &str) -> Value {
    let mut line = String::new();
    peer.read_line(&mut line).unwrap();
    let request: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], method);
    request
}
fn send(peer: &mut BufReader<UnixStream>, value: &Value) {
    serde_json::to_writer(peer.get_mut(), value).unwrap();
    peer.get_mut().write_all(b"\n").unwrap();
}
fn answer(peer: &mut BufReader<UnixStream>, request: &Value, result: &Value) {
    send(
        peer,
        &json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
    );
}

#[test]
fn memory_learn_prefers_a_listening_daemon_without_touching_the_local_file() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let home = directory.path().join("home");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut peer = BufReader::new(stream);
        let request = receive(&mut peer, "memory.consent");
        assert_eq!(request["params"], json!({"answer":"learn"}));
        answer(&mut peer, &request, &json!({"consent":"granted"}));
    });
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .env("HOME", &home)
        .env("ELEFANTE_SOCKET", &socket)
        .args(["memory", "learn", "on"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"memory learning is granted\n");
    assert!(output.stderr.is_empty());
    assert!(!home.join(".elefante/memory-consent").exists());
    assert!(!home.join(".elefante/memory.key").exists());
}

#[test]
fn budgets_reach_daemon_and_coalesced_duplicates_render_once() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut peer = BufReader::new(stream);
        let create = receive(&mut peer, "session.create");
        assert_eq!(create["params"]["max_rounds"], 8);
        assert_eq!(create["params"]["max_tokens"], 2048);
        assert_eq!(create["params"]["effort"], "low");
        answer(&mut peer, &create, &json!({"session_id":"s1","seq":0}));
        let attach = receive(&mut peer, "session.attach");
        answer(
            &mut peer,
            &attach,
            &json!({"kind":"resumed","from":1,"events":[]}),
        );
        let prompt = receive(&mut peer, "session.prompt");
        answer(&mut peer, &prompt, &json!({"turn_id":"t1"}));
        let note = json!({"session_id":"s1","seq_from":1,"seq_to":3,"turn_id":"t1","method":"text.delta","params":{"text":"answer\n"}});
        send(&mut peer, &note);
        send(&mut peer, &note);
        send(
            &mut peer,
            &json!({"session_id":"s1","seq":4,"turn_id":"t1","method":"turn.finished","params":{"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0,"reasoning_tokens":0,"cached_input_tokens":0,"cache_write_tokens":0,"cost":null}}}),
        );
        let close = receive(&mut peer, "session.close");
        answer(&mut peer, &close, &json!({}));
    });
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .env("ELEFANTE_SOCKET", socket)
        .env_remove("ELEFANTE_EFFORT")
        .args([
            "--max-rounds",
            "8",
            "--max-tokens",
            "2048",
            "--effort",
            "low",
            "hello",
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"answer\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn disconnected_headless_client_resumes_from_its_last_delivered_event() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut peer = BufReader::new(stream);
        let create = receive(&mut peer, "session.create");
        answer(&mut peer, &create, &json!({"session_id":"s1","seq":0}));
        let attach = receive(&mut peer, "session.attach");
        answer(
            &mut peer,
            &attach,
            &json!({"kind":"resumed","from":1,"events":[]}),
        );
        let prompt = receive(&mut peer, "session.prompt");
        answer(&mut peer, &prompt, &json!({"turn_id":"t1"}));
        send(
            &mut peer,
            &json!({"session_id":"s1","seq":1,"turn_id":"t1","method":"text.delta","params":{"text":"first "}}),
        );
        drop(peer);
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut peer = BufReader::new(stream);
        let attach = receive(&mut peer, "session.attach");
        assert_eq!(attach["params"]["since"], 1);
        answer(
            &mut peer,
            &attach,
            &json!({"kind":"resumed","from":2,"events":[
                {"session_id":"s1","seq":2,"turn_id":"t1","method":"text.delta","params":{"text":"second\n"}},
                {"session_id":"s1","seq":3,"turn_id":"t1","method":"turn.finished","params":{"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0,"reasoning_tokens":0,"cached_input_tokens":0,"cache_write_tokens":0,"cost":null}}}
            ]}),
        );
        let close = receive(&mut peer, "session.close");
        answer(&mut peer, &close, &json!({}));
    });
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .env("ELEFANTE_SOCKET", socket)
        .env_remove("ELEFANTE_EFFORT")
        .arg("hello")
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"first second\n");
    assert!(output.stderr.is_empty());
}
