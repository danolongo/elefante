use std::collections::HashMap;

use elefante_mem::{
    RepoId, Store,
    extract::{Import, Symbol},
    graph::{self, Applied, Batch, FileUpdate, GraphSnapshot},
};
use serde_json::Value;

fn store() -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let store = Store::open(&RepoId::resolve_with_home(tmp.path(), None).unwrap()).unwrap();
    (tmp, store)
}

fn sym(kind: &str, qname: &str, signature: &str, doc: &str, line: u32) -> Symbol {
    Symbol {
        kind: kind.to_owned(),
        name: qname.rsplit("::").next().unwrap().to_owned(),
        qname: qname.to_owned(),
        signature: signature.to_owned(),
        doc: doc.to_owned(),
        line,
        end_line: line + 1,
        via_trait: None,
    }
}

fn file(path: &str, doc: &str, symbols: Vec<Symbol>, imports: &[(&str, &str)]) -> FileUpdate {
    FileUpdate {
        path: path.to_owned(),
        language: Some("rust".to_owned()),
        size: 100,
        file_doc: doc.to_owned(),
        symbols,
        imports: imports
            .iter()
            .map(|(k, s)| Import {
                kind: (*k).to_owned(),
                spec: (*s).to_owned(),
            })
            .collect(),
        truncated: false,
    }
}

fn apply(store: &Store, updates: Vec<FileUpdate>, deletes: &[&str], now: i64) -> Applied {
    let batch = Batch {
        updates,
        deletes: deletes.iter().map(|s| (*s).to_owned()).collect(),
    };
    store
        .writer()
        .call_blocking(move |s| graph::apply_batch(&mut s.memory, &batch, now).unwrap())
        .unwrap()
}

fn ids(store: &Store) -> HashMap<String, i64> {
    store
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

fn updated_at(store: &Store) -> HashMap<String, i64> {
    store
        .writer()
        .call_blocking(|s| {
            s.memory
                .prepare("SELECT key, updated_at_ns FROM node")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
        .unwrap()
}

fn snapshot(store: &Store) -> GraphSnapshot {
    store
        .writer()
        .call_blocking(|s| graph::snapshot(&s.memory).unwrap())
        .unwrap()
}

fn generation(store: &Store) -> i64 {
    store
        .writer()
        .call_blocking(|s| graph::generation(&s.memory).unwrap())
        .unwrap()
}

/// FTS rows must be exactly the nodes: none orphaned by a delete, none missing after an insert.
fn assert_fts_matches_nodes(store: &Store) {
    let (nodes, fts, orphans, missing): (i64, i64, i64, i64) = store
        .writer()
        .call_blocking(|s| {
            let n = |sql: &str| s.memory.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
            (
                n("SELECT count(*) FROM node"),
                n("SELECT count(*) FROM node_fts"),
                n("SELECT count(*) FROM node_fts WHERE rowid NOT IN (SELECT id FROM node)"),
                n("SELECT count(*) FROM node WHERE id NOT IN (SELECT rowid FROM node_fts)"),
            )
        })
        .unwrap();
    assert_eq!(orphans, 0, "FTS rows without a node");
    assert_eq!(missing, 0, "nodes without an FTS row");
    assert_eq!(nodes, fts);
}

#[test]
fn a_file_with_symbols_and_imports_is_written_as_nodes_and_import_refs() {
    let (_tmp, store) = store();
    let applied = apply(
        &store,
        vec![file(
            "src/lib.rs",
            "Core loop.",
            vec![
                sym("fn", "run", "pub fn run()", "Runs a turn.", 3),
                sym("method", "Session::prompt", "fn prompt(&self)", "", 9),
            ],
            &[("use", "crate::sse::SseDecoder"), ("mod", "sse")],
        )],
        &[],
        10,
    );
    assert!(applied.changed && applied.path_set_changed);
    assert_eq!(applied.import_changed_files, ["src/lib.rs"]);

    let snap = snapshot(&store);
    let kinds: Vec<(&str, &str)> = snap
        .nodes
        .iter()
        .map(|n| (n.0.as_str(), n.1.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("file", "src/lib.rs"),
            ("symbol", "src/lib.rs::Session::prompt"),
            ("symbol", "src/lib.rs::run"),
        ]
    );
    let file_node = &snap.nodes[0];
    assert_eq!(
        (file_node.2.as_str(), file_node.3.as_str()),
        ("lib.rs", "Core loop.")
    );
    let attrs: Value = serde_json::from_str(&file_node.4).unwrap();
    assert_eq!(attrs["lang"], "rust");
    assert_eq!(attrs["size"], 100);
    let run = &snap.nodes[2];
    assert_eq!(
        run.3, "pub fn run()\nRuns a turn.",
        "signature, newline, doc"
    );
    let attrs: Value = serde_json::from_str(&run.4).unwrap();
    assert_eq!(
        (attrs["kind"].as_str(), attrs["line"].as_u64()),
        (Some("fn"), Some(3))
    );
    assert_eq!(snap.nodes[1].3, "fn prompt(&self)", "no doc, no newline");
    assert_eq!(
        snap.imports,
        [
            ("src/lib.rs".into(), "mod".into(), "sse".into()),
            (
                "src/lib.rs".into(),
                "use".into(),
                "crate::sse::SseDecoder".into()
            ),
        ]
    );
    assert_fts_matches_nodes(&store);
}

#[test]
fn symbols_belong_to_their_file_and_every_node_is_index_owned() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![file("a.rs", "", vec![sym("fn", "f", "fn f()", "", 1)], &[])],
        &[],
        10,
    );
    let (owned, parents): (i64, i64) = store
        .writer()
        .call_blocking(|s| {
            (
                s.memory
                    .query_row(
                        "SELECT count(*) FROM node WHERE source = 'index'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap(),
                s.memory
                    .query_row(
                        "SELECT count(*) FROM node c JOIN node p ON p.id = c.file_id
                         WHERE c.kind = 'symbol' AND p.kind = 'file' AND p.key = 'a.rs'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap(),
            )
        })
        .unwrap();
    assert_eq!((owned, parents), (2, 1));
}

#[test]
fn reapplying_identical_content_changes_nothing_at_all() {
    let (_tmp, store) = store();
    let update = || {
        file(
            "a.rs",
            "Doc.",
            vec![sym("fn", "f", "fn f()", "Does f.", 1)],
            &[("use", "x")],
        )
    };
    apply(&store, vec![update()], &[], 10);
    let before = (generation(&store), updated_at(&store), snapshot(&store));

    let again = apply(&store, vec![update()], &[], 99);
    assert!(!again.changed && !again.path_set_changed);
    assert!(again.import_changed_files.is_empty());
    assert_eq!(again.generation, before.0);
    assert_eq!(
        generation(&store),
        before.0,
        "no change, no generation bump"
    );
    assert_eq!(updated_at(&store), before.1, "no timestamp may move");
    assert_eq!(snapshot(&store), before.2);
}

#[test]
fn an_edit_keeps_unchanged_ids_and_moves_only_the_rows_that_changed() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![file(
            "a.rs",
            "Doc.",
            vec![
                sym("fn", "keep", "fn keep()", "", 1),
                sym("fn", "edit", "fn edit()", "", 3),
                sym("fn", "gone", "fn gone()", "", 5),
            ],
            &[],
        )],
        &[],
        10,
    );
    let ids_before = ids(&store);
    let stamps_before = updated_at(&store);
    let generation_before = generation(&store);

    // `edit` gains a parameter, `gone` disappears, `fresh` is new, `keep` is untouched.
    let applied = apply(
        &store,
        vec![file(
            "a.rs",
            "Doc.",
            vec![
                sym("fn", "keep", "fn keep()", "", 1),
                sym("fn", "edit", "fn edit(x: u32)", "", 3),
                sym("fn", "fresh", "fn fresh()", "", 7),
            ],
            &[],
        )],
        &[],
        50,
    );
    assert!(applied.changed && !applied.path_set_changed);
    assert_eq!(
        generation(&store),
        generation_before + 1,
        "one batch, one bump"
    );

    let ids_after = ids(&store);
    for key in ["a.rs", "a.rs::keep", "a.rs::edit"] {
        assert_eq!(ids_after[key], ids_before[key], "{key} must keep its id");
    }
    assert!(!ids_after.contains_key("a.rs::gone"));
    assert!(!ids_before.contains_key("a.rs::fresh") && ids_after.contains_key("a.rs::fresh"));

    let stamps_after = updated_at(&store);
    assert_eq!(stamps_after["a.rs"], stamps_before["a.rs"]);
    assert_eq!(stamps_after["a.rs::keep"], stamps_before["a.rs::keep"]);
    assert_eq!(stamps_after["a.rs::edit"], 50, "the edited row moved");
    assert_fts_matches_nodes(&store);
}

#[test]
fn duplicate_qualified_names_are_disambiguated_by_source_order() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![file(
            "a.rs",
            "",
            vec![
                sym("fn", "f", "fn f(a: u8)", "", 1),
                sym("fn", "f", "fn f(a: u16)", "", 5),
                sym("fn", "f", "fn f(a: u32)", "", 9),
            ],
            &[],
        )],
        &[],
        10,
    );
    let keys: Vec<String> = snapshot(&store)
        .nodes
        .into_iter()
        .filter(|n| n.0 == "symbol")
        .map(|n| n.1)
        .collect();
    assert_eq!(keys, ["a.rs::f", "a.rs::f#2", "a.rs::f#3"]);
}

#[test]
fn deleting_a_file_removes_its_symbols_imports_edges_and_search_rows() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![
            file(
                "a.rs",
                "",
                vec![sym("fn", "f", "fn f()", "", 1)],
                &[("use", "b")],
            ),
            file("b.rs", "", vec![sym("fn", "g", "fn g()", "", 1)], &[]),
        ],
        &[],
        10,
    );
    store
        .writer()
        .call_blocking(|s| {
            s.memory
                .execute(
                    "INSERT INTO edge SELECT a.id, b.id, 'depends_on', 1.0, 1
                     FROM node a, node b WHERE a.key = 'a.rs' AND b.key = 'b.rs'",
                    [],
                )
                .unwrap();
        })
        .unwrap();
    let before = generation(&store);

    let applied = apply(&store, vec![], &["a.rs"], 20);
    assert!(applied.changed && applied.path_set_changed);
    assert_eq!(generation(&store), before + 1);
    let snap = snapshot(&store);
    let keys: Vec<&str> = snap.nodes.iter().map(|n| n.1.as_str()).collect();
    assert_eq!(keys, ["b.rs", "b.rs::g"]);
    assert!(
        snap.edges.is_empty(),
        "an edge cascades with either endpoint"
    );
    assert!(snap.imports.is_empty());
    assert_fts_matches_nodes(&store);

    let missing = apply(&store, vec![], &["never-existed.rs"], 30);
    assert!(!missing.changed, "deleting nothing changes nothing");
}

#[test]
fn a_batch_of_many_files_bumps_the_generation_exactly_once() {
    let (_tmp, store) = store();
    let before = generation(&store);
    let updates = (0..25)
        .map(|i| {
            file(
                &format!("f{i}.rs"),
                "",
                vec![sym("fn", "f", "fn f()", "", 1)],
                &[],
            )
        })
        .collect();
    let applied = apply(&store, updates, &[], 10);
    assert_eq!(applied.generation, before + 1);
    assert_eq!(generation(&store), before + 1);
}

#[test]
fn imports_are_replaced_and_reported_only_when_they_actually_change() {
    let (_tmp, store) = store();
    let with = |imports: &[(&str, &str)]| file("a.rs", "", vec![], imports);
    let first = apply(&store, vec![with(&[("use", "x"), ("use", "y")])], &[], 10);
    assert_eq!(first.import_changed_files, ["a.rs"]);

    let same = apply(&store, vec![with(&[("use", "y"), ("use", "x")])], &[], 20);
    assert!(
        same.import_changed_files.is_empty(),
        "order is not a change"
    );
    assert!(!same.changed);

    let changed = apply(&store, vec![with(&[("use", "x"), ("mod", "z")])], &[], 30);
    assert_eq!(changed.import_changed_files, ["a.rs"]);
    let specs: Vec<String> = snapshot(&store).imports.into_iter().map(|i| i.2).collect();
    assert_eq!(specs, ["z", "x"]);
}

#[test]
fn renaming_a_symbol_is_a_new_symbol_with_a_new_id() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![file(
            "a.rs",
            "",
            vec![sym("fn", "old", "fn old()", "", 1)],
            &[],
        )],
        &[],
        10,
    );
    let before = ids(&store)["a.rs::old"];
    apply(
        &store,
        vec![file(
            "a.rs",
            "",
            vec![sym("fn", "new", "fn new()", "", 1)],
            &[],
        )],
        &[],
        20,
    );
    let after = ids(&store);
    assert!(!after.contains_key("a.rs::old"));
    assert_ne!(after["a.rs::new"], before);
}

#[test]
fn the_orphan_sweep_removes_unlisted_files_and_nothing_else() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![
            file("keep.rs", "", vec![sym("fn", "k", "fn k()", "", 1)], &[]),
            file("drop.rs", "", vec![sym("fn", "d", "fn d()", "", 1)], &[]),
        ],
        &[],
        10,
    );
    let live: std::collections::HashSet<String> = ["keep.rs".to_owned()].into();
    let applied = store
        .writer()
        .call_blocking(move |s| graph::apply_removals(&mut s.memory, &[], Some(&live), 20).unwrap())
        .unwrap();
    assert!(applied.changed);
    let keys: Vec<String> = snapshot(&store).nodes.into_iter().map(|n| n.1).collect();
    assert_eq!(keys, ["keep.rs", "keep.rs::k"]);
    assert_fts_matches_nodes(&store);

    // Sweeping again with the same live set is a no-op: no change, no generation bump.
    let live: std::collections::HashSet<String> = ["keep.rs".to_owned()].into();
    let before = generation(&store);
    store
        .writer()
        .call_blocking(move |s| graph::apply_removals(&mut s.memory, &[], Some(&live), 30).unwrap())
        .unwrap();
    assert_eq!(generation(&store), before);
}

#[test]
fn rows_the_indexer_does_not_own_survive_deletes_and_sweeps() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![file("a.rs", "", vec![sym("fn", "f", "fn f()", "", 1)], &[])],
        &[],
        10,
    );
    store
        .writer()
        .call_blocking(|s| {
            s.memory
                .execute_batch(
                    "INSERT INTO node(kind, key, name, body, source, created_at_ns, updated_at_ns)
                       VALUES ('fact', 'prefers-tabs', 'prefers-tabs', 'The user prefers tabs.',
                               'learned:test', 1, 1),
                              ('file', 'learned.rs', 'learned.rs', '', 'learned:test', 1, 1);
                     INSERT INTO edge
                       SELECT f.id, a.id, 'about', 1.0, 1
                       FROM node f, node a WHERE f.key = 'prefers-tabs' AND a.key = 'a.rs';",
                )
                .unwrap();
        })
        .unwrap();

    // Delete the file the learned fact is about, name the learned file explicitly, and sweep
    // with an empty live set. None of that may touch a learned row.
    let applied = store
        .writer()
        .call_blocking(|s| {
            let empty = std::collections::HashSet::<String>::new();
            graph::apply_removals(
                &mut s.memory,
                &["a.rs".to_owned(), "learned.rs".to_owned()],
                Some(&empty),
                20,
            )
            .unwrap()
        })
        .unwrap();
    assert!(applied.changed);
    let snap = snapshot(&store);
    let keys: Vec<(&str, &str)> = snap
        .nodes
        .iter()
        .map(|n| (n.0.as_str(), n.1.as_str()))
        .collect();
    assert_eq!(keys, [("fact", "prefers-tabs"), ("file", "learned.rs")]);
    assert!(
        snap.edges.is_empty(),
        "the learned edge into the deleted file cascades; the fact itself stays"
    );
}

#[test]
fn a_file_without_an_extractor_is_a_path_only_node() {
    let (_tmp, store) = store();
    let mut update = file("Makefile", "", vec![], &[]);
    update.language = None;
    apply(&store, vec![update], &[], 10);
    let snap = snapshot(&store);
    let attrs: Value = serde_json::from_str(&snap.nodes[0].4).unwrap();
    assert!(attrs["lang"].is_null());
    assert_eq!(snap.nodes[0].3, "");
    assert_fts_matches_nodes(&store);
}

#[test]
fn identifier_words_in_names_and_signatures_are_searchable() {
    let (_tmp, store) = store();
    apply(
        &store,
        vec![file(
            "crates/core/src/turn_lock.rs",
            "",
            vec![sym(
                "method",
                "Session::takeTurnLock",
                "fn takeTurnLock(&self)",
                "Waits for the turn lock.",
                4,
            )],
            &[],
        )],
        &[],
        10,
    );
    let hits = |query: &'static str| -> Vec<String> {
        store
            .writer()
            .call_blocking(move |s| {
                s.memory
                    .prepare(
                        "SELECT n.key FROM node_fts f JOIN node n ON n.id = f.rowid
                         WHERE node_fts MATCH ?1 ORDER BY n.key",
                    )
                    .unwrap()
                    .query_map([query], |r| r.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            })
            .unwrap()
    };
    let symbol = "crates/core/src/turn_lock.rs::Session::takeTurnLock";
    assert!(hits("\"take\"").contains(&symbol.to_owned()));
    assert!(hits("\"turn\"").contains(&symbol.to_owned()));
    assert!(hits("\"lock\"").contains(&symbol.to_owned()));
    assert!(hits("\"session\"").contains(&symbol.to_owned()));
    // The unsplit form is the whole identifier, so a symbol is found by `sessiontaketurnlock`,
    // while a bare `turnlock` finds the *file* `turn_lock.rs` through its joined stem.
    assert!(hits("\"sessiontaketurnlock\"").contains(&symbol.to_owned()));
    assert_eq!(hits("\"turnlock\""), ["crates/core/src/turn_lock.rs"]);
    // The file is findable by the words of its path; its symbols are not, so a path match does
    // not flood the results with every symbol in the file.
    assert_eq!(hits("\"crates\""), ["crates/core/src/turn_lock.rs"]);
}
