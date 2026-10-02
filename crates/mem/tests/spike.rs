//! Premises the M2 design asserts. Each test here pins a fact the design rests on, so a
//! dependency bump or a toolchain change that falsifies one fails loudly instead of silently
//! degrading retrieval.

mod common;

use std::ops::ControlFlow;

use common::GitRepo;
use rusqlite::Connection;
use tree_sitter::{ParseOptions, Parser};

fn memory() -> Connection {
    Connection::open_in_memory().expect("in-memory sqlite")
}

#[test]
fn bundled_sqlite_has_fts5_and_window_functions() {
    let db = memory();
    let fts5: i64 = db
        .query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fts5, 1, "bundled SQLite must be built with FTS5");

    db.execute_batch(
        "CREATE TABLE e(src INTEGER, dst INTEGER, w REAL);
         INSERT INTO e VALUES (1,10,1),(1,11,5),(1,12,3),(2,20,2);",
    )
    .unwrap();
    // The per-node fan-out cap in § 10.4 relies on exactly this shape.
    let kept: Vec<(i64, i64)> = db
        .prepare(
            "SELECT src, dst FROM (
               SELECT src, dst, ROW_NUMBER() OVER (PARTITION BY src ORDER BY w DESC) AS rn FROM e
             ) WHERE rn <= 2 ORDER BY src, dst",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(kept, vec![(1, 11), (1, 12), (2, 20)]);
}

fn fts_table() -> Connection {
    let db = memory();
    db.execute_batch(
        "CREATE VIRTUAL TABLE t USING fts5(name_terms, body_terms,
           tokenize = 'porter unicode61 remove_diacritics 2');",
    )
    .unwrap();
    db
}

fn matches(db: &Connection, query: &str) -> rusqlite::Result<Vec<i64>> {
    let mut statement = db.prepare("SELECT rowid FROM t WHERE t MATCH ?1 ORDER BY rowid")?;
    statement
        .query_map([query], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()
}

#[test]
fn unicode61_splits_snake_case_but_not_camel_case() {
    let db = fts_table();
    db.execute(
        "INSERT INTO t(rowid, name_terms, body_terms) VALUES (1, 'turn_lock', ''), (2, 'TurnLock', '')",
        [],
    )
    .unwrap();
    // `_` is a separator, so both words of the snake_case form are searchable.
    assert_eq!(matches(&db, "\"turn\"").unwrap(), vec![1]);
    assert_eq!(matches(&db, "\"lock\"").unwrap(), vec![1]);
    // camelCase is one lowercased token: querying its parts misses it. Splitting identifiers
    // before insertion is therefore the indexer's job, not the tokenizer's.
    assert_eq!(matches(&db, "\"turnlock\"").unwrap(), vec![2]);
    assert!(!matches(&db, "\"turn\"").unwrap().contains(&2));
}

#[test]
fn porter_stemming_lets_parsing_find_parse() {
    let db = fts_table();
    db.execute(
        "INSERT INTO t(rowid, name_terms, body_terms) VALUES (1, 'parse', '')",
        [],
    )
    .unwrap();
    assert_eq!(matches(&db, "\"parsing\"").unwrap(), vec![1]);
}

#[test]
fn bm25_column_weights_favour_the_name_column() {
    let db = fts_table();
    db.execute_batch(
        "INSERT INTO t(rowid, name_terms, body_terms) VALUES
           (1, 'session', 'unrelated words here'),
           (2, 'other',   'session session session');",
    )
    .unwrap();
    // bm25() is lower-is-better; the weights are positional, name first.
    let first: i64 = db
        .query_row(
            "SELECT rowid FROM t WHERE t MATCH '\"session\"' ORDER BY bm25(t, 10.0, 1.0) LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first, 1, "a name hit must outrank a body-only hit");
}

fn quote_term(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

#[test]
fn raw_prompt_text_can_break_match_but_quoted_terms_cannot() {
    let db = fts_table();
    db.execute(
        "INSERT INTO t(rowid, name_terms, body_terms) VALUES (1, 'session', 'x')",
        [],
    )
    .unwrap();
    let hostile = ["\"", "NEAR(", "*", "-x", "a OR", "AND", "(", "x:y", "\"\""];

    let mut raw_errors = Vec::new();
    for input in hostile {
        if matches(&db, input).is_err() {
            raw_errors.push(input);
        }
    }
    // Recorded so the design's claim is the observed one, not the assumed one.
    println!("raw MATCH errors on: {raw_errors:?}");
    assert!(raw_errors.contains(&"\""), "an unbalanced quote must error");

    for input in hostile {
        assert!(
            matches(&db, &quote_term(input)).is_ok(),
            "quoted form of {input:?} must always be a valid expression"
        );
    }
}

const RUST_SAMPLE: &str = "//! Module doc.\n\npub struct Session { id: u32 }\n\nimpl Session {\n    /// Appends a turn.\n    pub async fn prompt(&mut self) -> u32 { 1 }\n}\n";
const TSX_SAMPLE: &str = "export const Panel = (props: { n: number }) => <div>{props.n}</div>;\nexport function f(a: string): string { return a; }\n";

fn parse(language: &tree_sitter::Language, source: &str) -> tree_sitter::Tree {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .expect("grammar is ABI-compatible");
    parser.parse(source, None).expect("parse")
}

#[test]
fn grammars_load_against_tree_sitter_0_27_and_parse_real_syntax() {
    let rust = parse(&tree_sitter_rust::LANGUAGE.into(), RUST_SAMPLE);
    assert!(!rust.root_node().has_error());
    let rust_sexp = rust.root_node().to_sexp();
    assert!(rust_sexp.contains("struct_item"), "{rust_sexp}");
    assert!(rust_sexp.contains("impl_item"), "{rust_sexp}");
    assert!(rust_sexp.contains("function_item"), "{rust_sexp}");

    let tsx = parse(&tree_sitter_typescript::LANGUAGE_TSX.into(), TSX_SAMPLE);
    assert!(!tsx.root_node().has_error());
    let tsx_sexp = tsx.root_node().to_sexp();
    assert!(tsx_sexp.contains("arrow_function"), "{tsx_sexp}");
    assert!(tsx_sexp.contains("jsx_element"), "{tsx_sexp}");

    let ts = parse(
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "export function f(a: string): string { return a; }\n",
    );
    assert!(!ts.root_node().has_error());
}

#[test]
fn identical_bytes_parse_differently_as_ts_and_tsx() {
    // The reason `typescript@1` and `tsx@1` are separate extractor ids: a type assertion is
    // valid TS and not valid TSX, so one content hash can need two extractions.
    let source = "const x = <string>y;\n";
    let ts = parse(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), source);
    let tsx = parse(&tree_sitter_typescript::LANGUAGE_TSX.into(), source);
    assert!(!ts.root_node().has_error());
    assert!(tsx.root_node().has_error());
}

#[test]
fn malformed_source_still_yields_a_tree_with_the_good_parts() {
    let tree = parse(
        &tree_sitter_rust::LANGUAGE.into(),
        "pub struct Ok1;\nfn broken( {{{\npub struct Ok2;\n",
    );
    let root = tree.root_node();
    assert!(root.has_error());
    let sexp = root.to_sexp();
    assert_eq!(sexp.matches("struct_item").count(), 2, "{sexp}");
}

#[test]
fn a_progress_callback_abandons_a_parse() {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let source = "fn a() {}\n".repeat(200_000);
    let mut calls = 0u32;
    let mut stop_immediately = |_: &tree_sitter::ParseState| {
        calls += 1;
        ControlFlow::Break(())
    };
    let bytes = source.as_bytes();
    let tree = parser.parse_with_options(
        &mut |offset, _| &bytes[offset.min(bytes.len())..],
        None,
        Some(ParseOptions::new().progress_callback(&mut stop_immediately)),
    );
    assert!(
        tree.is_none(),
        "a Break from the callback must abort the parse"
    );
    assert!(calls >= 1);
}

/// Splits `git log -z` output into NUL-separated tokens with the per-commit framing removed.
fn tokens(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .map(|t| {
            String::from_utf8_lossy(t)
                .trim_start_matches('\n')
                .to_owned()
        })
        .collect()
}

#[test]
fn git_name_status_with_rename_detection_has_the_shape_the_parser_assumes() {
    let mut repo = GitRepo::init();
    repo.write("src/a.rs", "a\n");
    repo.write("src/b.rs", "b\n");
    repo.commit("c1");
    repo.rename("src/a.rs", "src/c.rs");
    repo.write("src/b.rs", "b\nb2\n");
    repo.commit("c2");

    let raw = repo.git(&[
        "-c",
        "core.quotepath=false",
        "log",
        "--no-ext-diff",
        "--no-textconv",
        "--no-merges",
        "--name-status",
        "-M",
        "-z",
        "--format=%x00%H",
    ]);
    let t = tokens(&raw);
    let hashes: Vec<usize> = t
        .iter()
        .enumerate()
        .filter(|(i, tok)| {
            tok.len() == 40
                && tok.bytes().all(|b| b.is_ascii_hexdigit())
                && *i > 0
                && t[*i - 1].is_empty()
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hashes.len(), 2, "two commits: {t:?}");

    // Newest first. Its statuses follow the hash: `M b.rs`, then `R100 a.rs c.rs` — the rename
    // carries *two* paths, which is what a `--name-only` parser would silently lose.
    let newest = &t[hashes[0] + 1..hashes[1] - 1];
    assert_eq!(newest, ["M", "src/b.rs", "R100", "src/a.rs", "src/c.rs"]);
    let oldest = &t[hashes[1] + 1..];
    assert_eq!(&oldest[..4], ["A", "src/a.rs", "A", "src/b.rs"]);
}
