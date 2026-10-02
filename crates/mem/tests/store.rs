mod common;

use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use common::GitRepo;
use elefante_mem::{
    MemError, Pairing, RepoId, Store, StoreDir,
    schema::{self, open_memory_reader},
    terms::identifier_terms,
};
use rusqlite::Connection;

fn repo_for(dir: &Path) -> RepoId {
    RepoId::resolve_with_home(dir, None).unwrap()
}

/// A plain directory marked as a repository, so no git binary is involved.
fn fresh() -> (tempfile::TempDir, RepoId) {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let repo = repo_for(tmp.path());
    (tmp, repo)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn scalar(store: &Store, sql: &'static str) -> i64 {
    store
        .writer()
        .call_blocking(move |s| s.memory.query_row(sql, [], |r| r.get(0)).unwrap())
        .unwrap()
}

fn cache_scalar(store: &Store, sql: &'static str) -> i64 {
    store
        .writer()
        .call_blocking(move |s| s.cache.query_row(sql, [], |r| r.get(0)).unwrap())
        .unwrap()
}

fn meta(store: &Store, key: &'static str) -> String {
    store
        .writer()
        .call_blocking(move |s| {
            s.memory
                .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .unwrap()
        })
        .unwrap()
}

#[test]
fn open_creates_both_databases_with_the_documented_schema() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();

    for table in ["meta", "node", "edge", "import_ref", "node_fts"] {
        assert_eq!(
            scalar(
                &store,
                match table {
                    "meta" => "SELECT count(*) FROM sqlite_master WHERE name = 'meta'",
                    "node" => "SELECT count(*) FROM sqlite_master WHERE name = 'node'",
                    "edge" => "SELECT count(*) FROM sqlite_master WHERE name = 'edge'",
                    "import_ref" => "SELECT count(*) FROM sqlite_master WHERE name = 'import_ref'",
                    _ => "SELECT count(*) FROM sqlite_master WHERE name = 'node_fts'",
                }
            ),
            1,
            "memory.db is missing {table}"
        );
    }
    for table in ["cache_meta", "file_state", "extraction", "brief"] {
        let count: i64 = store
            .writer()
            .call_blocking(move |s| {
                s.cache
                    .query_row(
                        "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                        [table],
                        |r| r.get(0),
                    )
                    .unwrap()
            })
            .unwrap();
        assert_eq!(count, 1, "cache.db is missing {table}");
    }
    assert_eq!(
        scalar(&store, "PRAGMA user_version"),
        schema::MEMORY_SCHEMA_VERSION
    );
    assert_eq!(
        cache_scalar(&store, "PRAGMA user_version"),
        schema::CACHE_SCHEMA_VERSION
    );
    assert_eq!(meta(&store, "repo_generation"), "0");
    assert_eq!(meta(&store, "index_complete"), "0");
    assert_eq!(meta(&store, "graph_epoch").len(), 32);
    assert_eq!(scalar(&store, "PRAGMA foreign_keys"), 1);
    assert_eq!(store.pairing(), Pairing::Reset);
}

#[test]
fn both_databases_run_in_wal_mode() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    for path in [store.dir().memory_db(), store.dir().cache_db()] {
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "{}", path.display());
    }
}

#[test]
fn the_ignore_file_exists_before_any_database() {
    let (_tmp, repo) = fresh();
    let dir = StoreDir::create(&repo).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
        "*\n"
    );
    assert!(
        !dir.memory_db().exists(),
        "no database may precede the ignore file"
    );
    assert!(!dir.cache_db().exists());
}

#[test]
fn git_status_stays_clean_with_the_store_open_and_written() {
    let mut git = GitRepo::init();
    git.write("src/a.rs", "fn a() {}\n");
    git.commit("c1");
    let repo = repo_for(git.path());
    let store = Store::open(&repo).unwrap();
    store
        .writer()
        .call_blocking(|s| {
            s.memory
                .execute(
                    "INSERT INTO node(kind, key, name, created_at_ns, updated_at_ns)
                     VALUES ('file', 'src/a.rs', 'a.rs', 1, 1)",
                    [],
                )
                .unwrap();
        })
        .unwrap();
    assert!(store.dir().path().join("memory.db-wal").exists());
    let status = git.git(&["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        status.is_empty(),
        "git status must be clean: {}",
        String::from_utf8_lossy(&status)
    );
}

#[test]
fn directory_is_private_and_every_database_file_is_owner_only() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    // Force WAL traffic so the sidecar files exist, then check each by name.
    store
        .writer()
        .call_blocking(|s| {
            s.memory
                .execute(
                    "UPDATE meta SET value = '0' WHERE key = 'repo_generation'",
                    [],
                )
                .unwrap();
            s.cache
                .execute(
                    "INSERT INTO cache_meta(key, value) VALUES ('probe', '1')",
                    [],
                )
                .unwrap();
        })
        .unwrap();
    let dir = store.dir().path();
    assert_eq!(mode(dir), 0o700);
    for name in [
        "memory.db",
        "memory.db-wal",
        "memory.db-shm",
        "cache.db",
        "cache.db-wal",
        "cache.db-shm",
    ] {
        let path = dir.join(name);
        assert!(path.exists(), "{name} should exist while the store is open");
        assert_eq!(mode(&path), 0o600, "{name}");
    }
}

#[test]
fn a_looser_preexisting_directory_is_tightened() {
    let (tmp, repo) = fresh();
    let dir = tmp.path().join(".elefante");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    StoreDir::create(&repo).unwrap();
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn reopening_preserves_the_epoch_and_the_pairing() {
    let (_tmp, repo) = fresh();
    let epoch = {
        let store = Store::open(&repo).unwrap();
        meta(&store, "graph_epoch")
    };
    let store = Store::open(&repo).unwrap();
    assert_eq!(meta(&store, "graph_epoch"), epoch);
    assert_eq!(store.pairing(), Pairing::Matched);
}

#[test]
fn a_deleted_graph_clears_file_state_but_keeps_extractions() {
    let (_tmp, repo) = fresh();
    let (dir, old_epoch) = {
        let store = Store::open(&repo).unwrap();
        store
            .writer()
            .call_blocking(|s| {
                s.cache
                    .execute_batch(
                        "INSERT INTO file_state VALUES ('a.rs', 1, 1, x'00', 1);
                         INSERT INTO extraction VALUES (x'aa', 'rust@1', '{}', 1);",
                    )
                    .unwrap();
            })
            .unwrap();
        (store.dir().clone(), meta(&store, "graph_epoch"))
    };
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.path().join(format!("memory.db{suffix}")));
    }
    let store = Store::open(&repo).unwrap();
    assert_eq!(store.pairing(), Pairing::Reset);
    assert_ne!(meta(&store, "graph_epoch"), old_epoch);
    assert_eq!(cache_scalar(&store, "SELECT count(*) FROM file_state"), 0);
    assert_eq!(
        cache_scalar(&store, "SELECT count(*) FROM extraction"),
        1,
        "extractions are keyed by content and must survive a graph reset"
    );
}

#[test]
fn a_matched_reopen_keeps_file_state() {
    let (_tmp, repo) = fresh();
    {
        let store = Store::open(&repo).unwrap();
        store
            .writer()
            .call_blocking(|s| {
                s.cache
                    .execute("INSERT INTO file_state VALUES ('a.rs', 1, 1, x'00', 1)", [])
                    .unwrap();
            })
            .unwrap();
    }
    let store = Store::open(&repo).unwrap();
    assert_eq!(cache_scalar(&store, "SELECT count(*) FROM file_state"), 1);
}

#[test]
fn a_cache_at_another_schema_version_is_recreated_not_migrated() {
    let (_tmp, repo) = fresh();
    let dir = {
        let store = Store::open(&repo).unwrap();
        store
            .writer()
            .call_blocking(|s| {
                s.cache
                    .execute(
                        "INSERT INTO extraction VALUES (x'aa', 'rust@1', '{}', 1)",
                        [],
                    )
                    .unwrap();
            })
            .unwrap();
        store.dir().clone()
    };
    Connection::open(dir.cache_db())
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let store = Store::open(&repo).unwrap();
    assert_eq!(cache_scalar(&store, "SELECT count(*) FROM extraction"), 0);
    assert_eq!(
        cache_scalar(&store, "PRAGMA user_version"),
        schema::CACHE_SCHEMA_VERSION
    );
}

#[test]
fn a_graph_from_a_newer_build_is_refused_and_left_untouched() {
    let (_tmp, repo) = fresh();
    let dir = {
        let store = Store::open(&repo).unwrap();
        store.dir().clone()
    };
    Connection::open(dir.memory_db())
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    let before = fs::read(dir.memory_db()).unwrap();
    assert!(matches!(
        Store::open(&repo),
        Err(MemError::UnsupportedSchema { found: 99, .. })
    ));
    assert_eq!(fs::read(dir.memory_db()).unwrap(), before);
}

#[test]
fn a_graph_with_tables_but_no_version_is_corrupt_not_recreated() {
    let (_tmp, repo) = fresh();
    let dir = StoreDir::create(&repo).unwrap();
    Connection::open(dir.memory_db())
        .unwrap()
        .execute_batch("CREATE TABLE meta(key TEXT, value TEXT);")
        .unwrap();
    assert!(matches!(Store::open(&repo), Err(MemError::Corrupt { .. })));
}

#[test]
fn co_change_edges_must_be_stored_in_canonical_order() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    store
        .writer()
        .call_blocking(|s| {
            for (key, name) in [("a.rs", "a.rs"), ("b.rs", "b.rs")] {
                s.memory
                    .execute(
                        "INSERT INTO node(kind, key, name, created_at_ns, updated_at_ns)
                         VALUES ('file', ?1, ?2, 1, 1)",
                        [key, name],
                    )
                    .unwrap();
            }
            let insert = |src: i64, dst: i64, rel: &str| {
                s.memory.execute(
                    "INSERT INTO edge(src, dst, rel, created_at_ns) VALUES (?1, ?2, ?3, 1)",
                    rusqlite::params![src, dst, rel],
                )
            };
            assert!(insert(2, 1, "co_edited_with").is_err());
            assert!(insert(1, 2, "co_edited_with").is_ok());
            assert!(
                insert(2, 1, "depends_on").is_ok(),
                "other relations are directed"
            );
        })
        .unwrap();
}

#[test]
fn deleting_a_file_cascades_to_its_symbols_imports_and_edges() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    store
        .writer()
        .call_blocking(|s| {
            let conn = &s.memory;
            conn.execute_batch(
                "INSERT INTO node(id, kind, key, name, created_at_ns, updated_at_ns)
                   VALUES (1, 'file', 'a.rs', 'a.rs', 1, 1), (2, 'file', 'b.rs', 'b.rs', 1, 1);
                 INSERT INTO node(id, kind, key, name, file_id, created_at_ns, updated_at_ns)
                   VALUES (3, 'symbol', 'a.rs::f', 'f', 1, 1, 1);
                 INSERT INTO import_ref VALUES (1, 'use', 'crate::x');
                 INSERT INTO edge VALUES (1, 2, 'depends_on', 1.0, 1);
                 INSERT INTO edge VALUES (2, 1, 'depends_on', 1.0, 1);",
            )
            .unwrap();
            conn.execute("DELETE FROM node WHERE id = 1", []).unwrap();
            let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
            assert_eq!(count("SELECT count(*) FROM node"), 1);
            assert_eq!(count("SELECT count(*) FROM import_ref"), 0);
            assert_eq!(count("SELECT count(*) FROM edge"), 0);
        })
        .unwrap();
}

#[test]
fn a_node_key_is_unique_per_kind() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    store
        .writer()
        .call_blocking(|s| {
            let insert = |kind: &str| {
                s.memory.execute(
                    "INSERT INTO node(kind, key, name, created_at_ns, updated_at_ns)
                     VALUES (?1, 'x', 'x', 1, 1)",
                    [kind],
                )
            };
            assert!(insert("file").is_ok());
            assert!(insert("file").is_err());
            assert!(insert("symbol").is_ok());
        })
        .unwrap();
}

#[test]
fn split_terms_make_camel_and_snake_identifiers_searchable_by_their_words() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    store
        .writer()
        .call_blocking(|s| {
            for (rowid, name) in [(1, "TurnLock"), (2, "turn_lock"), (3, "unrelated")] {
                s.memory
                    .execute(
                        "INSERT INTO node_fts(rowid, name_terms, path_terms, body_terms)
                         VALUES (?1, ?2, '', '')",
                        rusqlite::params![rowid, identifier_terms(name)],
                    )
                    .unwrap();
            }
            let hits = |query: &str| -> Vec<i64> {
                s.memory
                    .prepare("SELECT rowid FROM node_fts WHERE node_fts MATCH ?1 ORDER BY rowid")
                    .unwrap()
                    .query_map([query], |r| r.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            };
            assert_eq!(hits("\"turn\""), vec![1, 2]);
            assert_eq!(hits("\"lock\""), vec![1, 2]);
            assert_eq!(hits("\"turnlock\""), vec![1, 2]);
        })
        .unwrap();
}

#[test]
fn the_reader_sees_committed_writes_and_cannot_write() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    store
        .writer()
        .call_blocking(|s| {
            s.memory
                .execute(
                    "UPDATE meta SET value = '7' WHERE key = 'repo_generation'",
                    [],
                )
                .unwrap();
        })
        .unwrap();
    let generation: String = store
        .reader()
        .call_blocking(|s| {
            s.memory
                .query_row(
                    "SELECT value FROM meta WHERE key = 'repo_generation'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        })
        .unwrap();
    assert_eq!(generation, "7");
    let write = store
        .reader()
        .call_blocking(|s| s.memory.execute("UPDATE meta SET value = '8'", []).is_err())
        .unwrap();
    assert!(write, "the reader connection must be read-only");
}

#[test]
fn a_reader_is_not_blocked_by_an_open_write_transaction() {
    let (_tmp, repo) = fresh();
    let store = Store::open(&repo).unwrap();
    let path = store.dir().memory_db();
    let (before, after) = store
        .writer()
        .call_blocking(move |s| {
            s.memory
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     INSERT INTO node(kind, key, name, created_at_ns, updated_at_ns)
                       VALUES ('file', 'pending.rs', 'pending.rs', 1, 1);",
                )
                .unwrap();
            // With the writer mid-transaction, a second connection must read the last committed
            // state immediately. Without WAL this would sit in busy_timeout and then fail.
            let reader = open_memory_reader(&path).unwrap();
            let count = |c: &Connection| -> i64 {
                c.query_row("SELECT count(*) FROM node", [], |r| r.get(0))
                    .unwrap()
            };
            let before = count(&reader);
            s.memory.execute_batch("COMMIT").unwrap();
            (before, count(&reader))
        })
        .unwrap();
    assert_eq!((before, after), (0, 1));
}
