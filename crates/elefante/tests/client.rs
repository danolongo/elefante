//! Exercise the real headless binary against a deterministic protocol peer.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::Command,
    thread,
    time::Duration,
};

#[test]
fn daemon_startup_failure_reports_captured_stderr() {
    let directory = tempfile::tempdir().unwrap();
    // The client starts its own executable as the daemon; an invalid setting makes that daemon
    // refuse to start, and its reason must reach the person rather than a timeout.
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .env("ELEFANTE_SOCKET", directory.path().join("socket"))
        .env("ELEFANTE_IDLE_SECS", "not-a-number")
        .env_remove("ELEFANTE_EFFORT")
        .arg("hello")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(output.stdout.len(), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid ELEFANTE_IDLE_SECS"), "{stderr}");
    assert!(stderr.contains("exited during startup"), "{stderr}");
    assert!(!stderr.contains("within 2 seconds"), "{stderr}");
}

#[test]
fn elefante_daemon_serves_the_socket_in_the_foreground() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("run/socket");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .arg("daemon")
        .env("HOME", directory.path().join("home"))
        .env("ELEFANTE_SOCKET", &socket)
        .env("ELEFANTE_IDLE_SECS", "1")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "daemon exited before listening"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "daemon never listened"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(daemon.wait().unwrap().success(), "exits cleanly when idle");
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
fn memory_learn_goes_to_the_listening_daemon_without_touching_the_local_file() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let home = directory.path().join("home");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut peer = BufReader::new(stream);
        let request = receive(&mut peer, "memory.consent");
        assert_eq!(
            request["params"],
            json!({"answer":"learn","extractor":"vendor/extractor"})
        );
        answer(&mut peer, &request, &json!({"consent":"granted"}));
    });
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .env("HOME", &home)
        .env("ELEFANTE_SOCKET", &socket)
        .env("ELEFANTE_EXTRACT_MODEL", "vendor/extractor")
        .args(["memory", "learn", "on"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"memory learning is granted for vendor/extractor\n"
    );
    assert_eq!(output.stderr.len(), 0);
    assert!(!home.join(".elefante/memory-consent").exists());
    assert!(!home.join(".elefante/memory.key").exists());
}

#[test]
fn memory_list_goes_to_the_listening_daemon_and_renders_safe_rows() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut peer = BufReader::new(stream);
        let request = receive(&mut peer, "memory.list");
        assert_eq!(request["params"]["cwd"], cwd.to_string_lossy().as_ref());
        assert_eq!(request["params"]["all"], false);
        answer(
            &mut peer,
            &request,
            &json!({"notes":[{
                "id":14,"kind":"convention","status":"active","statement":"Use pnpm.",
                "origin":"learned:explicit","confidence":0.9,"verified":true,"reinforcements":1
            }]}),
        );
    });
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(directory.path())
        .env("ELEFANTE_SOCKET", &socket)
        .env_remove("HOME")
        .args(["memory", "list"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Use pnpm."), "{stdout}");
    assert!(stdout.contains("explicit"), "{stdout}");
    assert_eq!(output.stderr.len(), 0);
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
    assert_eq!(output.stderr.len(), 0);
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
    assert_eq!(output.stderr.len(), 0);
}

fn refuse(peer: &mut BufReader<UnixStream>, request: &Value, code: i64) {
    send(
        peer,
        &json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":code,"message":"unknown method"}}),
    );
}

#[test]
fn a_daemon_that_predates_a_memory_command_is_named_not_worked_around() {
    for (args, method) in [
        (&["memory", "list"][..], "memory.list"),
        (&["memory", "learn", "on"], "memory.consent"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("socket");
        let home = directory.path().join("home");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer = BufReader::new(stream);
            let request = receive(&mut peer, method);
            refuse(&mut peer, &request, -32601);
        });
        let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
            .current_dir(directory.path())
            .env("HOME", &home)
            .env("ELEFANTE_SOCKET", &socket)
            .env("ELEFANTE_EXTRACT_MODEL", "vendor/extractor")
            .args(args)
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(output.stdout.len(), 0);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("older build"), "{stderr}");
        assert!(
            !home.join(".elefante").exists(),
            "nothing is written around the daemon"
        );
    }
}

#[test]
fn another_daemon_error_is_still_an_error() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut peer = BufReader::new(stream);
        let request = receive(&mut peer, "memory.list");
        refuse(&mut peer, &request, -32007);
    });
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(directory.path())
        .env("HOME", directory.path().join("home"))
        .env("ELEFANTE_SOCKET", &socket)
        .args(["memory", "list"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success(), "{output:?}");
}
