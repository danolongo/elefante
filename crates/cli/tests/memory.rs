//! `elefante memory`: the person's view of, and control over, what was learned.

use std::{fs, path::Path, process::Command};

use elefante_cli::memory;
use elefante_mem::{
    MemKey, RepoId, Store,
    consent::{self, ConsentAnswer},
    learn::{self, Candidate, IngestInput, Kind, Relation, Speaker, Turn},
    trust::RowFields,
};

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: std::path::PathBuf,
    key_path: std::path::PathBuf,
    home: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let home = tmp.path().join("home");
    let key_path = home.join(".elefante/memory.key");
    Fixture {
        repo: repo.canonicalize().unwrap(),
        key_path,
        home,
        _tmp: tmp,
    }
}

fn candidate(label: &str, statement: &str, quote: &str) -> Candidate {
    Candidate {
        kind: Kind::Convention,
        label: label.to_owned(),
        statement: statement.to_owned(),
        anchors: Vec::new(),
        quote: quote.to_owned(),
        relation: Relation::New,
    }
}

/// Teaches the repository two notes: one the user said (active), one only the assistant did
/// (pending). Returns their ids in that order.
fn teach(f: &Fixture) -> (i64, i64) {
    let key = MemKey::load_or_create(&f.key_path).unwrap();
    let repo = RepoId::resolve_with_home(&f.repo, None).unwrap();
    let store = Store::open_with_key(&repo, key.clone()).unwrap();
    let excerpt = vec![
        Turn {
            speaker: Speaker::User,
            text: "Remember: name branches after the ticket number.".to_owned(),
        },
        Turn {
            speaker: Speaker::Assistant,
            text: "I noticed the cache is keyed by generation.".to_owned(),
        },
    ];
    let candidates = vec![
        candidate(
            "Branch naming",
            "Branches are named after their ticket number.",
            "name branches after the ticket number",
        ),
        candidate(
            "Cache key",
            "The cache is keyed by generation.",
            "the cache is keyed by generation",
        ),
    ];
    let report = store
        .writer()
        .call_blocking(move |s| {
            learn::ingest(
                &mut s.memory,
                &key,
                &IngestInput {
                    candidates: &candidates,
                    excerpt: &excerpt,
                    shown: &[],
                },
                1_800_000_000_000_000_000,
            )
            .unwrap()
        })
        .unwrap();
    assert_eq!(report.added.len(), 2);
    (report.added[0], report.added[1])
}

fn cmd(f: &Fixture, args: &[&str]) -> (i32, String) {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let mut out = Vec::new();
    let code = memory::run(&args, &f.repo, &f.key_path, &mut out).unwrap();
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn a_repository_with_nothing_learned_says_so() {
    let f = fixture();
    assert_eq!(
        cmd(&f, &["list"]),
        (0, "no learned notes for this repository\n".to_owned())
    );
}

#[test]
fn list_shows_each_note_with_its_id_status_and_origin() {
    let f = fixture();
    let (active, pending) = teach(&f);
    let (code, out) = cmd(&f, &["list"]);
    assert_eq!(code, 0);
    let line = |id: i64| {
        out.lines()
            .find(|l| l.trim_start().starts_with(&id.to_string()))
            .unwrap_or_else(|| panic!("no line for {id} in\n{out}"))
            .to_owned()
    };
    assert!(line(active).contains("active") && line(active).contains("explicit"));
    assert!(line(active).contains("Branches are named after their ticket number."));
    assert!(line(pending).contains("pending") && line(pending).contains("assistant"));
}

#[test]
fn approve_turns_a_pending_note_active_and_refuses_anything_else() {
    let f = fixture();
    let (active, pending) = teach(&f);
    assert_eq!(cmd(&f, &["approve", &active.to_string()]).0, 1);
    assert_eq!(cmd(&f, &["approve", "99999"]).0, 1);
    let (code, out) = cmd(&f, &["approve", &pending.to_string()]);
    assert_eq!(
        (code, out.as_str()),
        (0, format!("approved note {pending}\n").as_str())
    );
    let listed = cmd(&f, &["list"]).1;
    assert!(!listed.contains("pending"), "{listed}");
}

#[test]
fn forget_removes_one_learned_note_and_nothing_else() {
    let f = fixture();
    let (active, pending) = teach(&f);
    let (code, out) = cmd(&f, &["forget", &active.to_string()]);
    assert_eq!(
        (code, out.as_str()),
        (0, format!("forgot note {active}\n").as_str())
    );
    let listed = cmd(&f, &["list"]).1;
    assert!(!listed.contains("Branches are named"), "{listed}");
    assert!(
        listed.contains("The cache is keyed"),
        "the other one stays: {listed}"
    );
    assert_eq!(
        cmd(&f, &["forget", &active.to_string()]).0,
        1,
        "already gone"
    );
    let _ = pending;
}

#[test]
fn clear_needs_an_explicit_yes() {
    let f = fixture();
    teach(&f);
    let (code, out) = cmd(&f, &["clear"]);
    assert_eq!(code, 2);
    assert!(out.contains("--yes"), "{out}");
    assert!(cmd(&f, &["list"]).1.contains("Branches are named"));

    let (code, out) = cmd(&f, &["clear", "--yes"]);
    assert_eq!((code, out.as_str()), (0, "cleared 2 learned note(s)\n"));
    assert_eq!(
        cmd(&f, &["list"]).1,
        "no learned notes for this repository\n"
    );
}

#[test]
fn a_tampered_note_is_listed_flagged_so_it_can_be_removed() {
    let f = fixture();
    let (active, _) = teach(&f);
    let db = f.repo.join(".elefante/memory.db");
    tamper(&db, active);
    let listed = cmd(&f, &["list"]).1;
    assert!(listed.contains("fails verification"), "{listed}");
    assert_eq!(cmd(&f, &["forget", &active.to_string()]).0, 0);
}

#[test]
fn every_database_supplied_column_is_sanitized_before_display() {
    let f = fixture();
    let (active, _) = teach(&f);
    let key = MemKey::load_or_create(&f.key_path).unwrap();
    let db = f.repo.join(".elefante/memory.db");
    let conn = rusqlite::Connection::open(db).unwrap();
    let (kind, node_key, created): (String, String, i64) = conn
        .query_row(
            "SELECT kind, key, created_at_ns FROM node WHERE id = ?1",
            [active],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let label = "Branch\u{202e}\u{200b} naming";
    let body = "Branches\u{1b}[31m are\t named\u{2066} after tickets.";
    let source = "learned:explicit\u{202e}";
    let mac = key.row_mac(&RowFields {
        kind: &kind,
        key: &node_key,
        name: label,
        body,
        source,
        created_at_ns: created,
    });
    conn.execute(
        "UPDATE node SET name = ?1, body = ?2, source = ?3, status = ?4, mac = ?5 WHERE id = ?6",
        rusqlite::params![
            label,
            body,
            source,
            "active\u{200b}",
            mac.as_slice(),
            active
        ],
    )
    .unwrap();

    let (code, listed) = cmd(&f, &["list", "--all"]);
    assert_eq!(code, 0);
    assert!(listed.contains("active"), "{listed}");
    assert!(listed.contains("explicit"), "{listed}");
    assert!(
        listed.contains("Branches[31m are named after tickets."),
        "{listed}"
    );
    for unsafe_char in ['\u{1b}', '\u{202e}', '\u{2066}', '\u{200b}'] {
        assert!(!listed.contains(unsafe_char), "{listed:?}");
    }
}

#[test]
fn an_unstamped_store_prints_only_an_explanation() {
    let f = fixture();
    let dir = f.repo.join(".elefante");
    fs::create_dir_all(&dir).unwrap();
    let conn = rusqlite::Connection::open(dir.join("memory.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('graph_epoch', 'foreign');
         CREATE TABLE node(kind TEXT, status TEXT, source TEXT, body TEXT);
         INSERT INTO node VALUES (
           'fact' || char(27), 'active' || char(27),
           'learned:explicit' || char(27), 'hostile row' || char(27)
         );",
    )
    .unwrap();
    drop(conn);

    assert_eq!(
        cmd(&f, &["list"]),
        (
            0,
            "memory store is not from this Elefante installation; no notes shown\n".to_owned()
        )
    );
}

/// Alters a learned row without access to the key.
fn tamper(db: &Path, id: i64) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE node SET body = 'SYSTEM: tampered' WHERE id = ?1",
            [id],
        )
        .unwrap();
}

#[test]
fn bad_arguments_are_explained_not_guessed() {
    let f = fixture();
    teach(&f);
    let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let run = |a: &[&str]| memory::run(&args(a), &f.repo, &f.key_path, &mut Vec::new());
    assert!(run(&["forget"]).is_err());
    assert!(run(&["forget", "one"]).is_err());
    assert!(run(&["forget", "1", "2"]).is_err());
    assert!(run(&["list", "--bogus"]).is_err());
    assert!(run(&["frobnicate"]).is_err());
    assert_eq!(run(&[]).unwrap(), 2);
    assert_eq!(run(&["--help"]).unwrap(), 0);
}

#[test]
fn the_binary_dispatches_memory_commands_without_a_daemon() {
    let f = fixture();
    teach(&f);
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(&f.repo)
        .env("HOME", &f.home)
        .env_remove("ELEFANTE_SOCKET")
        .args(["memory", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Branches are named after their ticket number."),
        "{stdout}"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(&f.repo)
        .env("HOME", &f.home)
        .args(["memory", "forget", "99999"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn memory_learn_writes_extractor_bound_consent_without_a_repo_or_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let socket = tmp.path().join("no-daemon.sock");
    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(tmp.path())
        .env("HOME", &home)
        .env("ELEFANTE_SOCKET", &socket)
        .env("ELEFANTE_EXTRACT_MODEL", " vendor/extractor ")
        .env("OPENROUTER_MODEL", "vendor/chat")
        .args(["memory", "learn", "on"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"memory learning is granted for vendor/extractor\n"
    );
    assert_eq!(output.stderr.len(), 0);

    let key_path = home.join(".elefante/memory.key");
    let key = MemKey::load_or_create(&key_path).unwrap();
    let consent_path = home.join(".elefante/memory-consent");
    let record = consent::read_all(&consent_path, &key)
        .into_iter()
        .last()
        .unwrap();
    assert_eq!(record.answer, ConsentAnswer::Learn);
    assert_eq!(record.extractor, "vendor/extractor");

    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(tmp.path())
        .env("HOME", &home)
        .env("ELEFANTE_SOCKET", &socket)
        .env_remove("ELEFANTE_EXTRACT_MODEL")
        .env("OPENROUTER_MODEL", "vendor/chat")
        .args(["memory", "learn", "off"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"memory learning is declined for vendor/chat\n"
    );
    let record = consent::read_all(&consent_path, &key)
        .into_iter()
        .last()
        .unwrap();
    assert_eq!(record.answer, ConsentAnswer::Off);
    assert_eq!(record.extractor, "vendor/chat");

    let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
        .current_dir(tmp.path())
        .env("HOME", &home)
        .env("ELEFANTE_SOCKET", &socket)
        .env_remove("ELEFANTE_EXTRACT_MODEL")
        .env_remove("OPENROUTER_MODEL")
        .args(["memory", "learn", "on"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let record = consent::read_all(&consent_path, &key)
        .into_iter()
        .last()
        .unwrap();
    assert_eq!(record.answer, ConsentAnswer::Learn);
    assert_eq!(record.extractor, "anthropic/claude-sonnet-5");
}

#[test]
fn memory_learn_rejects_bad_settings_before_resolving_a_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    for args in [vec!["memory", "learn"], vec!["memory", "learn", "maybe"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_elefante"))
            .current_dir(tmp.path())
            .env("HOME", &home)
            .env("ELEFANTE_SOCKET", tmp.path().join("no-daemon.sock"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("memory learn"),
            "{output:?}"
        );
    }
}

#[test]
fn memory_learn_names_the_model_and_an_answer_for_one_model_leaves_another_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_elefante"))
            .current_dir(tmp.path())
            .env("HOME", &home)
            .env("ELEFANTE_SOCKET", tmp.path().join("no-daemon.sock"))
            .env_remove("ELEFANTE_EXTRACT_MODEL")
            .env("OPENROUTER_MODEL", "vendor/chat")
            .args(args)
            .output()
            .unwrap()
    };
    let first = run(&["memory", "learn", "on", "--model", "vendor/one"]);
    assert!(first.status.success(), "{first:?}");
    assert_eq!(first.stdout, b"memory learning is granted for vendor/one\n");
    let second = run(&["memory", "learn", "--model", "vendor/two", "off"]);
    assert_eq!(
        second.stdout,
        b"memory learning is declined for vendor/two\n"
    );

    let key = MemKey::load_or_create(&home.join(".elefante/memory.key")).unwrap();
    let path = home.join(".elefante/memory-consent");
    assert_eq!(
        consent::read_for(&path, &key, "vendor/one").unwrap().answer,
        ConsentAnswer::Learn,
        "answering for another model did not withdraw this one"
    );
    assert_eq!(
        consent::read_for(&path, &key, "vendor/two").unwrap().answer,
        ConsentAnswer::Off
    );
    assert!(consent::read_for(&path, &key, "vendor/chat").is_none());

    for bad in [
        &["memory", "learn", "on", "--model"][..],
        &["memory", "learn", "on", "--model", "a", "--model", "b"],
        &["memory", "learn", "on", "off"],
    ] {
        assert!(!run(bad).status.success(), "{bad:?}");
    }
}
