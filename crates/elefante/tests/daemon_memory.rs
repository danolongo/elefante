//! The repository index through a real daemon: what a model is sent, what `memory_search` returns,
//! and that switching memory off really switches it off.

use std::{fs, path::Path, time::Duration};

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
    async fn connect(path: &Path) -> Self {
        let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
        Self {
            read: BufReader::new(read),
            write,
        }
    }

    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        let mut bytes =
            serde_json::to_vec(&json!({"id":id,"method":method,"params":params})).unwrap();
        bytes.push(b'\n');
        self.write.write_all(&bytes).await.unwrap();
        loop {
            let value = self.next().await;
            if value["id"] == id {
                return value;
            }
        }
    }

    async fn next(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), self.read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(read > 0, "connection closed unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    async fn finish(&mut self) {
        loop {
            let note = self.next().await;
            if note["method"] == "turn.finished" {
                return;
            }
        }
    }
}

struct Harness {
    dir: tempfile::TempDir,
    repo: tempfile::TempDir,
    config: DaemonConfig,
    task: JoinHandle<anyhow::Result<()>>,
}

const LIB: &str = "//! Frobnicator core.\n\n/// Frobnicates widgets.\npub fn frobnicate() {}\n";

impl Harness {
    async fn start(memory: bool, rounds: Value, files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        for (path, contents) in files {
            let target = repo.path().join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, contents).unwrap();
        }
        let script = dir.path().join("script.json");
        let mut value = json!({"rounds": rounds});
        value["record_requests"] = json!(dir.path().join("requests.jsonl"));
        fs::write(&script, serde_json::to_vec(&value).unwrap()).unwrap();
        let config = DaemonConfig {
            socket: dir.path().join("run/elefante.sock"),
            credentials: dir.path().join("home/.elefante/credentials.toml"),
            api_key: None,
            base_url: Some("http://127.0.0.1:9".into()),
            stub_provider: Some(script),
            idle_timeout: Duration::from_secs(60),
            memory,
            learn: LearnMode::Off,
            learn_idle: Duration::from_secs(300),
            learn_timeout: Duration::from_secs(60),
        };
        let task = tokio::spawn(run(config.clone()));
        for _ in 0..200 {
            if UnixStream::connect(&config.socket).await.is_ok() {
                return Self {
                    dir,
                    repo,
                    config,
                    task,
                };
            }
            assert!(!task.is_finished(), "daemon failed at startup");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("daemon did not become ready");
    }

    async fn session(&self, client: &mut Client, cwd: &Path) -> String {
        let result = client.rpc(1, "session.create", json!({"cwd": cwd})).await;
        result["result"]["session_id"].as_str().unwrap().to_owned()
    }

    async fn prompt(&self, client: &mut Client, session: &str, text: &str) {
        client
            .rpc(
                2,
                "session.prompt",
                json!({"session_id": session, "text": text}),
            )
            .await;
        client.finish().await;
    }

    fn db(&self) -> Option<rusqlite::Connection> {
        rusqlite::Connection::open_with_flags(
            self.repo.path().join(".elefante/memory.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()
    }

    /// Waits until the first full index has finished, which the daemon does in the background.
    async fn indexed(&self) {
        for _ in 0..300 {
            let done = self.db().is_some_and(|db| {
                db.query_row(
                    "SELECT value FROM meta WHERE key = 'index_complete'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .is_ok_and(|v| v == "1")
            });
            if done {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the repository was never indexed");
    }

    fn requests(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn texts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|block| match block["type"].as_str().unwrap() {
                    "text" => block["text"].as_str().unwrap().to_owned(),
                    "tool_use" => format!("[tool_use {}]", block["name"].as_str().unwrap()),
                    "tool_result" => {
                        format!("[tool_result {}]", block["content"].as_str().unwrap())
                    }
                    other => format!("[{other}]"),
                })
                .collect::<String>()
        })
        .collect()
}

fn search_then_answer(query: &str) -> Value {
    json!([
        [
            {"ToolUseStart": {"id": "call-1", "name": "memory_search"}},
            {"ToolUseComplete": {"id": "call-1", "input": {"query": query}}},
            {"Stop": "tool_use"}
        ],
        [{"TextDelta": "done"}, {"Stop": "end_turn"}]
    ])
}

fn answer() -> Value {
    json!([[{"TextDelta": "ok"}, {"Stop": "end_turn"}]])
}

#[tokio::test]
async fn a_session_is_given_a_brief_and_a_block_and_memory_search_finds_the_symbol() {
    let h = Harness::start(
        true,
        search_then_answer("frobnicate widgets"),
        &[("src/lib.rs", LIB), ("README.md", "# Frobnicator\n")],
    )
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client, h.repo.path()).await;
    h.indexed().await;
    h.prompt(&mut client, &session, "where do we frobnicate widgets")
        .await;

    let requests = h.requests();
    assert_eq!(requests.len(), 2, "one tool round, one answer");
    let first = texts(&requests[0]);
    assert!(first[0].starts_with("<repo-brief>"), "{first:?}");
    assert_eq!(first[1], "where do we frobnicate widgets");
    assert!(
        first[2].starts_with("<repo-index"),
        "the block follows the prompt: {first:?}"
    );
    assert!(first[2].contains("src/lib.rs"));
    assert!(first[2].contains("fn frobnicate · l4 · Frobnicates widgets."));

    // Gate: the tool answers with the symbol's path and line.
    let second = texts(&requests[1]);
    assert_eq!(second[..3], first[..3], "the same prefix on the next round");
    let result = second.last().unwrap();
    assert!(
        result.contains("src/lib.rs:4 fn frobnicate — Frobnicates widgets."),
        "{result}"
    );
}

#[tokio::test]
async fn no_request_carries_a_system_message_other_than_the_configured_prompt() {
    let h = Harness::start(
        true,
        search_then_answer("frobnicate"),
        &[("src/lib.rs", LIB)],
    )
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client, h.repo.path()).await;
    h.indexed().await;
    h.prompt(&mut client, &session, "frobnicate").await;

    for request in h.requests() {
        assert_eq!(request["system"].as_array().unwrap().len(), 1);
        for message in request["messages"].as_array().unwrap() {
            assert_ne!(
                message["role"], "system",
                "repository text must not carry system authority"
            );
        }
    }
}

#[tokio::test]
async fn the_next_turn_gets_a_fresh_block_not_the_previous_one() {
    let h = Harness::start(
        true,
        json!([
            [{"TextDelta": "one"}, {"Stop": "end_turn"}],
            [{"TextDelta": "two"}, {"Stop": "end_turn"}]
        ]),
        &[
            ("src/lib.rs", LIB),
            ("docs/readme.md", "# Readme about gizmos\n"),
        ],
    )
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client, h.repo.path()).await;
    h.indexed().await;
    h.prompt(&mut client, &session, "frobnicate widgets").await;
    h.prompt(&mut client, &session, "readme about gizmos").await;

    let requests = h.requests();
    let second = texts(&requests[1]);
    let blocks: Vec<&String> = second
        .iter()
        .filter(|t| t.starts_with("<repo-index"))
        .collect();
    assert_eq!(blocks.len(), 1, "exactly this turn's block: {second:?}");
    assert!(blocks[0].contains("docs/readme.md"));
    assert!(
        !blocks[0].contains("frobnicate"),
        "the first turn's block is gone"
    );
    assert_eq!(
        second[0],
        texts(&requests[0])[0],
        "the brief is frozen for the session"
    );
}

#[tokio::test]
async fn edits_made_between_turns_reach_the_index_without_a_new_session() {
    let h = Harness::start(
        true,
        json!([[{"TextDelta": "one"}, {"Stop": "end_turn"}], [{"TextDelta": "two"}, {"Stop": "end_turn"}]]),
        &[("src/lib.rs", LIB)],
    )
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client, h.repo.path()).await;
    h.indexed().await;
    h.prompt(&mut client, &session, "frobnicate").await;

    let target = h.repo.path().join("src/lib.rs");
    fs::write(
        &target,
        format!("{LIB}\n/// Added later.\npub fn newly_added() {{}}\n"),
    )
    .unwrap();
    // Age it past the racy-timestamp window so a stat-based refresh trusts the new size.
    fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(60))
        .unwrap();
    h.prompt(&mut client, &session, "frobnicate").await;

    for _ in 0..300 {
        let present = h.db().is_some_and(|db| {
            db.query_row(
                "SELECT count(*) FROM node WHERE key = 'src/lib.rs::newly_added'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .is_ok_and(|n| n == 1)
        });
        if present {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("a turn's refresh never picked up the edit");
}

#[tokio::test]
async fn a_session_in_a_subdirectory_sees_only_its_subtree_while_the_repository_is_indexed() {
    let h = Harness::start(
        true,
        search_then_answer("frobnicate"),
        &[
            (
                "sub/a.rs",
                "/// Frobnicates widgets.\npub fn frobnicate() {}\n",
            ),
            (
                "other/b.rs",
                "/// Frobnicates other widgets.\npub fn frobnicate_other() {}\n",
            ),
        ],
    )
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client, &h.repo.path().join("sub")).await;
    h.indexed().await;
    h.prompt(&mut client, &session, "frobnicate").await;

    let requests = h.requests();
    let combined = requests
        .iter()
        .flat_map(texts)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(combined.contains("sub/a.rs"), "{combined}");
    assert!(
        !combined.contains("other/b.rs"),
        "a hit outside the session's root is noise:\n{combined}"
    );
    assert!(
        h.repo.path().join(".elefante").is_dir(),
        "the store lives at the repository root"
    );
    assert!(!h.repo.path().join("sub/.elefante").exists());
}

#[tokio::test]
async fn two_sessions_in_one_repository_share_one_store() {
    let h = Harness::start(true, answer(), &[("src/lib.rs", LIB)]).await;
    let mut client = Client::connect(&h.config.socket).await;
    let first = h.session(&mut client, h.repo.path()).await;
    let second = h.session(&mut client, h.repo.path()).await;
    assert_ne!(first, second);
    h.indexed().await;
    let mut names: Vec<String> = fs::read_dir(h.repo.path().join(".elefante"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.retain(|n| !n.ends_with("-wal") && !n.ends_with("-shm"));
    names.sort();
    assert_eq!(names, [".gitignore", "cache.db", "memory.db"]);
}

#[tokio::test]
async fn switching_memory_off_writes_nothing_and_adds_nothing() {
    let h = Harness::start(
        false,
        search_then_answer("frobnicate"),
        &[("src/lib.rs", LIB)],
    )
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client, h.repo.path()).await;
    h.prompt(&mut client, &session, "frobnicate widgets").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !h.repo.path().join(".elefante").exists(),
        "gate: no store directory may be created"
    );
    let requests = h.requests();
    for request in &requests {
        for text in texts(request) {
            assert!(
                !text.contains("<repo-brief") && !text.contains("<repo-index"),
                "no context when memory is off: {text}"
            );
        }
    }
    assert_eq!(texts(&requests[0]), ["frobnicate widgets"]);
    let refusal = texts(&requests[1]).pop().unwrap();
    assert_eq!(
        refusal,
        "[tool_result memory_search is unavailable: this session has no repository index]"
    );
    let result = client
        .rpc(3, "memory.list", json!({"cwd":h.repo.path()}))
        .await;
    assert_eq!(result["error"]["code"], -32007);
}

#[tokio::test]
async fn a_session_started_where_indexing_is_refused_still_works() {
    // The filesystem root is never indexed; the session must run exactly as if memory were off.
    let h = Harness::start(true, answer(), &[]).await;
    let mut client = Client::connect(&h.config.socket).await;
    let result = client.rpc(1, "session.create", json!({"cwd": "/"})).await;
    let session = result["result"]["session_id"].as_str().unwrap().to_owned();
    h.prompt(&mut client, &session, "hello").await;
    assert_eq!(texts(&h.requests()[0]), ["hello"]);
}
