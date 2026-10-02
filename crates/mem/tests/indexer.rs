mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use common::GitRepo;
use elefante_mem::{
    Counters, IndexStats, MemError, RepoId, Store,
    graph::{self, GraphSnapshot},
    indexer::{Hooks, IndexOptions, RefreshReport, refresh_blocking},
    state,
};

const FILES: &[(&str, &str)] = &[
    (
        "src/lib.rs",
        "//! Core loop.\n\n/// Runs a turn.\npub fn run() {}\n\npub struct Session;\n",
    ),
    ("ui/app.tsx", "/** App. */\nexport function App() {}\n"),
    ("README.md", "# Elefante\n\n## Usage\n"),
    ("Makefile", "all:\n\ttrue\n"),
];

fn age(path: &Path, seconds: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(seconds))
        .unwrap();
}

struct Tree {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    store: Option<Store>,
}

impl Tree {
    fn new(files: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let tree = Self {
            root: RepoId::resolve_with_home(tmp.path(), None)
                .unwrap()
                .root()
                .to_path_buf(),
            _tmp: tmp,
            store: None,
        };
        for (path, contents) in files {
            tree.write(path, contents, 3600);
        }
        Self {
            store: Some(
                Store::open(&RepoId::resolve_with_home(&tree.root, None).unwrap()).unwrap(),
            ),
            ..tree
        }
    }

    /// Files are aged into the past so the racy-timestamp window does not apply unless a test
    /// is specifically about it.
    fn write(&self, path: &str, contents: &str, age_seconds: u64) {
        let target = self.root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents).unwrap();
        age(&target, age_seconds);
    }

    fn store(&self) -> &Store {
        self.store.as_ref().unwrap()
    }

    fn refresh(&self) -> (RefreshReport, IndexStats) {
        self.refresh_with(&IndexOptions::default()).unwrap()
    }

    fn refresh_with(
        &self,
        options: &IndexOptions,
    ) -> Result<(RefreshReport, IndexStats), MemError> {
        let counters = Counters::default();
        let report = refresh_blocking(self.store(), &self.root, &counters, options)?;
        Ok((report, counters.snapshot()))
    }

    fn snapshot(&self) -> GraphSnapshot {
        self.store()
            .writer()
            .call_blocking(|s| graph::snapshot(&s.memory).unwrap())
            .unwrap()
    }

    fn keys(&self) -> Vec<String> {
        self.snapshot().nodes.into_iter().map(|n| n.1).collect()
    }

    fn generation(&self) -> i64 {
        self.store()
            .writer()
            .call_blocking(|s| graph::generation(&s.memory).unwrap())
            .unwrap()
    }

    fn meta(&self, key: &'static str) -> Option<String> {
        self.store()
            .writer()
            .call_blocking(move |s| {
                s.memory
                    .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                    .ok()
            })
            .unwrap()
    }

    fn ids(&self) -> std::collections::HashMap<String, i64> {
        self.store()
            .writer()
            .call_blocking(|s| {
                s.memory
                    .prepare("SELECT key, id FROM node")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            })
            .unwrap()
    }

    /// Closes the store, as a process exit would.
    fn close(&mut self) -> elefante_mem::StoreDir {
        let dir = self.store.as_ref().unwrap().dir().clone();
        self.store = None;
        dir
    }

    fn reopen(&mut self) {
        self.store =
            Some(Store::open(&RepoId::resolve_with_home(&self.root, None).unwrap()).unwrap());
    }
}

fn node_keys(snapshot: &GraphSnapshot) -> Vec<(&str, &str)> {
    snapshot
        .nodes
        .iter()
        .map(|n| (n.0.as_str(), n.1.as_str()))
        .collect()
}

const EXPECTED_GRAPH: [(&str, &str); 7] = [
    ("file", "Makefile"),
    ("file", "README.md"),
    ("file", "src/lib.rs"),
    ("file", "ui/app.tsx"),
    ("symbol", "src/lib.rs::Session"),
    ("symbol", "src/lib.rs::run"),
    ("symbol", "ui/app.tsx::App"),
];

#[test]
fn the_first_refresh_builds_the_graph_and_counts_what_it_did() {
    let tree = Tree::new(FILES);
    let (report, stats) = tree.refresh();

    assert_eq!(node_keys(&tree.snapshot()), EXPECTED_GRAPH);
    assert_eq!(report.updated.len(), 4);
    assert!(report.complete && !report.truncated);
    assert_eq!(stats.hashed, 4);
    assert_eq!(
        stats.parsed, 2,
        "one Rust and one TSX parse; Markdown has no parser"
    );
    assert_eq!(stats.extraction_hits, 0);
    assert_eq!(stats.batches, 1);
    assert_eq!(report.generation, 1);
    assert_eq!(tree.meta("index_complete").as_deref(), Some("1"));
    assert_eq!(tree.meta("truncated").as_deref(), Some("0"));
}

#[test]
fn a_warm_start_parses_nothing_reads_nothing_and_does_not_bump_the_generation() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let snapshot = tree.snapshot();

    let (report, stats) = tree.refresh();
    assert_eq!(stats.parsed, 0, "gate: zero tree-sitter invocations");
    assert_eq!(stats.hashed, 0);
    assert_eq!(stats.batches, 0);
    assert_eq!(stats.stat_unchanged, 4);
    assert!(report.updated.is_empty() && report.removed.is_empty());
    assert_eq!(tree.generation(), 1);
    assert_eq!(tree.snapshot(), snapshot);
}

#[test]
fn changing_one_file_reparses_exactly_one_and_bumps_the_generation_once() {
    let tree = Tree::new(FILES);
    tree.refresh();

    tree.write(
        "src/lib.rs",
        "//! Core loop.\npub fn run() {}\npub fn extra() {}\n",
        1800,
    );
    let (report, stats) = tree.refresh();
    assert_eq!(report.updated, ["src/lib.rs"]);
    assert_eq!(stats.parsed, 1);
    assert_eq!(stats.hashed, 1);
    assert_eq!(stats.batches, 1);
    assert_eq!(tree.generation(), 2, "gate: one bump");
    let snapshot = tree.snapshot();
    assert!(
        node_keys(&snapshot).contains(&("symbol", "src/lib.rs::extra")),
        "the new symbol is in the graph"
    );
}

#[test]
fn an_edit_keeps_the_ids_of_everything_it_did_not_change() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let before = tree.ids();

    tree.write(
        "src/lib.rs",
        "//! Core loop.\n\n/// Runs a turn.\npub fn run(x: u32) {}\n\npub struct Session;\n",
        1800,
    );
    tree.refresh();
    let after = tree.ids();
    for key in [
        "src/lib.rs",
        "src/lib.rs::run",
        "src/lib.rs::Session",
        "ui/app.tsx",
        "README.md",
    ] {
        assert_eq!(
            after[key], before[key],
            "{key} must keep its id across an edit"
        );
    }
}

#[test]
fn touching_a_file_reads_it_but_writes_nothing() {
    let tree = Tree::new(FILES);
    tree.refresh();
    age(&tree.root.join("README.md"), 1000);

    let (report, stats) = tree.refresh();
    assert_eq!(stats.hashed, 1);
    assert_eq!(stats.parsed, 0);
    assert_eq!(stats.batches, 0, "no graph change, no commit counted");
    assert!(report.updated.is_empty());
    assert_eq!(tree.generation(), 1);
}

#[test]
fn files_are_committed_in_batches_each_bumping_the_generation() {
    let tree = Tree::new(FILES);
    let options = IndexOptions {
        batch_files: 2,
        ..IndexOptions::default()
    };
    let (_, stats) = tree.refresh_with(&options).unwrap();
    assert_eq!(stats.batches, 2);
    assert_eq!(tree.generation(), 2);
    assert_eq!(node_keys(&tree.snapshot()), EXPECTED_GRAPH);
}

#[test]
fn a_deleted_file_leaves_the_graph_and_the_cache_state() {
    let tree = Tree::new(FILES);
    tree.refresh();
    fs::remove_file(tree.root.join("ui/app.tsx")).unwrap();

    let (report, stats) = tree.refresh();
    assert_eq!(report.removed, ["ui/app.tsx"]);
    assert_eq!(stats.batches, 1);
    assert_eq!(tree.generation(), 2);
    let keys = tree.keys();
    assert!(!keys.iter().any(|k| k.starts_with("ui/app.tsx")));
    let remembered = tree
        .store()
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap())
        .unwrap();
    assert!(!remembered.contains_key("ui/app.tsx"));
}

#[test]
fn a_text_file_that_becomes_binary_loses_its_node_and_is_remembered_as_skipped() {
    let tree = Tree::new(FILES);
    tree.refresh();
    fs::write(tree.root.join("src/lib.rs"), [b'f', 0, 1, 2]).unwrap();
    age(&tree.root.join("src/lib.rs"), 900);

    let (report, _) = tree.refresh();
    assert_eq!(report.removed, ["src/lib.rs"]);
    let keys = tree.keys();
    assert!(
        !keys.iter().any(|k| k.starts_with("src/lib.rs")),
        "{keys:?}"
    );
    let (_, warm) = tree.refresh();
    assert_eq!(warm.hashed, 0, "a skipped binary is not re-read");
}

#[test]
fn files_without_an_extractor_are_path_only_nodes() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let snapshot = tree.snapshot();
    let makefile = snapshot.nodes.iter().find(|n| n.1 == "Makefile").unwrap();
    assert_eq!(makefile.3, "");
    let readme = snapshot.nodes.iter().find(|n| n.1 == "README.md").unwrap();
    assert_eq!(readme.3, "Elefante · Usage");
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_skipped_and_does_not_abort_the_refresh() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new(FILES);
    let locked = tree.root.join("Makefile");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read(&locked).is_ok(); // true when running as root
    let (_, stats) = tree.refresh();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    let keys = tree.keys();
    assert!(keys.iter().any(|k| k == "src/lib.rs"));
    if !readable {
        assert!(!keys.iter().any(|k| k == "Makefile"));
        assert_eq!(stats.hashed, 3);
    }
}

#[test]
fn deleting_the_cache_database_converges_on_an_identical_graph() {
    let mut tree = Tree::new(FILES);
    tree.refresh();
    let original = tree.snapshot();

    let dir = tree.close();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.path().join(format!("cache.db{suffix}")));
    }
    tree.reopen();
    let (_, stats) = tree.refresh();
    assert_eq!(
        stats.parsed, 2,
        "with the cache gone, everything is parsed again"
    );
    assert_eq!(
        tree.snapshot(),
        original,
        "the same graph, not merely a graph"
    );
}

#[test]
fn deleting_the_cache_while_a_file_vanishes_still_removes_that_file() {
    let mut tree = Tree::new(FILES);
    tree.refresh();
    let dir = tree.close();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.path().join(format!("cache.db{suffix}")));
    }
    fs::remove_file(tree.root.join("ui/app.tsx")).unwrap();

    tree.reopen();
    // With `file_state` empty no deletion is ever *noticed*; only the orphan sweep can find it.
    tree.refresh();
    let keys = tree.keys();
    assert!(
        !keys.iter().any(|k| k.starts_with("ui/app.tsx")),
        "{keys:?}"
    );
    assert!(keys.iter().any(|k| k == "src/lib.rs"));
}

#[test]
fn deleting_the_graph_rebuilds_an_identical_one_without_parsing() {
    let mut tree = Tree::new(FILES);
    tree.refresh();
    let original = tree.snapshot();

    let dir = tree.close();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.path().join(format!("memory.db{suffix}")));
    }
    tree.reopen();
    let (_, stats) = tree.refresh();
    assert_eq!(
        stats.parsed, 0,
        "gate: extractions are keyed by content and survive"
    );
    assert_eq!(
        stats.extraction_hits, 3,
        "Rust, TSX and Markdown come from the cache"
    );
    assert_eq!(
        stats.hashed, 4,
        "the graph was empty, so every file is re-examined"
    );
    assert_eq!(tree.snapshot(), original);
}

struct CrashAfterFirstGraphCommit(AtomicBool);

impl Hooks for CrashAfterFirstGraphCommit {
    fn after_graph_commit(&self) -> Result<(), MemError> {
        if self.0.swap(false, Ordering::SeqCst) {
            Err(MemError::Aborted(
                "injected between the two commits".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_crash_between_the_graph_commit_and_the_cache_write_converges_on_the_next_run() {
    let mut tree = Tree::new(FILES);
    let options = IndexOptions {
        batch_files: 2,
        hooks: Arc::new(CrashAfterFirstGraphCommit(AtomicBool::new(true))),
    };
    let crashed = tree.refresh_with(&options);
    assert!(matches!(crashed, Err(MemError::Aborted(_))));

    // The window, observed: the graph has the first batch, the cache remembers none of it.
    let graph_nodes = tree.snapshot().nodes.len();
    let remembered = tree
        .store()
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap().len())
        .unwrap();
    assert!(graph_nodes > 0, "the graph commit happened");
    assert_eq!(remembered, 0, "the cache write did not");

    // A restart, then an ordinary refresh.
    tree.close();
    tree.reopen();
    tree.refresh();

    let clean = Tree::new(FILES);
    clean.refresh();
    assert_eq!(
        tree.snapshot(),
        clean.snapshot(),
        "no duplicate and no missing rows"
    );
}

#[test]
fn secrets_never_reach_either_database_or_any_byte_under_the_store_directory() {
    let tree = Tree::new(&[
        (
            ".env",
            "OPENROUTER_API_KEY=sk-ant-api03-ENVFILEKEY12345678\n",
        ),
        (
            "certs/server.pem",
            "-----BEGIN PRIVATE KEY-----\nsk-ant-api03-PEMKEY12345678\n",
        ),
        (
            "src/secret.rs",
            "/// Rotate sk-ant-api03-DOCCOMMENTKEY1234 monthly.\n\
             pub const KEY: &str = \"sk-ant-api03-CONSTKEY1234567\";\n\
             pub fn connect(token: &str) -> &'static str { \"sk-ant-api03-BODYKEY12345678\" }\n",
        ),
        ("docs/NOTES.md", "# Rotating sk-ant-api03-HEADINGKEY12345\n"),
        (
            "ui/config.ts",
            "export const API = \"sk-ant-api03-TSCONSTKEY12345\";\n\
             export function f(key = \"sk-ant-api03-TSDEFAULTKEY1234\") {}\n",
        ),
    ]);
    tree.refresh();

    // Not vacuous: the files were indexed, and what was safe to keep is there.
    let snapshot = tree.snapshot();
    let keys: Vec<&str> = node_keys(&snapshot).into_iter().map(|k| k.1).collect();
    assert!(keys.contains(&"src/secret.rs::KEY"), "{keys:?}");
    assert!(keys.contains(&"ui/config.ts::f"));
    assert!(
        snapshot.nodes.iter().any(|n| n.3.contains("[REDACTED]")),
        "the doc comment is kept, redacted"
    );
    assert!(!keys.contains(&".env") && !keys.contains(&"certs/server.pem"));

    // The strongest check: raw bytes of every file, WAL and shared-memory sidecars included.
    let dir = tree.store().dir().path().to_path_buf();
    let mut scanned = Vec::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = fs::read(&path).unwrap();
        for needle in [
            "sk-ant",
            "ENVFILEKEY",
            "PEMKEY",
            "DOCCOMMENTKEY",
            "CONSTKEY",
            "BODYKEY",
            "HEADINGKEY",
            "TSCONSTKEY",
            "TSDEFAULTKEY",
        ] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "{needle} found in {}",
                path.display()
            );
        }
        scanned.push(path.file_name().unwrap().to_string_lossy().into_owned());
    }
    assert!(scanned.iter().any(|n| n == "memory.db-wal"), "{scanned:?}");
    assert!(scanned.iter().any(|n| n == "cache.db"), "{scanned:?}");
}

#[test]
fn git_status_is_clean_after_a_full_refresh() {
    let mut git = GitRepo::init();
    git.write("src/lib.rs", "pub fn run() {}\n");
    git.write("README.md", "# Title\n");
    git.commit("c1");
    let store = Store::open(&RepoId::resolve_with_home(git.path(), None).unwrap()).unwrap();
    let root = RepoId::resolve_with_home(git.path(), None)
        .unwrap()
        .root()
        .to_path_buf();
    refresh_blocking(
        &store,
        &root,
        &Counters::default(),
        &IndexOptions::default(),
    )
    .unwrap();

    let status = git.git(&["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        status.is_empty(),
        "git status must be clean: {}",
        String::from_utf8_lossy(&status)
    );
    // The index really ran: this is not a clean status because nothing happened.
    let nodes: i64 = store
        .writer()
        .call_blocking(|s| {
            s.memory
                .query_row("SELECT count(*) FROM node", [], |r| r.get(0))
                .unwrap()
        })
        .unwrap();
    assert!(nodes >= 3);
}
