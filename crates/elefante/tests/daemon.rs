use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

use elefante::daemon::{DaemonConfig, LearnMode, run};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    task::JoinHandle,
};

struct Client {
    read: BufReader<OwnedReadHalf>,
    write: OwnedWriteHalf,
}
impl Client {
    /// A connection that has not yet authenticated.
    async fn unauthenticated(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let (read, write) = stream.into_split();
        Self {
            read: BufReader::new(read),
            write,
        }
    }

    async fn connect(path: &Path) -> Self {
        let mut client = Self::unauthenticated(path).await;
        let token = fs::read_to_string(elefante::daemon::token_path(path)).unwrap();
        let reply = client.rpc(0, "auth", json!({"token":token})).await;
        assert_eq!(reply["result"], json!({}), "{reply}");
        client
    }

    async fn send(&mut self, id: u64, method: &str, params: Value) {
        let mut bytes =
            serde_json::to_vec(&json!({"id":id,"method":method,"params":params})).unwrap();
        bytes.push(b'\n');
        self.write.write_all(&bytes).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "connection closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(id, method, params).await;
        loop {
            let value = self.next().await;
            if value["id"] == id {
                return value;
            }
        }
    }

    async fn finish(&mut self) -> Vec<Value> {
        let mut notes = Vec::new();
        loop {
            let note = self.next().await;
            if note.get("id").is_some() {
                continue;
            }
            let finished = note["method"] == "turn.finished";
            notes.push(note);
            if finished {
                return notes;
            }
        }
    }
}

struct Daemon {
    dir: tempfile::TempDir,
    config: DaemonConfig,
    task: JoinHandle<anyhow::Result<()>>,
}
impl Daemon {
    async fn start(script: Option<Value>, idle: Duration) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let stub = script.map(|script| {
            let path = dir.path().join("script.json");
            fs::write(&path, serde_json::to_vec(&script).unwrap()).unwrap();
            path
        });
        let config = DaemonConfig {
            socket: dir.path().join("run/elefante.sock"),
            credentials: dir.path().join("home/.elefante/credentials.toml"),
            api_key: None,
            base_url: Some("http://127.0.0.1:9".into()),
            stub_provider: stub,
            memory: true,
            learn: LearnMode::Off,
            learn_idle: Duration::from_secs(300),
            learn_timeout: Duration::from_secs(60),
            idle_timeout: idle,
        };
        let task = tokio::spawn(run(config.clone()));
        for _ in 0..100 {
            if UnixStream::connect(&config.socket).await.is_ok() {
                return Self { dir, config, task };
            }
            assert!(!task.is_finished(), "daemon failed at startup");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("daemon did not become ready");
    }

    async fn client(&self) -> Client {
        Client::connect(&self.config.socket).await
    }
    async fn create(&self, client: &mut Client) -> String {
        let result = client
            .rpc(1, "session.create", json!({"cwd":self.dir.path()}))
            .await;
        assert_eq!(result["result"]["seq"], 0);
        assert_eq!(result["result"]["memory"]["consent"], "off");
        assert_eq!(result["result"]["memory"]["extractor"], Value::Null);
        result["result"]["session_id"].as_str().unwrap().to_owned()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn script() -> Value {
    json!({"delay_ms":5,"events":[{"ReasoningDelta":"think"},{"TextDelta":"hello "},{"TextDelta":"world"},{"Stop":"end_turn"}]})
}

#[tokio::test]
async fn two_clients_receive_identical_gapless_turns_and_reconnect_resumes() {
    let daemon = Daemon::start(Some(script()), Duration::from_secs(20)).await;
    let mut first = daemon.client().await;
    let session = daemon.create(&mut first).await;
    let mut second = daemon.client().await;
    assert_eq!(
        second
            .rpc(2, "session.attach", json!({"session_id":session}))
            .await["result"]["kind"],
        "snapshot"
    );
    first
        .rpc(
            3,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    let (one, two) = tokio::join!(first.finish(), second.finish());
    assert_eq!(one, two);
    for (index, note) in one.iter().enumerate() {
        assert_eq!(note["seq"], (index + 1) as u64);
        assert_eq!(note["session_id"], session);
        serde_json::from_value::<elefante::wire::Notification>(note.clone()).unwrap();
    }
    let mut seq = one.last().unwrap()["seq"].as_u64().unwrap();
    drop(first);
    let mut first = daemon.client().await;
    let attached = first
        .rpc(
            4,
            "session.attach",
            json!({"session_id":session,"since":seq-2}),
        )
        .await;
    assert_eq!(attached["result"]["kind"], "resumed");
    assert_eq!(attached["result"]["memory"]["consent"], "off");
    assert_eq!(attached["result"]["memory"]["extractor"], Value::Null);
    assert_eq!(attached["result"]["from"], seq - 1);
    let replayed = attached["result"]["events"].as_array().unwrap();
    assert_eq!(&replayed[..2], &one[one.len() - 2..]);
    assert_eq!(replayed[2]["method"], "memory.status");
    seq = replayed.last().unwrap()["seq"].as_u64().unwrap();
    first
        .rpc(
            5,
            "session.prompt",
            json!({"session_id":session,"text":"again"}),
        )
        .await;
    let next = first.finish().await;
    assert_eq!(next[0]["seq"], seq + 1);
    assert_ne!(next[0]["turn_id"], one[0]["turn_id"]);
    let snapshot = first
        .rpc(6, "session.attach", json!({"session_id":session}))
        .await;
    assert_eq!(snapshot["result"]["memory"]["consent"], "off");
    assert_eq!(
        snapshot["result"]["snapshot"]["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        snapshot["result"]["snapshot"]["turns"][0]["text"],
        "hello world"
    );
    serde_json::from_value::<elefante::wire::AttachResult>(snapshot["result"].clone()).unwrap();
}

#[tokio::test]
async fn reconnect_mid_turn_returns_delta_then_live_output_intact() {
    let daemon = Daemon::start(Some(json!({"events":[{"TextDelta":"one"},{"delay_ms":200},{"TextDelta":"two"},{"Stop":"end_turn"}]})), Duration::from_secs(20)).await;
    let mut first = daemon.client().await;
    let session = daemon.create(&mut first).await;
    first
        .rpc(
            2,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    let started = first.next().await;
    assert_eq!(started["method"], "turn.started");
    drop(first);
    tokio::time::sleep(Duration::from_millis(25)).await;
    let mut second = daemon.client().await;
    second
        .send(
            3,
            "session.attach",
            json!({"session_id":session,"since":started["seq"]}),
        )
        .await;
    let attached = second.next().await;
    assert_eq!(attached["id"], 3, "attach result must precede live notes");
    assert_eq!(attached["result"]["kind"], "resumed");
    assert_eq!(attached["result"]["from"], 2);
    let mut events = attached["result"]["events"].as_array().unwrap().clone();
    events.extend(second.finish().await);
    let text: String = events
        .iter()
        .filter(|event| event["method"] == "text.delta")
        .map(|event| event["params"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "onetwo");
    for pair in events.windows(2) {
        assert_eq!(
            pair[0]["seq"].as_u64().unwrap() + 1,
            pair[1]["seq"].as_u64().unwrap()
        );
    }
}

#[tokio::test]
async fn stale_ring_snapshots_and_future_cursor_errors() {
    let mut events: Vec<_> = (0..4200).map(|_| json!({"TextDelta":"x"})).collect();
    events.push(json!({"Stop":"end_turn"}));
    let daemon = Daemon::start(Some(json!({"events":events})), Duration::from_secs(20)).await;
    let mut client = daemon.client().await;
    let session = daemon.create(&mut client).await;
    client
        .rpc(
            2,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    client.finish().await;
    let attached = client
        .rpc(3, "session.attach", json!({"session_id":session,"since":1}))
        .await;
    assert_eq!(attached["result"]["kind"], "snapshot");
    assert_eq!(
        attached["result"]["snapshot"]["turns"][0]["text"],
        "x".repeat(4200)
    );
    let future = client
        .rpc(
            4,
            "session.attach",
            json!({"session_id":session,"since":99999}),
        )
        .await;
    assert_eq!(future["error"]["code"], -32003);
}

#[tokio::test]
async fn interrupt_rejects_concurrent_prompt_and_close_is_idempotent() {
    let daemon = Daemon::start(
        Some(json!({"events":[{"TextDelta":"one"},{"delay_ms":5000},{"Stop":"end_turn"}]})),
        Duration::from_secs(20),
    )
    .await;
    let mut client = daemon.client().await;
    let session = daemon.create(&mut client).await;
    client
        .rpc(
            2,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    assert_eq!(
        client
            .rpc(
                3,
                "session.prompt",
                json!({"session_id":session,"text":"again"})
            )
            .await["error"]["code"],
        -32002
    );
    client
        .rpc(4, "session.interrupt", json!({"session_id":session}))
        .await;
    let notes = client.finish().await;
    assert_eq!(
        notes.last().unwrap()["params"]["stop_reason"],
        "interrupted"
    );
    client
        .rpc(
            5,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    assert_eq!(
        client
            .rpc(6, "session.close", json!({"session_id":session}))
            .await["result"],
        json!({})
    );
    assert_eq!(
        client
            .rpc(7, "session.close", json!({"session_id":session}))
            .await["result"],
        json!({})
    );
    assert_eq!(
        client
            .rpc(8, "session.attach", json!({"session_id":session}))
            .await["error"]["code"],
        -32001
    );
}

#[tokio::test]
async fn idle_exit_waits_for_detached_turn_and_private_socket_is_removed() {
    let daemon = Daemon::start(
        Some(json!({"events":[{"delay_ms":250},{"TextDelta":"done"},{"Stop":"end_turn"}]})),
        Duration::from_millis(75),
    )
    .await;
    assert_eq!(
        fs::metadata(daemon.config.socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let mut client = daemon.client().await;
    let session = daemon.create(&mut client).await;
    client
        .rpc(
            2,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !daemon.task.is_finished(),
        "live detached turn keeps daemon alive"
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        daemon.task.is_finished(),
        "daemon should exit once turn completes plus idle timeout"
    );
    assert!(!daemon.config.socket.exists());
}

/// Leaves a socket file at `path` with nobody listening on it.
///
/// On macOS a listening socket can keep accepting connections for a moment after its descriptor is
/// closed, so a daemon that probed immediately would find a "live" daemon that was never there and
/// every starter would exit. Waiting until the path refuses connections makes the setup mean what
/// the test says it means.
fn leave_stale_socket(path: &Path) {
    drop(std::os::unix::net::UnixListener::bind(path).unwrap());
    for _ in 0..400 {
        if std::os::unix::net::UnixStream::connect(path).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("a closed listener kept accepting connections");
}

#[tokio::test]
async fn stale_socket_recovers_and_concurrent_start_converges() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().join("elefante.sock");
    leave_stale_socket(&path);
    let config = DaemonConfig {
        socket: path.clone(),
        credentials: dir.path().join("credentials.toml"),
        api_key: None,
        base_url: None,
        stub_provider: None,
        memory: true,
        learn: LearnMode::Off,
        learn_idle: Duration::from_secs(300),
        learn_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(1),
    };
    let mut first = tokio::spawn(run(config.clone()));
    let mut second = tokio::spawn(run(config));
    for _ in 0..100 {
        if UnixStream::connect(&path).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::timeout(Duration::from_millis(500), async {
        tokio::select! {
            result = &mut first => { result.unwrap().unwrap(); assert!(!second.is_finished()); },
            result = &mut second => { result.unwrap().unwrap(); assert!(!first.is_finished()); },
        }
    })
    .await
    .unwrap();
    // The starter that lost wrote no token, so the token on disk is the live daemon's.
    Client::connect(&path).await;
    first.abort();
    second.abort();
}

#[tokio::test]
async fn the_first_request_must_present_the_daemons_token() {
    let daemon = Daemon::start(None, Duration::from_secs(20)).await;
    let path = elefante::daemon::token_path(&daemon.config.socket);
    assert_eq!(path, daemon.config.socket.with_file_name("token"));
    let token = fs::read_to_string(&path).unwrap();
    assert_eq!(token.len(), 32, "128 bits as hex");
    assert!(
        token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{token}"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let wrong = format!(
        "{}{}",
        if token.starts_with('0') { "1" } else { "0" },
        &token[1..]
    );
    let frame = |request: Value| format!("{request}\n");
    for (case, request) in [
        ("invalid JSON", "not json\n".to_owned()),
        (
            "another method",
            frame(json!({"id":1,"method":"session.create","params":{"cwd":daemon.dir.path()}})),
        ),
        (
            "a wrong token",
            frame(json!({"id":1,"method":"auth","params":{"token":wrong}})),
        ),
        (
            "an empty token",
            frame(json!({"id":1,"method":"auth","params":{"token":""}})),
        ),
        (
            "no token",
            frame(json!({"id":1,"method":"auth","params":{}})),
        ),
    ] {
        let mut client = Client::unauthenticated(&daemon.config.socket).await;
        client.write.write_all(request.as_bytes()).await.unwrap();
        let reply = client.next().await;
        assert_eq!(reply["error"]["code"], -32008, "{case}: {reply}");
        let mut line = String::new();
        let closed = tokio::time::timeout(Duration::from_secs(2), client.read.read_line(&mut line))
            .await
            .unwrap();
        assert!(closed.is_err() || closed.unwrap() == 0, "{case}: {line}");
    }
    // Positive control: the right token opens the same daemon to the same request.
    let mut client = daemon.client().await;
    daemon.create(&mut client).await;
}

#[tokio::test]
async fn a_client_that_arrives_before_the_daemon_authenticates() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/elefante.sock");
    let waiting = tokio::spawn({
        let socket = socket.clone();
        async move {
            // The client polls only for the socket. The daemon writes the token before it
            // listens, so the token is there to read the moment a connection succeeds.
            while UnixStream::connect(&socket).await.is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Client::connect(&socket).await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished());
    let config = DaemonConfig {
        socket,
        credentials: dir.path().join("home/.elefante/credentials.toml"),
        api_key: None,
        base_url: None,
        stub_provider: None,
        memory: false,
        learn: LearnMode::Off,
        learn_idle: Duration::from_secs(300),
        learn_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(20),
    };
    let daemon = tokio::spawn(run(config));
    let mut client = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let reply = client
        .rpc(1, "session.create", json!({"cwd":dir.path()}))
        .await;
    assert!(reply["result"]["session_id"].is_string(), "{reply}");
    daemon.abort();
}

#[tokio::test]
async fn directory_ownership_serializes_stale_reclamation_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("elefante.sock");
    leave_stale_socket(&socket);
    let ownership = fs::File::open(dir.path()).unwrap();
    ownership.lock().unwrap();
    let config = DaemonConfig {
        socket: socket.clone(),
        credentials: dir.path().join("credentials.toml"),
        api_key: None,
        base_url: None,
        stub_provider: None,
        memory: true,
        learn: LearnMode::Off,
        learn_idle: Duration::from_secs(300),
        learn_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_secs(20),
    };
    let first = tokio::spawn(run(config.clone()));
    let second = tokio::spawn(run(config));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Emulate the starter that owns reclamation replacing the stale inode while
    // contenders are waiting. Neither contender may unlink its newly live socket.
    fs::remove_file(&socket).unwrap();
    let live = tokio::net::UnixListener::bind(&socket).unwrap();
    ownership.unlock().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    assert!(UnixStream::connect(&socket).await.is_ok());
    drop(live);
}

#[tokio::test]
async fn sessions_from_a_previous_daemon_cannot_alias_after_restart() {
    let mut daemon = Daemon::start(None, Duration::from_secs(20)).await;
    let mut client = daemon.client().await;
    let previous = daemon.create(&mut client).await;
    drop(client);
    daemon.task.abort();
    let _ = (&mut daemon.task).await;
    daemon.task = tokio::spawn(run(daemon.config.clone()));
    for _ in 0..100 {
        if UnixStream::connect(&daemon.config.socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut client = daemon.client().await;
    let current = daemon.create(&mut client).await;
    assert_ne!(current, previous);
    assert_eq!(
        client
            .rpc(2, "session.attach", json!({"session_id":previous}))
            .await["error"]["code"],
        -32001
    );
}

#[tokio::test]
async fn invalid_cold_start_finishes_validation_before_socket_publication() {
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("socket");
        if stale {
            leave_stale_socket(&socket);
        }
        // Holding ownership makes the ordering deterministic: validation must
        // return its named failure without reaching reclamation or bind at all.
        let ownership = fs::File::open(dir.path()).unwrap();
        ownership.lock().unwrap();
        let config = DaemonConfig {
            socket: socket.clone(),
            credentials: dir.path().join("credentials.toml"),
            api_key: Some("test-key".into()),
            base_url: Some("not-a-url".into()),
            stub_provider: None,
            memory: true,
            learn: LearnMode::Off,
            learn_idle: Duration::from_secs(300),
            learn_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(20),
        };
        let error = tokio::time::timeout(Duration::from_secs(1), run(config))
            .await
            .expect("invalid configuration must fail before trying to publish a socket")
            .unwrap_err();
        assert!(error.to_string().contains("OPENROUTER_BASE_URL is invalid"));
        assert!(UnixStream::connect(&socket).await.is_err());
        assert_eq!(
            socket.exists(),
            stale,
            "validation must leave the socket pathname untouched"
        );
        ownership.unlock().unwrap();
    }
}

#[tokio::test]
async fn warm_daemon_wins_over_invalid_starter_configuration() {
    let daemon = Daemon::start(None, Duration::from_secs(20)).await;
    let mut config = daemon.config.clone();
    config.api_key = Some("test-key".into());
    config.base_url = Some("not-a-url".into());
    let ownership = fs::File::open(config.socket.parent().unwrap()).unwrap();
    ownership.lock().unwrap();
    tokio::time::timeout(Duration::from_secs(1), run(config))
        .await
        .unwrap()
        .unwrap();
    ownership.unlock().unwrap();
    assert!(!daemon.task.is_finished());
    assert!(UnixStream::connect(&daemon.config.socket).await.is_ok());
}

#[tokio::test]
async fn missing_key_is_application_error_and_invalid_cwd_is_rejected() {
    let daemon = Daemon::start(None, Duration::from_secs(20)).await;
    let mut client = daemon.client().await;
    let invalid = client
        .rpc(
            1,
            "session.create",
            json!({"cwd":daemon.dir.path().join("absent")}),
        )
        .await;
    assert_eq!(invalid["error"]["code"], -32602);
    let session = daemon.create(&mut client).await;
    assert_eq!(
        client
            .rpc(
                2,
                "session.prompt",
                json!({"session_id":session,"text":"go"})
            )
            .await["error"]["code"],
        -32004
    );
    assert_eq!(
        client
            .rpc(3, "key.set", json!({"api_key":"test-key"}))
            .await["result"],
        json!({})
    );
    assert_eq!(
        fs::metadata(&daemon.config.credentials)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let response = client
        .rpc(
            4,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    assert!(
        response["result"]["turn_id"].is_string(),
        "key.set must replace absent cached provider"
    );
    client
        .rpc(5, "session.interrupt", json!({"session_id":session}))
        .await;
}

#[tokio::test]
async fn oversized_and_unterminated_frames_close_connection() {
    let daemon = Daemon::start(None, Duration::from_secs(20)).await;
    let mut client = daemon.client().await;
    let huge = vec![b' '; elefante::daemon::MAX_LINE_BYTES + 2];
    let _ = client.write.write_all(&huge).await;
    let mut line = String::new();
    let result = tokio::time::timeout(Duration::from_secs(2), client.read.read_line(&mut line))
        .await
        .unwrap();
    assert!(result.is_err() || result.unwrap() == 0);
    let mut client = daemon.client().await;
    client.write.write_all(b"{}").await.unwrap();
    client.write.shutdown().await.unwrap();
    assert_eq!(client.read.read_line(&mut line).await.unwrap(), 0);
}

fn tool_script(command: &str) -> Value {
    json!({"rounds":[[
        {"ToolUseStart":{"id":"call-1","name":"bash"}},
        {"ToolUseComplete":{"id":"call-1","input":{"command":command}}},
        {"Stop":"tool_use"}
    ],[{"TextDelta":"done"},{"Stop":"end_turn"}]]})
}

#[tokio::test]
async fn approval_fanout_allow_and_deny_are_effective_and_single_use() {
    for decision in ["allow", "deny"] {
        let daemon = Daemon::start(
            Some(tool_script("touch approved-marker")),
            Duration::from_secs(20),
        )
        .await;
        let mut first = daemon.client().await;
        let session = daemon.create(&mut first).await;
        let mut second = daemon.client().await;
        second
            .rpc(2, "session.attach", json!({"session_id":session}))
            .await;
        first
            .rpc(
                3,
                "session.prompt",
                json!({"session_id":session,"text":"go"}),
            )
            .await;
        let mut before = Vec::new();
        let requested = loop {
            let note = first.next().await;
            before.push(note.clone());
            if note["method"] == "approval.requested" {
                break note;
            }
        };
        assert!(!daemon.dir.path().join("approved-marker").exists());
        let approval_id = &requested["params"]["approval_id"];
        let snapshot = first
            .rpc(4, "session.attach", json!({"session_id":session}))
            .await;
        assert_eq!(
            snapshot["result"]["snapshot"]["active_turn"]["pending_approval"]["approval_id"],
            *approval_id
        );
        first
            .rpc(
                5,
                "session.resolve_approval",
                json!({"session_id":session,"approval_id":approval_id,"decision":decision}),
            )
            .await;
        let (tail, all) = tokio::join!(first.finish(), second.finish());
        before.extend(tail);
        assert_eq!(before, all);
        assert!(all.iter().any(|note| note["method"] == "approval.resolved"
            && note["params"]["approved"] == (decision == "allow")));
        assert_eq!(
            daemon.dir.path().join("approved-marker").exists(),
            decision == "allow"
        );
        assert_eq!(
            first
                .rpc(
                    6,
                    "session.resolve_approval",
                    json!({"session_id":session,"approval_id":approval_id,"decision":decision})
                )
                .await["error"]["code"],
            -32005
        );
    }
}

#[tokio::test]
async fn denied_command_never_requests_approval_and_pending_interrupt_pairs_history() {
    let daemon = Daemon::start(Some(tool_script("rm marker")), Duration::from_secs(20)).await;
    fs::write(daemon.dir.path().join("marker"), "keep").unwrap();
    let mut client = daemon.client().await;
    let session = daemon.create(&mut client).await;
    client
        .rpc(
            2,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    let events = client.finish().await;
    assert!(
        !events
            .iter()
            .any(|note| note["method"] == "approval.requested")
    );
    assert!(
        events
            .iter()
            .any(|note| note["method"] == "tool.finished" && note["params"]["is_error"] == true)
    );
    assert_eq!(
        fs::read_to_string(daemon.dir.path().join("marker")).unwrap(),
        "keep"
    );

    let daemon = Daemon::start(
        Some(tool_script("touch interrupted-marker")),
        Duration::from_secs(20),
    )
    .await;
    let mut client = daemon.client().await;
    let session = daemon.create(&mut client).await;
    client
        .rpc(
            2,
            "session.prompt",
            json!({"session_id":session,"text":"go"}),
        )
        .await;
    loop {
        if client.next().await["method"] == "approval.requested" {
            break;
        }
    }
    client
        .rpc(3, "session.interrupt", json!({"session_id":session}))
        .await;
    let events = client.finish().await;
    assert!(
        events.iter().any(
            |note| note["method"] == "approval.resolved" && note["params"]["approved"] == false
        )
    );
    assert_eq!(
        events.last().unwrap()["params"]["stop_reason"],
        "interrupted"
    );
    assert!(!daemon.dir.path().join("interrupted-marker").exists());
    client
        .rpc(
            4,
            "session.prompt",
            json!({"session_id":session,"text":"next"}),
        )
        .await;
    let events = client.finish().await;
    assert!(
        !events.iter().any(|note| note["method"] == "session.error"),
        "stub rejects any unpaired tool history"
    );
    assert_eq!(events.last().unwrap()["params"]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn key_set_rebuilds_provider_and_environment_key_wins() {
    use tokio::io::AsyncReadExt as _;
    for environment_key in [None, Some("environment-key")] {
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = http.local_addr().unwrap();
        let request_task = tokio::spawn(async move {
            let (mut stream, _) = http.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let count = stream.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = header
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"accepted\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            socket: dir.path().join("run/socket"),
            credentials: dir.path().join("home/.elefante/credentials.toml"),
            api_key: environment_key.map(str::to_owned),
            base_url: Some(format!("http://{address}")),
            stub_provider: None,
            memory: true,
            learn: LearnMode::Off,
            learn_idle: Duration::from_secs(300),
            learn_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(20),
        };
        let task = tokio::spawn(run(config.clone()));
        for _ in 0..100 {
            if UnixStream::connect(&config.socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let daemon = Daemon { dir, config, task };
        let mut client = daemon.client().await;
        let session = daemon.create(&mut client).await;
        if environment_key.is_none() {
            assert_eq!(
                client
                    .rpc(
                        2,
                        "session.prompt",
                        json!({"session_id":session,"text":"go"})
                    )
                    .await["error"]["code"],
                -32004
            );
        }
        assert_eq!(
            client
                .rpc(3, "key.set", json!({"api_key":"stored-key"}))
                .await["result"],
            json!({})
        );
        client
            .rpc(
                4,
                "session.prompt",
                json!({"session_id":session,"text":"go"}),
            )
            .await;
        let notes = client.finish().await;
        assert!(
            notes
                .iter()
                .any(|note| note["method"] == "text.delta" && note["params"]["text"] == "accepted")
        );
        assert!(!notes.iter().any(|note| note["method"] == "session.error"));
        let request = request_task.await.unwrap().to_lowercase();
        assert!(request.contains(&format!(
            "authorization: bearer {}",
            environment_key.unwrap_or("stored-key")
        )));
    }
}
