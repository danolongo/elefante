//! Learning through a real daemon: when the extractor is asked, what it is asked, that its answer
//! reaches the next prompt, and that the switches really switch it off. The model is the stub
//! provider, whose extraction replies are scripted separately from the turn it is serving.

use std::{fs, path::Path, time::Duration};

use elefante_mem::{
    MemKey,
    consent::{self, CONSENT_FILE, ConsentAnswer, ConsentRecord},
    trust::KEY_FILE,
};
use elefante_rpc::Usage;
use elefante_srv::{DaemonConfig, LearnMode, run};
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
    learn: LearnMode,
    learn_idle: Duration,
    idle_timeout: Duration,
    extract: Value,
    extract_usage: Usage,
    preconsent: Option<ConsentRecord>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            memory: true,
            learn: LearnMode::On,
            learn_idle: Duration::from_secs(300),
            idle_timeout: Duration::from_secs(60),
            extract: json!([]),
            extract_usage: Usage::default(),
            preconsent: None,
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
            "extract_usage": options.extract_usage,
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
        if let Some(record) = options.preconsent {
            let parent = config.credentials.parent().unwrap();
            let key = MemKey::load_or_create(&parent.join(KEY_FILE)).unwrap();
            consent::write(&parent.join(CONSENT_FILE), &key, &record).unwrap();
        }
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
        let result = self.create(client, 1, None).await;
        result["result"]["session_id"].as_str().unwrap().to_owned()
    }

    async fn create(&self, client: &mut Client, id: u64, model: Option<&str>) -> Value {
        let mut params = json!({"cwd": self.repo.path()});
        if let Some(model) = model {
            params["model"] = model.into();
        }
        client.rpc(id, "session.create", params).await
    }

    async fn restart(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        let task = tokio::spawn(run(self.config.clone()));
        for _ in 0..200 {
            if UnixStream::connect(&self.config.socket).await.is_ok() {
                self.task = Some(task);
                return;
            }
            assert!(!task.is_finished(), "daemon failed on restart");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("daemon did not restart");
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

fn consent_record(answer: ConsentAnswer, extractor: &str) -> ConsentRecord {
    ConsentRecord {
        answer,
        extractor: extractor.to_owned(),
    }
}

#[tokio::test]
async fn standalone_consent_targets_the_configured_default_extractor() {
    let h = Harness::start(Options {
        learn: LearnMode::Ask,
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    assert_eq!(
        client
            .rpc(1, "memory.consent", json!({"answer":"learn"}))
            .await["result"]["consent"],
        "granted"
    );
    let created = h.create(&mut client, 2, None).await;
    assert_eq!(created["result"]["memory"]["consent"], "granted");
    assert_eq!(
        created["result"]["memory"]["extractor"],
        "anthropic/claude-sonnet-5"
    );
}

#[tokio::test]
async fn unset_consent_reads_no_text_and_grant_is_not_retroactive() {
    let h = Harness::start(Options {
        learn: LearnMode::Ask,
        extract: json!([reply(
            "Branches are named from the post-consent ticket.",
            "post-consent ticket names branches"
        )]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let created = h.create(&mut client, 1, None).await;
    assert_eq!(created["result"]["memory"]["consent"], "unset");
    assert_eq!(
        created["result"]["memory"]["extractor"],
        "anthropic/claude-sonnet-5"
    );
    let session = created["result"]["session_id"].as_str().unwrap();

    h.prompt(
        &mut client,
        session,
        "Remember: this pre-consent phrase must never be extracted.",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(h.extractions().is_empty());
    assert!(h.notes().is_empty());

    assert_eq!(
        client
            .rpc(3, "memory.consent", json!({"answer":"learn"}))
            .await["result"]["consent"],
        "granted"
    );
    h.prompt(
        &mut client,
        session,
        "Remember: post-consent ticket names branches.",
    )
    .await;
    h.notes_appear(1).await;
    let requests = h.extractions();
    assert_eq!(requests.len(), 1);
    assert!(!all_text(&requests[0]).contains("pre-consent phrase"));
    assert!(all_text(&requests[0]).contains("post-consent ticket"));
}

#[tokio::test]
async fn answers_are_shared_persisted_extractor_bound_and_connection_local() {
    let mut h = Harness::start(Options {
        learn: LearnMode::Ask,
        ..Options::default()
    })
    .await;
    let mut first = Client::connect(&h.config.socket).await;
    let mut second = Client::connect(&h.config.socket).await;
    let one = h.create(&mut first, 1, Some("provider/one")).await;
    let two = h.create(&mut second, 1, Some("provider/one")).await;
    assert_eq!(one["result"]["memory"]["consent"], "unset");
    assert_eq!(two["result"]["memory"]["consent"], "unset");
    assert_eq!(
        first
            .rpc(2, "memory.consent", json!({"answer":"learn"}))
            .await["result"]["consent"],
        "granted"
    );
    let session_two = two["result"]["session_id"].as_str().unwrap();
    let attached = second
        .rpc(2, "session.attach", json!({"session_id":session_two}))
        .await;
    assert_eq!(attached["result"]["memory"]["consent"], "granted");
    drop(first);
    drop(second);

    h.restart().await;
    let mut client = Client::connect(&h.config.socket).await;
    let matching = h.create(&mut client, 3, Some("provider/one")).await;
    assert_eq!(matching["result"]["memory"]["consent"], "granted");
    let changed = h.create(&mut client, 4, Some("provider/two")).await;
    assert_eq!(changed["result"]["memory"]["consent"], "unset");
    assert_eq!(changed["result"]["memory"]["extractor"], "provider/two");
    assert_eq!(
        client
            .rpc(5, "memory.consent", json!({"answer":"off"}))
            .await["result"]["consent"],
        "declined"
    );
    drop(client);

    h.restart().await;
    let mut client = Client::connect(&h.config.socket).await;
    let declined = h.create(&mut client, 6, Some("provider/two")).await;
    assert_eq!(declined["result"]["memory"]["consent"], "declined");
    let old_extractor = h.create(&mut client, 7, Some("provider/one")).await;
    assert_eq!(old_extractor["result"]["memory"]["consent"], "unset");
}

#[tokio::test]
async fn operator_modes_override_files_and_tampering_returns_to_unset() {
    let on = Harness::start(Options {
        learn: LearnMode::On,
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&on.config.socket).await;
    let created = on.create(&mut client, 1, None).await;
    assert_eq!(created["result"]["memory"]["consent"], "granted");
    assert!(
        !on.config
            .credentials
            .parent()
            .unwrap()
            .join(CONSENT_FILE)
            .exists()
    );
    assert_eq!(
        client
            .rpc(2, "memory.consent", json!({"answer":"off"}))
            .await["result"]["consent"],
        "granted"
    );
    assert!(
        !on.config
            .credentials
            .parent()
            .unwrap()
            .join(CONSENT_FILE)
            .exists()
    );

    let off = Harness::start(Options {
        learn: LearnMode::Off,
        preconsent: Some(consent_record(
            ConsentAnswer::Learn,
            "anthropic/claude-sonnet-5",
        )),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&off.config.socket).await;
    let created = off.create(&mut client, 1, None).await;
    assert_eq!(created["result"]["memory"]["consent"], "off");
    assert_eq!(created["result"]["memory"]["extractor"], Value::Null);
    assert_eq!(
        client
            .rpc(2, "memory.consent", json!({"answer":"learn"}))
            .await["result"]["consent"],
        "off"
    );

    let mut tampered = Harness::start(Options {
        learn: LearnMode::Ask,
        preconsent: Some(consent_record(
            ConsentAnswer::Learn,
            "anthropic/claude-sonnet-5",
        )),
        ..Options::default()
    })
    .await;
    let path = tampered
        .config
        .credentials
        .parent()
        .unwrap()
        .join(CONSENT_FILE);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["answer"] = "off".into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    tampered.restart().await;
    let mut client = Client::connect(&tampered.config.socket).await;
    let created = tampered.create(&mut client, 1, None).await;
    assert_eq!(created["result"]["memory"]["consent"], "unset");
}

#[tokio::test]
async fn revocation_discards_pending_text_before_idle_close_or_shutdown_flushes() {
    let mut h = Harness::start(Options {
        learn: LearnMode::Ask,
        learn_idle: Duration::from_millis(150),
        idle_timeout: Duration::from_millis(300),
        extract: json!([reply(
            "Deploys use the private pipeline.",
            "we deploy with the private pipeline"
        )]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let created = h.create(&mut client, 1, None).await;
    let session = created["result"]["session_id"].as_str().unwrap();
    client
        .rpc(2, "memory.consent", json!({"answer":"learn"}))
        .await;
    h.prompt(
        &mut client,
        session,
        "FYI we deploy with the private pipeline.",
    )
    .await;
    client
        .rpc(3, "memory.consent", json!({"answer":"off"}))
        .await;
    client
        .rpc(4, "session.close", json!({"session_id":session}))
        .await;
    drop(client);
    let task = h.task.take().unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(h.extractions().is_empty());
    assert!(h.notes().is_empty());
}

#[tokio::test]
async fn memory_status_and_events_restore_counts_health_and_background_spend() {
    let h = Harness::start(Options {
        learn: LearnMode::On,
        extract: json!([reply(BRANCHES, BRANCHES_QUOTE)]),
        extract_usage: Usage {
            input_tokens: 90,
            output_tokens: 10,
            reasoning_tokens: 3,
            cached_input_tokens: 4,
            cache_write_tokens: 5,
            cost: Some(0.0125),
        },
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let created = h.create(&mut client, 1, None).await;
    let session = created["result"]["session_id"].as_str().unwrap();
    let initial = &created["result"]["memory"];
    assert_eq!(initial["enabled"], true);
    assert_eq!(initial["consent"], "granted");
    assert_eq!(initial["learning"], "working");
    assert_eq!(initial["notes"], json!({"active":0,"pending":0}));
    assert_eq!(initial["spend"]["cost"], Value::Null);

    h.prompt(
        &mut client,
        session,
        "Remember: name branches after the ticket number.",
    )
    .await;
    h.notes_appear(1).await;

    let attached = client
        .rpc(3, "session.attach", json!({"session_id":session}))
        .await;
    let status = &attached["result"]["memory"];
    assert_eq!(status["notes"], json!({"active":1,"pending":0}));
    assert_eq!(status["spend"]["input_tokens"], 90);
    assert_eq!(status["spend"]["output_tokens"], 10);
    assert_eq!(status["spend"]["reasoning_tokens"], 3);
    assert_eq!(status["spend"]["cost"], 0.0125);

    let replay = client
        .rpc(4, "session.attach", json!({"session_id":session,"since":0}))
        .await;
    let events = replay["result"]["events"].as_array().unwrap();
    let learned = events
        .iter()
        .find(|event| event["method"] == "memory.learned")
        .expect("a learned-note hint is replayable");
    assert_eq!(learned["turn_id"], Value::Null);
    assert_eq!(learned["params"]["change"], "added");
    assert_eq!(learned["params"]["note"]["statement"], BRANCHES);
    assert!(
        events
            .iter()
            .any(|event| event["method"] == "memory.status")
    );
}

#[tokio::test]
async fn repository_memory_methods_list_forget_and_report_named_errors_by_cwd() {
    let h = Harness::start(Options {
        learn: LearnMode::On,
        extract: json!([reply(BRANCHES, BRANCHES_QUOTE)]),
        ..Options::default()
    })
    .await;
    let mut client = Client::connect(&h.config.socket).await;
    let created = h.create(&mut client, 1, None).await;
    let session = created["result"]["session_id"].as_str().unwrap();
    h.prompt(
        &mut client,
        session,
        "Remember: name branches after the ticket number.",
    )
    .await;
    h.notes_appear(1).await;

    let listed = client
        .rpc(3, "memory.list", json!({"cwd":h.repo.path()}))
        .await;
    let notes = listed["result"]["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["statement"], BRANCHES);
    let note_id = notes[0]["id"].as_i64().unwrap();
    assert!(notes[0]["confidence"].as_f64().is_some());
    assert_eq!(notes[0]["verified"], true);

    assert_eq!(
        client
            .rpc(
                4,
                "memory.forget",
                json!({"cwd":h.repo.path(),"id":note_id}),
            )
            .await["result"],
        json!({})
    );
    let status = client
        .rpc(5, "memory.status", json!({"cwd":h.repo.path()}))
        .await;
    assert_eq!(status["result"]["notes"], json!({"active":0,"pending":0}));
    let missing = client
        .rpc(
            6,
            "memory.forget",
            json!({"cwd":h.repo.path(),"id":note_id}),
        )
        .await;
    assert_eq!(missing["error"]["code"], -32006);
    assert_eq!(
        client
            .rpc(7, "memory.clear", json!({"cwd":h.repo.path()}))
            .await["result"]["removed"],
        0
    );
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
    let replay = client
        .rpc(3, "session.attach", json!({"session_id":session,"since":0}))
        .await;
    let events = replay["result"]["events"].as_array().unwrap();
    let context_index = events
        .iter()
        .position(|event| event["method"] == "memory.context")
        .expect("the injected notes are audited on the turn");
    assert_eq!(events[context_index - 1]["method"], "turn.started");
    assert_eq!(
        events[context_index]["turn_id"],
        events[context_index - 1]["turn_id"]
    );
    assert_eq!(
        events[context_index]["params"]["notes"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
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
        learn: LearnMode::Off,
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
        learn: LearnMode::Off,
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
