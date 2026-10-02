//! Walk, diff, hash and `file_state` composed — the warm-start behaviour, before the indexer
//! exists to own it. Files are aged into the past so the racy-mtime window does not apply unless
//! a test is specifically about it.

use std::{
    collections::HashMap,
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use elefante_mem::{
    Counters, RepoId, Store,
    diff::{RACY_WINDOW_NS, diff},
    hash::{BINARY_SNIFF_BYTES, hash_files, read_and_hash},
    state::{self, StateRow},
    walk::{Walk, WalkedFile, walk, walk_limited},
};

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

fn set_mtime(path: &Path, age: Duration) {
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    store: Store,
}

fn fixture(files: &[(&str, &str)]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    for (path, contents) in files {
        let target = tmp.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents).unwrap();
        set_mtime(&target, Duration::from_secs(3600));
    }
    let repo = RepoId::resolve_with_home(tmp.path(), None).unwrap();
    let root = repo.root().to_path_buf();
    let store = Store::open(&repo).unwrap();
    Fixture {
        _tmp: tmp,
        root,
        store,
    }
}

/// One refresh of the walk/diff/hash/state layer. Returns the paths whose content changed.
fn refresh(f: &Fixture, counters: &Counters) -> (Vec<String>, Vec<String>) {
    let walked = walk(&f.root, counters).unwrap();
    refresh_with(f, &walked, counters)
}

fn refresh_with(f: &Fixture, walked: &Walk, counters: &Counters) -> (Vec<String>, Vec<String>) {
    let prior = f
        .store
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap())
        .unwrap();
    let plan = diff(walked, &prior, counters);
    let hashed = hash_files(&f.root, &plan.to_hash, counters);
    let mut changed = Vec::new();
    let mut rows = Vec::new();
    for (file, result) in plan.to_hash.iter().zip(hashed) {
        let hashed = result.unwrap();
        if prior.get(&file.path).map(|r| r.blake3) != Some(hashed.blake3) && !hashed.binary {
            changed.push(file.path.clone());
        }
        rows.push(StateRow {
            path: file.path.clone(),
            size: file.size,
            mtime_ns: file.mtime_ns,
            blake3: hashed.blake3,
            indexed_at_ns: now_ns(),
            skip: hashed.binary,
        });
    }
    let deleted = plan.deleted.clone();
    f.store
        .writer()
        .call_blocking(move |s| {
            state::upsert(&s.cache, &rows).unwrap();
            state::delete(&s.cache, &plan.deleted).unwrap();
        })
        .unwrap();
    (changed, deleted)
}

#[test]
fn a_warm_start_reads_nothing() {
    let f = fixture(&[
        ("a.rs", "fn a() {}"),
        ("b.rs", "fn b() {}"),
        ("c.md", "# c"),
    ]);
    let first = Counters::default();
    let (changed, _) = refresh(&f, &first);
    assert_eq!(changed.len(), 3);
    assert_eq!(first.snapshot().hashed, 3);

    let second = Counters::default();
    let (changed, deleted) = refresh(&f, &second);
    assert!(changed.is_empty() && deleted.is_empty());
    let stats = second.snapshot();
    assert_eq!(stats.hashed, 0, "an unchanged tree must not be read");
    assert_eq!(stats.stat_unchanged, 3);
}

#[test]
fn editing_one_file_reads_exactly_one() {
    let f = fixture(&[
        ("a.rs", "fn a() {}"),
        ("b.rs", "fn b() {}"),
        ("c.rs", "fn c() {}"),
    ]);
    refresh(&f, &Counters::default());

    let path = f.root.join("b.rs");
    fs::write(&path, "fn b() { 1 }").unwrap();
    set_mtime(&path, Duration::from_secs(1800));

    let counters = Counters::default();
    let (changed, _) = refresh(&f, &counters);
    assert_eq!(changed, ["b.rs"]);
    assert_eq!(counters.snapshot().hashed, 1);
    assert_eq!(counters.snapshot().stat_unchanged, 2);
}

#[test]
fn touching_a_file_reads_it_but_reports_no_change() {
    let f = fixture(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
    refresh(&f, &Counters::default());
    set_mtime(&f.root.join("a.rs"), Duration::from_secs(1800));

    let counters = Counters::default();
    let (changed, _) = refresh(&f, &counters);
    assert!(
        changed.is_empty(),
        "identical content is not a change: nothing downstream may run"
    );
    assert_eq!(counters.snapshot().hashed, 1);

    // The refreshed stat is remembered, so the touch costs one read, once.
    let again = Counters::default();
    refresh(&f, &again);
    assert_eq!(again.snapshot().hashed, 0);
}

#[test]
fn a_same_size_edit_inside_the_racy_window_is_still_caught() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    // Written "now": its mtime is inside the window of the hash taken immediately after.
    fs::write(tmp.path().join("a.rs"), "fn a() { 1 }").unwrap();
    let repo = RepoId::resolve_with_home(tmp.path(), None).unwrap();
    let f = Fixture {
        root: repo.root().to_path_buf(),
        store: Store::open(&repo).unwrap(),
        _tmp: tmp,
    };
    refresh(&f, &Counters::default());

    // Same length, and the mtime put back exactly: stat cannot tell the two versions apart.
    let path = f.root.join("a.rs");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, "fn a() { 2 }").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    let (changed, _) = refresh(&f, &Counters::default());
    assert_eq!(
        changed,
        ["a.rs"],
        "an entry hashed within the racy window must be re-read"
    );

    // Once the file has been quiet for longer than the window, stat is trusted again.
    set_mtime(&path, Duration::from_secs(3600));
    refresh(&f, &Counters::default());
    let settled = Counters::default();
    refresh(&f, &settled);
    assert_eq!(settled.snapshot().hashed, 0);
}

#[test]
fn a_removed_file_is_reported_and_forgotten() {
    let f = fixture(&[("a.rs", ""), ("b.rs", "")]);
    refresh(&f, &Counters::default());
    fs::remove_file(f.root.join("b.rs")).unwrap();

    let (_, deleted) = refresh(&f, &Counters::default());
    assert_eq!(deleted, ["b.rs"]);
    let remaining = f
        .store
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap().len())
        .unwrap();
    assert_eq!(remaining, 1);
}

#[test]
fn an_incomplete_walk_never_reports_deletions() {
    let f = fixture(&[("a.rs", ""), ("b.rs", ""), ("c.rs", ""), ("d.rs", "")]);
    refresh(&f, &Counters::default());

    // Only two of the four files are examined; the other two merely went unseen.
    let partial = walk_limited(&f.root, 2, &Counters::default()).unwrap();
    assert!(!partial.complete);
    let prior = f
        .store
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap())
        .unwrap();
    assert!(
        diff(&partial, &prior, &Counters::default())
            .deleted
            .is_empty()
    );
}

#[test]
fn a_binary_is_hashed_once_and_then_skipped_on_stat() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    let image = tmp.path().join("logo.png");
    fs::write(&image, [0x89, b'P', b'N', b'G', 0, 1, 2, 3]).unwrap();
    set_mtime(&image, Duration::from_secs(3600));
    let text = tmp.path().join("a.rs");
    fs::write(&text, "fn a() {}").unwrap();
    set_mtime(&text, Duration::from_secs(3600));
    let repo = RepoId::resolve_with_home(tmp.path(), None).unwrap();
    let f = Fixture {
        root: repo.root().to_path_buf(),
        store: Store::open(&repo).unwrap(),
        _tmp: tmp,
    };

    let (changed, _) = refresh(&f, &Counters::default());
    assert_eq!(changed, ["a.rs"], "a binary is never reported as indexable");
    let rows = f
        .store
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap())
        .unwrap();
    assert!(rows["logo.png"].skip);
    assert!(!rows["a.rs"].skip);

    let warm = Counters::default();
    refresh(&f, &warm);
    assert_eq!(
        warm.snapshot().hashed,
        0,
        "a remembered binary must not be re-read on every start"
    );
}

fn file(path: &str, size: u64, mtime_ns: i64) -> WalkedFile {
    WalkedFile {
        path: path.to_owned(),
        size,
        mtime_ns,
    }
}

fn row(path: &str, size: u64, mtime_ns: i64, indexed_at_ns: i64) -> StateRow {
    StateRow {
        path: path.to_owned(),
        size,
        mtime_ns,
        blake3: [7; 32],
        indexed_at_ns,
        skip: false,
    }
}

fn walk_of(files: Vec<WalkedFile>) -> Walk {
    Walk {
        files,
        truncated: false,
        complete: true,
    }
}

fn to_hash(files: Vec<WalkedFile>, rows: Vec<StateRow>) -> Vec<String> {
    let state: HashMap<_, _> = rows.into_iter().map(|r| (r.path.clone(), r)).collect();
    diff(&walk_of(files), &state, &Counters::default())
        .to_hash
        .into_iter()
        .map(|f| f.path)
        .collect()
}

#[test]
fn diff_classifies_new_changed_and_unchanged() {
    let long_ago = 1_000;
    let indexed = long_ago + RACY_WINDOW_NS * 10;
    let result = to_hash(
        vec![
            file("same", 10, long_ago),
            file("grew", 11, long_ago),
            file("retimed", 10, long_ago + 1),
            file("new", 1, long_ago),
        ],
        vec![
            row("same", 10, long_ago, indexed),
            row("grew", 10, long_ago, indexed),
            row("retimed", 10, long_ago, indexed),
        ],
    );
    assert_eq!(result, ["grew", "retimed", "new"]);
}

#[test]
fn the_racy_window_boundary_is_exact() {
    let mtime = 5_000_000_000;
    // Hashed `RACY_WINDOW_NS` after the mtime: not yet outside the window, so re-read.
    let at_edge = to_hash(
        vec![file("f", 1, mtime)],
        vec![row("f", 1, mtime, mtime + RACY_WINDOW_NS)],
    );
    assert_eq!(at_edge, ["f"]);
    // One nanosecond further: trusted on stat.
    let past_edge = to_hash(
        vec![file("f", 1, mtime)],
        vec![row("f", 1, mtime, mtime + RACY_WINDOW_NS + 1)],
    );
    assert!(past_edge.is_empty());
    // An mtime in the future of its own hash (clock skew) is always suspect.
    let skewed = to_hash(
        vec![file("f", 1, mtime)],
        vec![row("f", 1, mtime, mtime - 1)],
    );
    assert_eq!(skewed, ["f"]);
}

#[test]
fn a_remembered_skip_is_trusted_on_stat_like_any_other_row() {
    let mtime = 1_000;
    let mut binary = row("logo.png", 8, mtime, mtime + RACY_WINDOW_NS * 5);
    binary.skip = true;
    assert!(to_hash(vec![file("logo.png", 8, mtime)], vec![binary]).is_empty());
}

#[test]
fn file_state_round_trips_including_the_skip_flag() {
    let f = fixture(&[]);
    let mut binary = row("b.png", 3, -5, 99);
    binary.skip = true;
    let rows = vec![row("a.rs", 12, 7, 100), binary];
    let expected = rows.clone();
    f.store
        .writer()
        .call_blocking(move |s| state::upsert(&s.cache, &rows).unwrap())
        .unwrap();
    let loaded = f
        .store
        .writer()
        .call_blocking(|s| state::load(&s.cache).unwrap())
        .unwrap();
    assert_eq!(loaded.len(), 2);
    for row in expected {
        assert_eq!(loaded[&row.path], row);
    }
}

#[test]
fn hashing_matches_the_reference_and_detects_binaries_at_the_sniff_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let write = |name: &str, bytes: &[u8]| {
        let path = tmp.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    };

    let text = write("t.txt", b"hello");
    let hashed = read_and_hash(&text).unwrap();
    assert_eq!(hashed.blake3, *blake3::hash(b"hello").as_bytes());
    assert!(!hashed.binary);
    assert_eq!(hashed.bytes, b"hello");

    let mut inside = vec![b'a'; BINARY_SNIFF_BYTES];
    inside[BINARY_SNIFF_BYTES - 1] = 0;
    let inside = read_and_hash(&write("in.bin", &inside)).unwrap();
    assert!(inside.binary && inside.bytes.is_empty());

    let mut outside = vec![b'a'; BINARY_SNIFF_BYTES + 1];
    outside[BINARY_SNIFF_BYTES] = 0;
    let outside = read_and_hash(&write("out.txt", &outside)).unwrap();
    assert!(
        !outside.binary,
        "a NUL past the sniff window does not make a file binary"
    );

    assert!(read_and_hash(&tmp.path().join("missing")).is_err());
}

#[test]
fn hash_files_preserves_order_and_isolates_failures() {
    let tmp = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for i in 0..40 {
        let path = format!("f{i:02}.txt");
        fs::write(tmp.path().join(&path), format!("content {i}")).unwrap();
        files.push(file(&path, 0, 0));
    }
    files.insert(7, file("gone.txt", 0, 0));

    let counters = Counters::default();
    let results = hash_files(tmp.path(), &files, &counters);
    assert_eq!(results.len(), 41);
    for (index, (file, result)) in files.iter().zip(&results).enumerate() {
        if file.path == "gone.txt" {
            assert!(result.is_err(), "slot {index}");
        } else {
            let expected = fs::read(tmp.path().join(&file.path)).unwrap();
            assert_eq!(
                result.as_ref().unwrap().blake3,
                *blake3::hash(&expected).as_bytes(),
                "slot {index} must hold its own file's hash"
            );
        }
    }
    assert_eq!(
        counters.snapshot().hashed,
        40,
        "failures are not counted as hashes"
    );
}
