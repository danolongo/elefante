//! Indexes a real repository and prints what the graph looks like. Not part of the gate: it needs
//! a path and exists so a human can judge whether the edges are plausible.
//!
//! ```sh
//! ELEFANTE_DOGFOOD=/path/to/a/copy cargo test -p elefante-mem --test dogfood -- --ignored --nocapture
//! ```
//!
//! Point it at a *copy*: indexing creates `.elefante/` inside the directory.

use std::path::PathBuf;

use elefante_mem::{
    Counters, RepoId, Store, graph,
    indexer::{IndexOptions, refresh_blocking},
};

#[test]
#[ignore = "needs ELEFANTE_DOGFOOD=<path to a repository copy>"]
fn index_a_real_repository_and_print_it() {
    let root = PathBuf::from(std::env::var("ELEFANTE_DOGFOOD").expect("set ELEFANTE_DOGFOOD"));
    let repo = RepoId::resolve_with_home(&root, None).unwrap();
    let store = Store::open(&repo).unwrap();
    let counters = Counters::default();
    let started = std::time::Instant::now();
    let report =
        refresh_blocking(&store, repo.root(), &counters, &IndexOptions::default()).unwrap();
    println!(
        "cold: {:?} {:?} generation={}",
        started.elapsed(),
        counters.snapshot(),
        report.generation
    );

    let warm = Counters::default();
    let started = std::time::Instant::now();
    refresh_blocking(&store, repo.root(), &warm, &IndexOptions::default()).unwrap();
    println!("warm: {:?} {:?}", started.elapsed(), warm.snapshot());

    let snapshot = store
        .writer()
        .call_blocking(|s| graph::snapshot(&s.memory).unwrap())
        .unwrap();
    let files = snapshot.nodes.iter().filter(|n| n.0 == "file").count();
    let symbols = snapshot.nodes.iter().filter(|n| n.0 == "symbol").count();
    let unresolved = snapshot.imports.len() - snapshot.edges.len();
    println!(
        "files={files} symbols={symbols} imports={} depends_on={} (imports without an edge ~{unresolved})",
        snapshot.imports.len(),
        snapshot
            .edges
            .iter()
            .filter(|e| e.2 == "depends_on")
            .count()
    );
    for edge in snapshot.edges.iter().filter(|e| e.2 == "depends_on") {
        println!("EDGE {} -> {}", edge.0, edge.1);
    }
    for node in snapshot.nodes.iter().filter(|n| n.0 == "symbol").take(40) {
        println!("SYM  {}  |  {}", node.1, node.3.replace('\n', "  //  "));
    }
}
