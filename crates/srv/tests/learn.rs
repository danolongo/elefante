//! Learning through a real daemon: when the extractor is asked, what it is asked, that its answer
//! reaches the next prompt, and that the switches really switch it off. The model is the stub
//! provider, whose extraction replies are scripted separately from the turn it is serving.

use std::{fs, path::Path, time::Duration};

use elefante_srv::{DaemonConfig, run};
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
            if self.next().await["method"] == "turn.finished" {
                return;
            }
        }
    }
}

struct Options {
    memory: bool,
    learn: bool,
    learn_idle: Duration,
    idle_timeout: Duration,
    extract: Value,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            memory: true,
            learn: true,
            learn_idle: Duration::from_secs(300),
            idle_timeout: Duration::from_secs(60),
            extract: json!([]),
        }
    }
}

struct Harness {
    dir: tempfile::TempDir,
    repo: tempfile::TempDir,
    config: DaemonConfig,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Harness {
    async fn start(options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        fs::create_dir_all(repo.path().join("src")).unwrap();
        fs::write(
            repo.path().join("src/lib.rs"),
            "//! Branch tooling.\n\n/// Names a branch.\npub fn name_branch() {}\n",
        )
        .unwrap();
        let script = dir.path().join("script.json");
        let value = json!({
            "rounds": [[{"TextDelta": "ok"}, {"Stop": "end_turn"}]],
            "extract": options.extract,
            "record_requests": dir.path().join("requests.jsonl"),
        });
        fs::write(&script, serde_json::to_vec(&value).unwrap()).unwrap();
        let config = DaemonConfig {
            socket: dir.path().join("run/elefante.sock"),
            credentials: dir.path().join("home/.elefante/credentials.toml"),
            api_key: None,
            base_url: Some("http://127.0.0.1:9".into()),
            stub_provider: Some(script),
            idle_timeout: options.idle_timeout,
            memory: options.memory,
            learn: options.learn,
            learn_idle: options.learn_idle,
            learn_timeout: Duration::from_secs(5),
        };
        let task = tokio::spawn(run(config.clone()));
        for _ in 0..200 {
            if UnixStream::connect(&config.socket).await.is_ok() {
                return Self {
                    dir,
                    repo,
                    config,
                    task: Some(task),
                };
            }
            assert!(!task.is_finished(), "daemon failed at startup");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("daemon did not become ready");
    }

    async fn session(&self, client: &mut Client) -> String {
        let result = client
            .rpc(1, "session.create", json!({"cwd": self.repo.path()}))
            .await;
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

    /// The learned notes now stored: `(statement, status)`.
    fn notes(&self) -> Vec<(String, String)> {
        let Some(db) = self.db() else {
            return Vec::new();
        };
        let mut statement = db
            .prepare("SELECT body, status FROM node WHERE source LIKE 'learned:%' ORDER BY id")
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    async fn notes_appear(&self, count: usize) -> Vec<(String, String)> {
        for _ in 0..400 {
            let notes = self.notes();
            if notes.len() >= count {
                return notes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {count} learned notes, found {:?}", self.notes());
    }

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

    /// Requests that carried a response schema: the extractor's.
    fn extractions(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.get("response_schema").is_some())
            .collect()
    }

    /// The turns' own requests.
    fn turns(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r.get("response_schema").is_none())
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn reply(statement: &str, quote: &str) -> String {
    json!({"memories": [{
        "kind": "convention", "label": "Branch naming", "statement": statement,
        "anchors": ["src/lib.rs"], "quote": quote,
        "relation": {"type": "new", "target": null},
    }]})
    .to_string()
}

const BRANCHES: &str = "Branches are named after their ticket number.";
const BRANCHES_QUOTE: &str = "name branches after the ticket number";

fn all_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().unwrap())
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_cue_makes_the_daemon_ask_and_the_answer_reaches_the_next_prompt() {
    let h = Harness::start(Options {
        extract: json!([reply(BRANCHES, BRANCHES_QUOTE)]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    h.indexed().await;

    h.prompt(
        &mut client,
        &session,
        "Remember: name branches after the ticket number.",
    )
    .await;
    let notes = h.notes_appear(1).await;
    assert_eq!(notes, vec![(BRANCHES.to_owned(), "active".to_owned())]);
    assert_eq!(h.extractions().len(), 1);
    let first_turn = &h.turns()[0];
    assert!(
        !all_text(first_turn).contains("<repo-memory"),
        "nothing was known when the first turn began"
    );

    h.prompt(&mut client, &session, "How should I name this branch?")
        .await;
    let second = h.turns().last().unwrap().clone();
    let text = all_text(&second);
    assert!(text.contains("<repo-memory generation="), "{text}");
    assert!(text.contains(&format!("convention: {BRANCHES}")), "{text}");
    assert!(
        text.contains("src/lib.rs"),
        "and the file the note is about came along: {text}"
    );
    // The block is context, not history: no extraction ever read it back.
    for request in h.extractions() {
        assert!(!all_text(&request).contains("<repo-memory"));
    }
}

#[tokio::test]
async fn the_extractor_is_not_served_from_the_turns_script() {
    let h = Harness::start(Options {
        extract: json!([reply(BRANCHES, BRANCHES_QUOTE)]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    client
        .rpc(
            2,
            "session.prompt",
            json!({"session_id": session, "text": "Remember: name branches after the ticket number."}),
        )
        .await;
    let finished = loop {
        let note = client.next().await;
        if note["method"] == "turn.finished" {
            break note;
        }
    };
    assert_eq!(finished["params"]["stop_reason"], "end_turn");
    h.notes_appear(1).await;
    assert_eq!(h.turns().len(), 1, "extraction took no scripted round");
}

#[tokio::test]
async fn ordinary_chatter_is_held_and_closing_the_session_extracts_it() {
    let h = Harness::start(Options {
        extract: json!([reply(
            "Deploys use the blue pipeline.",
            "we deploy with the blue pipeline"
        )]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    h.prompt(
        &mut client,
        &session,
        "FYI we deploy with the blue pipeline.",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        h.extractions().is_empty(),
        "no cue, little text: the gate waits"
    );

    client
        .rpc(3, "session.close", json!({"session_id": session}))
        .await;
    let notes = h.notes_appear(1).await;
    assert_eq!(notes[0].0, "Deploys use the blue pipeline.");
    assert_eq!(h.extractions().len(), 1);
}

#[tokio::test]
async fn a_quiet_session_is_extracted_without_being_closed() {
    let h = Harness::start(Options {
        learn_idle: Duration::from_millis(150),
        extract: json!([reply(
            "Deploys use the blue pipeline.",
            "we deploy with the blue pipeline"
        )]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    h.prompt(
        &mut client,
        &session,
        "FYI we deploy with the blue pipeline.",
    )
    .await;
    let notes = h.notes_appear(1).await;
    assert_eq!(notes[0].0, "Deploys use the blue pipeline.");
}

#[tokio::test]
async fn a_new_turn_restarts_the_quiet_wait() {
    let h = Harness::start(Options {
        learn_idle: Duration::from_millis(400),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    for _ in 0..3 {
        h.prompt(&mut client, &session, "fix a small thing").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(
        h.extractions().is_empty(),
        "turns kept arriving, so nobody was idle yet"
    );
    for _ in 0..200 {
        if !h.extractions().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the idle flush never fired");
}

#[tokio::test]
async fn a_session_that_never_closed_is_extracted_as_the_daemon_exits() {
    let mut h = Harness::start(Options {
        idle_timeout: Duration::from_millis(300),
        extract: json!([reply(
            "Deploys use the blue pipeline.",
            "we deploy with the blue pipeline"
        )]),
        ..Options::default()
    })
    .await;
    {
        let mut client = Client::connect(&h.config.socket).await;
        let session = h.session(&mut client).await;
        h.prompt(
            &mut client,
            &session,
            "FYI we deploy with the blue pipeline.",
        )
        .await;
    }
    // The client is gone without closing; the daemon idles out and flushes on its way.
    let task = h.task.take().unwrap();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("the daemon exits when idle")
        .unwrap()
        .unwrap();
    assert_eq!(h.notes().len(), 1);
}

#[tokio::test]
async fn learning_off_asks_nothing_but_memory_still_works() {
    let h = Harness::start(Options {
        learn: false,
        extract: json!([reply(BRANCHES, BRANCHES_QUOTE)]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    h.indexed().await;
    h.prompt(
        &mut client,
        &session,
        "Remember: name branches after the ticket number.",
    )
    .await;
    h.prompt(&mut client, &session, "Remember: always run the tests.")
        .await;
    client
        .rpc(3, "session.close", json!({"session_id": session}))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.extractions().is_empty());
    assert!(h.notes().is_empty());
    assert!(
        all_text(h.turns().last().unwrap()).contains("<repo-brief>")
            || all_text(h.turns().last().unwrap()).contains("repo-index"),
        "the index still serves context"
    );
}

#[tokio::test]
async fn memory_off_creates_nothing_and_asks_nothing() {
    let h = Harness::start(Options {
        memory: false,
        learn: false,
        extract: json!([reply(BRANCHES, BRANCHES_QUOTE)]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    h.prompt(
        &mut client,
        &session,
        "Remember: name branches after the ticket number.",
    )
    .await;
    client
        .rpc(3, "session.close", json!({"session_id": session}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.extractions().is_empty());
    assert!(!h.repo.path().join(".elefante").exists());
}

#[tokio::test]
async fn the_key_lives_beside_the_credentials_and_never_in_the_repository() {
    let h = Harness::start(Options::default()).await;
    let mut client = Client::connect(&h.config.socket).await;
    let _ = h.session(&mut client).await;
    h.indexed().await;
    let key = h.dir.path().join("home/.elefante/memory.key");
    assert!(key.exists());
    let bytes = fs::read(&key).unwrap();
    assert_eq!(bytes.len(), 32);
    for entry in walk(h.repo.path()) {
        let contents = fs::read(&entry).unwrap_or_default();
        assert!(
            !contents.windows(32).any(|w| w == bytes.as_slice()),
            "the key leaked into {}",
            entry.display()
        );
    }
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn an_invented_quote_is_refused_and_a_real_one_is_kept() {
    let h = Harness::start(Options {
        extract: json!([
            reply(BRANCHES, "branches are named after the ticket number"),
            reply(BRANCHES, BRANCHES_QUOTE)
        ]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let session = h.session(&mut client).await;
    h.indexed().await;
    // The stub's turn text is "ok"; the quote is therefore in neither speaker's words, so the
    // first extraction is refused outright, which is the point: no invented evidence is kept.
    h.prompt(&mut client, &session, "Remember: keep going.")
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.notes().is_empty());
    h.prompt(
        &mut client,
        &session,
        "Remember: name branches after the ticket number.",
    )
    .await;
    let notes = h.notes_appear(1).await;
    assert_eq!(notes[0].1, "active");
}
