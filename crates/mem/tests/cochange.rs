mod common;

use std::{
    collections::HashSet,
    fs,
    os::unix::fs::PermissionsExt,
    process::Command,
    time::{Duration, Instant},
};

use common::GitRepo;
use elefante_mem::{
    Counters, IndexStats, RepoId, Store,
    cochange::{self, Change, CoChangeConfig, Commit, Pair, compute, parse_log, run_bounded},
    graph::{self, GraphSnapshot},
    indexer::{IndexOptions, refresh_blocking},
};

// ----- parsing -----

const H1: &str = "1111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222";

/// `git log -z --format=%x00%H` framing: `\0hash\0`, then (if any) `\n` and NUL-terminated
/// status/path fields.
fn framed(commits: &[(&str, &[&[&[u8]]])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (hash, entries) in commits {
        out.extend_from_slice(b"\0");
        out.extend_from_slice(hash.as_bytes());
        out.extend_from_slice(b"\0");
        // Real git puts the newline before the first status only; an empty commit has none.
        if !entries.is_empty() {
            out.extend_from_slice(b"\n");
        }
        for fields in *entries {
            for field in *fields {
                out.extend_from_slice(field);
                out.push(0);
            }
        }
    }
    out
}

fn touch(path: &str) -> Change {
    Change::Touch(path.to_owned())
}

#[test]
fn parses_every_status_shape() {
    let raw = framed(&[
        (
            H1,
            &[
                &[b"M", b"src/b.rs"],
                &[b"R100", b"src/a.rs", b"src/c.rs"],
                &[b"D", b"gone.rs"],
                &[b"T", b"link"],
                &[b"C075", b"orig.rs", b"copy.rs"],
            ],
        ),
        (H2, &[&[b"A", b"src/a.rs"], &[b"A", b"src/b.rs"]]),
    ]);
    let commits = parse_log(&raw);
    assert_eq!(commits.len(), 2);
    assert_eq!(
        commits[0].changes,
        [
            touch("src/b.rs"),
            Change::Rename {
                from: "src/a.rs".to_owned(),
                to: "src/c.rs".to_owned()
            },
            touch("gone.rs"),
            touch("link"),
            touch("copy.rs"),
        ]
    );
    assert_eq!(commits[1].changes, [touch("src/a.rs"), touch("src/b.rs")]);
}

#[test]
fn tolerates_empty_garbage_and_truncated_input() {
    assert_eq!(parse_log(b"").len(), 0);
    assert_eq!(parse_log(b"warning: something\0\0junk").len(), 0);
    // A rename cut off before its second path must not invent one.
    let mut raw = framed(&[(H1, &[&[b"M", b"keep.rs"]])]);
    raw.extend_from_slice(b"R100\0only-one-path");
    let commits = parse_log(&raw);
    assert_eq!(commits[0].changes, [touch("keep.rs")]);
}

#[test]
fn a_commit_with_no_changes_is_still_a_commit() {
    let commits = parse_log(&framed(&[(H1, &[]), (H2, &[&[b"M", b"a.rs"]])]));
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0].changes.len(), 0);
}

#[test]
fn paths_that_are_not_utf8_are_skipped_and_do_not_derail_parsing() {
    let raw = framed(&[(H1, &[&[b"M", b"bad\xff.rs"], &[b"M", b"good.rs"]])]);
    let commits = parse_log(&raw);
    assert_eq!(commits[0].changes, [touch("good.rs")]);
}

#[test]
fn sha256_object_names_are_recognised() {
    let long = "a".repeat(64);
    let raw = framed(&[(&long, &[&[b"M", b"a.rs"]])]);
    assert_eq!(parse_log(&raw).len(), 1);
}

// ----- counting -----

fn commit(files: &[&str]) -> Commit {
    Commit {
        changes: files.iter().map(|f| touch(f)).collect(),
    }
}

fn set(files: &[&str]) -> HashSet<String> {
    files.iter().map(|f| (*f).to_owned()).collect()
}

fn pairs(result: &[Pair]) -> Vec<(&str, &str, u32)> {
    result
        .iter()
        .map(|p| (p.a.as_str(), p.b.as_str(), p.weight))
        .collect()
}

/// Newest first, as git emits them.
fn fixture_history() -> Vec<Commit> {
    let mass: Vec<String> = (0..60).map(|i| format!("m{i:02}.rs")).collect();
    let mut mass_files: Vec<&str> = mass.iter().map(String::as_str).collect();
    mass_files.extend(["a.rs", "b.rs"]);
    vec![
        commit(&mass_files),               // c7: 62 files, skipped
        commit(&["c.rs", "d.rs"]),         // c6
        commit(&["a.rs", "d.rs"]),         // c5
        commit(&["c.rs", "d.rs"]),         // c4
        commit(&["a.rs", "b.rs", "c.rs"]), // c3
        commit(&["a.rs", "b.rs"]),         // c2
        commit(&["a.rs", "b.rs", "c.rs"]), // c1
    ]
}

fn fixture_files() -> HashSet<String> {
    let mut files = set(&["a.rs", "b.rs", "c.rs", "d.rs"]);
    files.extend((0..60).map(|i| format!("m{i:02}.rs")));
    files
}

#[test]
fn weights_match_the_hand_computed_table() {
    // ab: c1 c2 c3 = 3     ac: c1 c3 = 2     bc: c1 c3 = 2
    // cd: c4 c6   = 2      ad: c5 = 1 (below the support floor)
    // c7 touched 62 files and contributes nothing, so ab stays 3.
    let result = compute(
        &fixture_history(),
        &fixture_files(),
        &CoChangeConfig::default(),
    );
    assert_eq!(
        pairs(&result),
        [
            ("a.rs", "b.rs", 3),
            ("a.rs", "c.rs", 2),
            ("b.rs", "c.rs", 2),
            ("c.rs", "d.rs", 2)
        ]
    );
}

#[test]
fn the_large_commit_threshold_is_exact() {
    let config = CoChangeConfig {
        max_commit_files: 3,
        min_support: 1,
        ..CoChangeConfig::default()
    };
    let at_limit = compute(&[commit(&["a", "b", "c"])], &set(&["a", "b", "c"]), &config);
    assert_eq!(at_limit.len(), 3, "exactly at the limit still counts");
    let over = compute(
        &[commit(&["a", "b", "c", "d"])],
        &set(&["a", "b", "c", "d"]),
        &config,
    );
    assert!(over.is_empty(), "one over is skipped");
}

#[test]
fn the_size_threshold_counts_every_path_in_the_commit_not_only_current_files() {
    // Four paths changed, only two still exist. Dropping to two *after* counting must not
    // let a mass commit back in.
    let config = CoChangeConfig {
        max_commit_files: 3,
        min_support: 1,
        ..CoChangeConfig::default()
    };
    let result = compute(
        &[commit(&["a", "b", "gone1", "gone2"])],
        &set(&["a", "b"]),
        &config,
    );
    assert_eq!(result.len(), 0);
}

#[test]
fn the_support_floor_is_configurable() {
    let history = [commit(&["a", "b"])];
    let files = set(&["a", "b"]);
    assert_eq!(
        compute(&history, &files, &CoChangeConfig::default()).len(),
        0
    );
    let config = CoChangeConfig {
        min_support: 1,
        ..CoChangeConfig::default()
    };
    assert_eq!(pairs(&compute(&history, &files, &config)), [("a", "b", 1)]);
}

#[test]
fn top_k_keeps_an_edge_that_is_in_either_endpoints_list() {
    // Weights: ab 3, ac 2, bc 2, cd 2. With k = 1: a -> ab; b -> ab; c -> ac (ties broken by
    // the other path, so `a` wins over `b` and `d`); d -> cd. bc is in neither list.
    let config = CoChangeConfig {
        top_k: 1,
        ..CoChangeConfig::default()
    };
    let result = compute(&fixture_history(), &fixture_files(), &config);
    assert_eq!(
        pairs(&result),
        [
            ("a.rs", "b.rs", 3),
            ("a.rs", "c.rs", 2),
            ("c.rs", "d.rs", 2)
        ]
    );
}

#[test]
fn files_that_no_longer_exist_and_duplicates_within_a_commit_are_handled() {
    let history = [
        commit(&["a", "b", "b", "deleted"]),
        commit(&["a", "b", "deleted"]),
    ];
    let result = compute(&history, &set(&["a", "b"]), &CoChangeConfig::default());
    assert_eq!(
        pairs(&result),
        [("a", "b", 2)],
        "a path twice in one commit counts once"
    );
}

#[test]
fn only_the_newest_max_commits_are_read() {
    let history = [
        commit(&["a", "b"]),
        commit(&["a", "b"]),
        commit(&["a", "b"]),
    ];
    let config = CoChangeConfig {
        max_commits: 2,
        ..CoChangeConfig::default()
    };
    assert_eq!(
        pairs(&compute(&history, &set(&["a", "b"]), &config)),
        [("a", "b", 2)]
    );
}

fn rename(from: &str, to: &str) -> Commit {
    Commit {
        changes: vec![Change::Rename {
            from: from.to_owned(),
            to: to.to_owned(),
        }],
    }
}

#[test]
fn history_before_a_rename_belongs_to_the_new_name() {
    // Newest first: after the rename `new.rs` and `x.rs` changed together once; before it, as
    // `old.rs`, they changed together twice.
    let history = [
        commit(&["new.rs", "x.rs"]),
        rename("old.rs", "new.rs"),
        commit(&["old.rs", "x.rs"]),
        commit(&["old.rs", "x.rs"]),
    ];
    let result = compute(
        &history,
        &set(&["new.rs", "x.rs"]),
        &CoChangeConfig::default(),
    );
    assert_eq!(pairs(&result), [("new.rs", "x.rs", 3)]);
}

#[test]
fn a_different_file_later_created_at_the_old_name_is_not_the_renamed_one() {
    let history = [
        commit(&["old.rs", "x.rs"]), // a brand-new old.rs, after the rename
        rename("old.rs", "new.rs"),
        commit(&["old.rs", "x.rs"]),
        commit(&["old.rs", "x.rs"]),
    ];
    let config = CoChangeConfig {
        min_support: 1,
        ..CoChangeConfig::default()
    };
    let result = compute(&history, &set(&["old.rs", "new.rs", "x.rs"]), &config);
    assert_eq!(
        pairs(&result),
        [("new.rs", "x.rs", 2), ("old.rs", "x.rs", 1)]
    );
}

#[test]
fn rename_chains_resolve_all_the_way_to_the_current_name() {
    let history = [
        rename("b.rs", "c.rs"),
        rename("a.rs", "b.rs"),
        commit(&["a.rs", "z.rs"]),
        commit(&["a.rs", "z.rs"]),
    ];
    let result = compute(
        &history,
        &set(&["c.rs", "z.rs"]),
        &CoChangeConfig::default(),
    );
    assert_eq!(pairs(&result), [("c.rs", "z.rs", 2)]);
}

// ----- the runner -----

#[test]
fn run_bounded_returns_output_and_rejects_failure_and_absence() {
    let ok = run_bounded(Command::new("echo").arg("hi"), Duration::from_secs(5), 1024);
    assert_eq!(ok.as_deref(), Some(&b"hi\n"[..]));
    assert!(run_bounded(&mut Command::new("false"), Duration::from_secs(5), 1024).is_none());
    assert!(
        run_bounded(
            &mut Command::new("elefante-no-such-binary"),
            Duration::from_secs(5),
            1024
        )
        .is_none()
    );
}

#[test]
fn run_bounded_gives_up_on_a_command_that_runs_past_its_timeout() {
    let started = Instant::now();
    let out = run_bounded(
        Command::new("sleep").arg("30"),
        Duration::from_millis(150),
        1024,
    );
    assert!(out.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it must not wait for the command"
    );
}

#[test]
fn run_bounded_stops_reading_an_endless_command_at_the_cap() {
    let out = run_bounded(&mut Command::new("yes"), Duration::from_secs(10), 1000).unwrap();
    assert_eq!(out.len(), 1000);
}

// ----- against real git -----

struct Indexed {
    store: Store,
    root: std::path::PathBuf,
}

fn open(git: &GitRepo) -> Indexed {
    let repo = RepoId::resolve_with_home(git.path(), None).unwrap();
    Indexed {
        root: repo.root().to_path_buf(),
        store: Store::open_with_key(&repo, elefante_mem::MemKey::from_bytes([7; 32])).unwrap(),
    }
}

fn refresh(indexed: &Indexed) -> (IndexStats, i64) {
    let counters = Counters::default();
    let report = refresh_blocking(
        &indexed.store,
        &indexed.root,
        &counters,
        &IndexOptions::default(),
    )
    .unwrap();
    (counters.snapshot(), report.generation)
}

fn snapshot(indexed: &Indexed) -> GraphSnapshot {
    indexed
        .store
        .writer()
        .call_blocking(|s| graph::snapshot(&s.memory).unwrap())
        .unwrap()
}

/// `(a, b, weight)` for every co-change edge, each pair sorted so edge orientation is irrelevant.
fn co_edges(snapshot: &GraphSnapshot) -> Vec<(String, String, String)> {
    let mut edges: Vec<(String, String, String)> = snapshot
        .edges
        .iter()
        .filter(|e| e.2 == "co_edited_with")
        .map(|e| {
            let (a, b) = if e.0 <= e.1 {
                (&e.0, &e.1)
            } else {
                (&e.1, &e.0)
            };
            (a.clone(), b.clone(), e.3.clone())
        })
        .collect();
    edges.sort();
    edges
}

fn edge(a: &str, b: &str, weight: &str) -> (String, String, String) {
    (a.to_owned(), b.to_owned(), weight.to_owned())
}

fn fixture_repo() -> GitRepo {
    let mut git = GitRepo::init();
    for f in ["a.rs", "b.rs", "c.rs"] {
        git.write(f, &format!("// {f} v1\n"));
    }
    git.commit("c1");
    for f in ["a.rs", "b.rs"] {
        git.write(f, &format!("// {f} v2\n"));
    }
    git.commit("c2");
    for f in ["a.rs", "b.rs", "c.rs"] {
        git.write(f, &format!("// {f} v3\n"));
    }
    git.commit("c3");
    git.write("d.rs", "// d v1\n");
    git.write("c.rs", "// c v4\n");
    git.commit("c4");
    git.write("a.rs", "// a v5\n");
    git.write("d.rs", "// d v5\n");
    git.commit("c5");
    git.write("c.rs", "// c v6\n");
    git.write("d.rs", "// d v6\n");
    git.commit("c6");
    for i in 0..60 {
        git.write(&format!("m{i:02}.rs"), "// mass\n");
    }
    git.write("a.rs", "// a v7\n");
    git.write("b.rs", "// b v7\n");
    git.commit("c7 mass");
    git
}

#[test]
fn a_fixture_repository_produces_exactly_the_hand_computed_weights() {
    let git = fixture_repo();
    let indexed = open(&git);
    let (stats, _) = refresh(&indexed);
    assert_eq!(stats.git_log_runs, 1);
    assert_eq!(
        co_edges(&snapshot(&indexed)),
        [
            edge("a.rs", "b.rs", "3.0"),
            edge("a.rs", "c.rs", "2.0"),
            edge("b.rs", "c.rs", "2.0"),
            edge("c.rs", "d.rs", "2.0"),
        ]
    );
}

#[test]
fn history_before_a_git_rename_counts_for_the_renamed_file() {
    let mut git = GitRepo::init();
    git.write("old.rs", "// shared body that will not change\n");
    git.write("x.rs", "// x v1\n");
    git.commit("c1");
    git.write("old.rs", "// shared body that will not change\n// edit\n");
    git.write("x.rs", "// x v2\n");
    git.commit("c2");
    git.rename("old.rs", "new.rs");
    git.commit("c3 rename");
    git.write(
        "new.rs",
        "// shared body that will not change\n// edit\n// more\n",
    );
    git.write("x.rs", "// x v3\n");
    git.commit("c4");

    let indexed = open(&git);
    refresh(&indexed);
    assert_eq!(
        co_edges(&snapshot(&indexed)),
        [edge("new.rs", "x.rs", "3.0")],
        "c1 and c2 happened to `old.rs`, which is now `new.rs`"
    );
}

#[test]
fn git_log_runs_only_when_head_has_moved() {
    let mut git = fixture_repo();
    let indexed = open(&git);
    assert_eq!(refresh(&indexed).0.git_log_runs, 1);

    // Same HEAD: nothing to recompute.
    assert_eq!(refresh(&indexed).0.git_log_runs, 0);

    // An uncommitted edit has no history, so it cannot change the answer either.
    git.write("a.rs", "// uncommitted\n");
    assert_eq!(refresh(&indexed).0.git_log_runs, 0);

    // A new commit moves HEAD and the weights.
    git.write("a.rs", "// a v8\n");
    git.write("b.rs", "// b v8\n");
    git.commit("c8");
    let before = snapshot(&indexed);
    let (stats, _) = refresh(&indexed);
    assert_eq!(stats.git_log_runs, 1);
    assert_ne!(co_edges(&before), co_edges(&snapshot(&indexed)));
    assert!(co_edges(&snapshot(&indexed)).contains(&edge("a.rs", "b.rs", "4.0")));
}

#[test]
fn recomputing_identical_edges_does_not_bump_the_generation() {
    let mut git = fixture_repo();
    let indexed = open(&git);
    refresh(&indexed);
    let (_, before) = refresh(&indexed);

    // A commit that changes no file the graph cares about pairwise: HEAD moves, weights do not.
    git.write("only-one.rs", "// solo\n");
    git.commit("c8 solo");
    let (stats, generation) = refresh(&indexed);
    assert_eq!(stats.git_log_runs, 1);
    // The new file bumps the generation once for its own rows; the unchanged edge set adds no
    // second bump (an import edge pass would have run, and found nothing to write either).
    assert_eq!(generation, before + 1);
}

#[test]
fn a_directory_that_is_not_a_repository_simply_has_no_co_change() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
    let repo = RepoId::resolve_with_home(tmp.path(), None).unwrap();
    let store = Store::open_with_key(&repo, elefante_mem::MemKey::from_bytes([7; 32])).unwrap();
    let counters = Counters::default();
    refresh_blocking(&store, repo.root(), &counters, &IndexOptions::default()).unwrap();

    assert_eq!(counters.snapshot().git_log_runs, 0);
    let (edges, head): (i64, Option<String>) = store
        .writer()
        .call_blocking(|s| {
            (
                s.memory
                    .query_row(
                        "SELECT count(*) FROM edge WHERE rel = 'co_edited_with'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap(),
                s.memory
                    .query_row(
                        "SELECT value FROM meta WHERE key = 'cochange_head'",
                        [],
                        |r| r.get(0),
                    )
                    .ok(),
            )
        })
        .unwrap();
    assert_eq!(edges, 0);
    assert_eq!(head, None);
}

#[test]
fn an_unborn_branch_has_no_head_and_no_edges() {
    let git = GitRepo::init();
    git.write("a.rs", "fn a() {}\n");
    assert!(cochange::head(git.path()).is_none());
    let indexed = open(&git);
    let (stats, _) = refresh(&indexed);
    assert_eq!(stats.git_log_runs, 0);
    assert_eq!(co_edges(&snapshot(&indexed)).len(), 0);
}

#[test]
fn head_is_a_full_object_name_once_there_is_a_commit() {
    let mut git = GitRepo::init();
    git.write("a.rs", "fn a() {}\n");
    git.commit("c1");
    let head = cochange::head(git.path()).unwrap();
    assert_eq!(head.len(), 40);
    assert!(head.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn a_repositorys_own_config_cannot_run_programs_on_our_behalf() {
    let mut git = GitRepo::init();
    for f in ["a.rs", "b.rs"] {
        git.write(f, "// v1\n");
    }
    git.commit("c1");
    for f in ["a.rs", "b.rs"] {
        git.write(f, "// v2\n");
    }
    git.commit("c2");

    // Every hook a hostile config could name, all pointing at a script that leaves a marker.
    let marker = git.path().join("..").join(format!(
        "elefante-pwned-{}",
        git.path().file_name().unwrap().to_string_lossy()
    ));
    let script = git.path().join(".git").join("pwn.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let config = git.path().join(".git").join("config");
    let mut text = fs::read_to_string(&config).unwrap();
    let hostile = format!(
        "[core]\n\tfsmonitor = {0}\n\tpager = {0}\n[diff]\n\texternal = {0}\n\ttextconv = {0}\n",
        script.display()
    );
    text.push_str(&hostile);
    fs::write(&config, text).unwrap();

    let indexed = open(&git);
    refresh(&indexed);

    assert!(
        co_edges(&snapshot(&indexed)).contains(&edge("a.rs", "b.rs", "2.0")),
        "git did run, and produced edges"
    );
    assert!(!marker.exists(), "a repository config executed a program");
    let _ = fs::remove_file(&marker);
}
