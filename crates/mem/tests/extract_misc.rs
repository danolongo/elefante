use std::time::Duration;

use elefante_mem::{
    Counters, RepoId, Store,
    extract::{
        MAX_SIGNATURE_CHARS,
        cache::{self, GC_MAX_AGE_NS},
        language_for,
    },
};

#[test]
fn only_known_extensions_have_an_extractor() {
    let table = [
        ("src/lib.rs", Some("rust@1")),
        ("a/b.ts", Some("typescript@1")),
        ("a/b.mts", Some("typescript@1")),
        ("types/a.d.ts", Some("typescript@1")),
        ("a/b.TSX", Some("tsx@1")),
        ("README.md", Some("markdown@1")),
        ("a.markdown", Some("markdown@1")),
        ("a.js", None),
        ("a.jsx", None),
        ("Makefile", None),
        ("a.rs.bak", None),
        (".rs", Some("rust@1")),
    ];
    for (path, expected) in table {
        assert_eq!(
            language_for(path).map(elefante_mem::extract::Language::id),
            expected,
            "{path}"
        );
    }
}

#[test]
fn exactly_one_parse_is_counted_per_extraction_and_none_for_markdown() {
    let counters = Counters::default();
    language_for("a.rs")
        .unwrap()
        .extract("fn a() {}", &counters);
    assert_eq!(counters.snapshot().parsed, 1);
    language_for("a.tsx")
        .unwrap()
        .extract("export function a() {}", &counters);
    assert_eq!(counters.snapshot().parsed, 2);
    language_for("a.md").unwrap().extract("# Title", &counters);
    assert_eq!(counters.snapshot().parsed, 2, "markdown needs no parser");
}

#[test]
fn a_parse_over_budget_is_abandoned_and_marked_truncated() {
    let source = "fn a() {}\n".repeat(200_000);
    for path in ["a.rs", "a.ts"] {
        let source = if path == "a.ts" {
            "export function a() {}\n".repeat(100_000)
        } else {
            source.clone()
        };
        let counters = Counters::default();
        let e = language_for(path)
            .unwrap()
            .extract_with_budget(&source, &counters, Duration::ZERO);
        assert!(e.truncated, "{path}");
        assert!(
            e.symbols.is_empty(),
            "{path}: an abandoned parse yields nothing"
        );
        assert_eq!(counters.snapshot().parsed, 1);
    }
}

#[test]
fn very_long_signatures_are_capped() {
    let params = (0..200)
        .map(|i| format!("argument_number_{i}: u32, "))
        .collect::<Vec<_>>()
        .concat();
    let e = language_for("a.rs")
        .unwrap()
        .extract(&format!("fn long({params}) {{}}"), &Counters::default());
    assert_eq!(e.symbols[0].signature.chars().count(), MAX_SIGNATURE_CHARS);
    assert!(e.symbols[0].signature.ends_with('…'));
}

#[test]
fn markdown_records_headings_outside_code_fences() {
    let source = "\
# Title

Some prose.

## Setup  ##

```sh
# not a heading, a shell comment
```

#hashtag is not a heading

~~~
## also inside a fence
~~~

### Usage
####### seven hashes is not a heading
";
    let e = language_for("README.md")
        .unwrap()
        .extract(source, &Counters::default());
    assert_eq!(e.file_doc, "Title · Setup · Usage");
    assert!(e.symbols.is_empty());
    assert!(!e.truncated);
}

#[test]
fn markdown_keeps_twelve_headings_and_flags_the_rest() {
    let source = (1..=15)
        .map(|i| {
            format!(
                "# H{i}
"
            )
        })
        .collect::<Vec<_>>()
        .concat();
    let e = language_for("a.md")
        .unwrap()
        .extract(&source, &Counters::default());
    assert_eq!(e.file_doc.matches('·').count(), 11);
    assert!(e.truncated);
}

#[test]
fn a_secret_in_a_heading_is_redacted() {
    let e = language_for("a.md").unwrap().extract(
        "# Key sk-ant-api03-ABCDEFGH12345678 rotation\n",
        &Counters::default(),
    );
    assert!(!e.file_doc.contains("sk-ant"));
    assert!(e.file_doc.contains("[REDACTED]"));
}

fn store() -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    let store = Store::open(&RepoId::resolve_with_home(tmp.path(), None).unwrap()).unwrap();
    (tmp, store)
}

#[test]
fn the_cache_round_trips_and_is_keyed_by_content_and_extractor() {
    let (_tmp, store) = store();
    let e = language_for("a.rs")
        .unwrap()
        .extract("pub fn a() {}", &Counters::default());
    let expected = e.clone();
    store
        .writer()
        .call_blocking(move |s| {
            let hash = [1u8; 32];
            assert!(
                cache::lookup(&s.cache, &hash, "rust@1", 10)
                    .unwrap()
                    .is_none()
            );
            cache::store(&s.cache, &hash, "rust@1", &e, 10).unwrap();
            assert_eq!(
                cache::lookup(&s.cache, &hash, "rust@1", 20).unwrap(),
                Some(expected)
            );
            // Same bytes, another extractor (TS vs TSX): a different row, so a miss.
            assert!(
                cache::lookup(&s.cache, &hash, "tsx@1", 20)
                    .unwrap()
                    .is_none()
            );
            // Another content: a miss.
            assert!(
                cache::lookup(&s.cache, &[2u8; 32], "rust@1", 20)
                    .unwrap()
                    .is_none()
            );
            // Storing again is harmless.
            cache::store(
                &s.cache,
                &hash,
                "rust@1",
                &cache::lookup(&s.cache, &hash, "rust@1", 30)
                    .unwrap()
                    .unwrap(),
                30,
            )
            .unwrap();
            let rows: i64 = s
                .cache
                .query_row("SELECT count(*) FROM extraction", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 1);
        })
        .unwrap();
}

#[test]
fn a_corrupt_cached_record_is_a_miss_and_is_removed() {
    let (_tmp, store) = store();
    store
        .writer()
        .call_blocking(|s| {
            s.cache
                .execute(
                    "INSERT INTO extraction VALUES (?1, 'rust@1', 'not json', 1)",
                    [[3u8; 32].as_slice()],
                )
                .unwrap();
            assert!(
                cache::lookup(&s.cache, &[3u8; 32], "rust@1", 5)
                    .unwrap()
                    .is_none()
            );
            let rows: i64 = s
                .cache
                .query_row("SELECT count(*) FROM extraction", [], |r| r.get(0))
                .unwrap();
            assert_eq!(rows, 0);
        })
        .unwrap();
}

#[test]
fn gc_removes_only_unreferenced_extractions_older_than_the_window() {
    let (_tmp, store) = store();
    let now = 100 * GC_MAX_AGE_NS;
    store
        .writer()
        .call_blocking(move |s| {
            let e = language_for("a.rs")
                .unwrap()
                .extract("fn a() {}", &Counters::default());
            for (byte, last_used) in [
                (1u8, 1),                       // old and unreferenced: collected
                (2u8, 1),                       // old but a live file points at it: kept
                (3u8, now - GC_MAX_AGE_NS / 2), // unreferenced but recent: kept
            ] {
                cache::store(&s.cache, &[byte; 32], "rust@1", &e, last_used).unwrap();
            }
            s.cache
                .execute(
                    "INSERT INTO file_state(path, size, mtime_ns, blake3, indexed_at_ns)
                     VALUES ('a.rs', 1, 1, ?1, 1)",
                    [[2u8; 32].as_slice()],
                )
                .unwrap();
            assert_eq!(cache::gc(&s.cache, now, GC_MAX_AGE_NS).unwrap(), 1);
            let kept: Vec<i64> = s
                .cache
                .prepare("SELECT substr(hex(blake3), 1, 2) FROM extraction ORDER BY blake3")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| i64::from_str_radix(&r.unwrap(), 16).unwrap())
                .collect();
            assert_eq!(kept, [2, 3]);
        })
        .unwrap();
}
