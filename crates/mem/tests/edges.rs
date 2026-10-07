mod common;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use common::tree::Tree;
use elefante_mem::{
    MemError,
    graph::GraphSnapshot,
    indexer::{Hooks, IndexOptions},
};

const FILES: &[(&str, &str)] = &[
    (
        "src/lib.rs",
        "mod util;\nmod gate;\nuse crate::util::helper;\npub fn run() {}\n",
    ),
    ("src/util.rs", "pub fn helper() {}\n"),
    ("src/gate/mod.rs", "pub mod rules;\npub struct Verdict;\n"),
    (
        "src/gate/rules.rs",
        "use super::Verdict;\nuse std::fmt::Display;\n",
    ),
    (
        "ui/app.tsx",
        "import { x } from './util';\nimport { y } from './foo';\nimport React from 'react';\n",
    ),
    ("ui/util.ts", "export function x() {}\n"),
];

fn edges(snapshot: &GraphSnapshot) -> Vec<(&str, &str)> {
    snapshot
        .edges
        .iter()
        .filter(|e| e.2 == "depends_on")
        .map(|e| (e.0.as_str(), e.1.as_str()))
        .collect()
}

#[test]
fn imports_become_depends_on_edges_between_files() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let snapshot = tree.snapshot();
    assert_eq!(
        edges(&snapshot),
        [
            ("src/gate/mod.rs", "src/gate/rules.rs"),
            ("src/gate/rules.rs", "src/gate/mod.rs"),
            ("src/lib.rs", "src/gate/mod.rs"),
            ("src/lib.rs", "src/util.rs"),
            ("ui/app.tsx", "ui/util.ts"),
        ],
        "`mod util;` and `use crate::util::helper` are one edge, not two"
    );
    assert!(snapshot.edges.iter().all(|e| e.3 == "1.0"), "weight is 1.0");
}

#[test]
fn unresolved_imports_stay_recorded_but_make_no_edge() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let snapshot = tree.snapshot();
    let specs: Vec<&str> = snapshot
        .imports
        .iter()
        .filter(|i| i.0 == "ui/app.tsx")
        .map(|i| i.2.as_str())
        .collect();
    assert_eq!(specs, ["./foo", "./util", "react"]);
    assert!(!edges(&snapshot).iter().any(|e| e.1.contains("foo")));
}

#[test]
fn creating_a_file_resolves_an_import_that_was_dangling_without_touching_the_importer() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let before_ids = tree.ids();
    let before_generation = tree.generation();

    tree.write("ui/foo.ts", "export function y() {}\n", 1800);
    tree.refresh();

    let snapshot = tree.snapshot();
    assert!(edges(&snapshot).contains(&("ui/app.tsx", "ui/foo.ts")));
    assert_eq!(tree.ids()["ui/app.tsx"], before_ids["ui/app.tsx"]);
    assert_eq!(
        tree.generation(),
        before_generation + 2,
        "one bump for the new file's rows, one when edges are re-derived"
    );
    assert_eq!(
        tree.meta("edges_dirty"),
        None,
        "the flag is cleared once edges are current"
    );
}

#[test]
fn deleting_a_file_removes_the_edge_to_it_and_leaves_the_importer_unchanged() {
    let tree = Tree::new(FILES);
    tree.write("ui/foo.ts", "export function y() {}\n", 3600);
    tree.refresh();
    assert!(edges(&tree.snapshot()).contains(&("ui/app.tsx", "ui/foo.ts")));
    let importer = tree
        .snapshot()
        .nodes
        .into_iter()
        .find(|n| n.1 == "ui/app.tsx")
        .unwrap();

    std::fs::remove_file(tree.root.join("ui/foo.ts")).unwrap();
    tree.refresh();

    let snapshot = tree.snapshot();
    assert!(!edges(&snapshot).iter().any(|e| e.1 == "ui/foo.ts"));
    let after = snapshot
        .nodes
        .into_iter()
        .find(|n| n.1 == "ui/app.tsx")
        .unwrap();
    assert_eq!(
        after, importer,
        "the importing file's own rows did not change"
    );
}

#[test]
fn an_edit_that_changes_no_imports_does_not_re_derive_edges() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let generation = tree.generation();
    let edges_before = tree.snapshot().edges;

    tree.write(
        "src/util.rs",
        "pub fn helper() {}\npub fn more() {}\n",
        1800,
    );
    tree.refresh();

    assert_eq!(
        tree.generation(),
        generation + 1,
        "only the content batch bumps"
    );
    assert_eq!(tree.snapshot().edges, edges_before);
}

#[test]
fn changing_a_files_imports_updates_its_edges() {
    let tree = Tree::new(FILES);
    tree.refresh();
    tree.write(
        "src/util.rs",
        "use crate::gate::Verdict;\npub fn helper() {}\n",
        1800,
    );
    tree.refresh();
    assert!(edges(&tree.snapshot()).contains(&("src/util.rs", "src/gate/mod.rs")));

    tree.write("src/util.rs", "pub fn helper() {}\n", 1700);
    tree.refresh();
    assert!(!edges(&tree.snapshot()).iter().any(|e| e.0 == "src/util.rs"));
}

#[test]
fn a_warm_refresh_derives_nothing_and_writes_nothing() {
    let tree = Tree::new(FILES);
    tree.refresh();
    let generation = tree.generation();
    let (_, stats) = tree.refresh();
    assert_eq!(stats.batches, 0);
    assert_eq!(tree.generation(), generation);
}

struct CrashOnce(AtomicBool);

impl Hooks for CrashOnce {
    fn after_graph_commit(&self) -> Result<(), MemError> {
        if self.0.swap(false, Ordering::SeqCst) {
            Err(MemError::Aborted("injected".to_owned()))
        } else {
            Ok(())
        }
    }
}

#[test]
fn edges_survive_a_crash_after_the_content_committed_but_before_they_were_derived() {
    let mut tree = Tree::new(FILES);
    let options = IndexOptions {
        batch_files: 1_000,
        hooks: Arc::new(CrashOnce(AtomicBool::new(true))),
        ..IndexOptions::default()
    };
    assert!(tree.refresh_with(&options).is_err());
    // The window: every file and import is in the graph, no edge is, and the flag says so.
    let snapshot = tree.snapshot();
    assert!(!snapshot.imports.is_empty());
    assert!(edges(&snapshot).is_empty());
    assert_eq!(tree.meta("edges_dirty").as_deref(), Some("1"));

    tree.close();
    tree.reopen();
    tree.refresh();

    let clean = Tree::new(FILES);
    clean.refresh();
    assert_eq!(tree.snapshot(), clean.snapshot(), "edges included");
}

#[test]
fn edges_between_other_nodes_are_not_the_resolvers_to_remove() {
    let tree = Tree::new(FILES);
    tree.refresh();
    tree.store()
        .writer()
        .call_blocking(|s| {
            s.memory
                .execute_batch(
                    "INSERT INTO node(kind, key, name, body, source, created_at_ns, updated_at_ns)
                       VALUES ('decision', 'use-sqlite', 'use-sqlite', 'We use SQLite.',
                               'learned:test', 1, 1);
                     INSERT INTO edge
                       SELECT d.id, f.id, 'depends_on', 1.0, 1
                       FROM node d, node f WHERE d.key = 'use-sqlite' AND f.key = 'src/util.rs';",
                )
                .unwrap();
        })
        .unwrap();

    // Force a re-derivation.
    tree.write("ui/new.ts", "export const z = () => 1;\n", 1800);
    tree.refresh();

    let snapshot = tree.snapshot();
    assert!(
        snapshot
            .edges
            .iter()
            .any(|e| e.0 == "use-sqlite" && e.1 == "src/util.rs" && e.2 == "depends_on"),
        "a depends_on edge from a learned node must survive re-derivation"
    );
}
