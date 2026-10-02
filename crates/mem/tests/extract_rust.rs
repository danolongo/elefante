use elefante_mem::{
    Counters,
    extract::{Extraction, language_for},
};

fn extract(source: &str) -> Extraction {
    language_for("x.rs")
        .unwrap()
        .extract(source, &Counters::default())
}

type Row<'a> = (
    u32,
    u32,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
    Option<&'a str>,
);

fn rows(e: &Extraction) -> Vec<Row<'_>> {
    e.symbols
        .iter()
        .map(|s| {
            (
                s.line,
                s.end_line,
                s.kind.as_str(),
                s.qname.as_str(),
                s.signature.as_str(),
                s.doc.as_str(),
                s.via_trait.as_deref(),
            )
        })
        .collect()
}

// The expected table is the point of this test; splitting it would only hide it.
#[allow(clippy::too_many_lines)]
#[test]
fn extracts_every_item_form_with_its_signature_doc_and_lines() {
    let e = extract(include_str!("fixtures/sample.rs"));
    assert_eq!(e.file_doc, "Provider-neutral agent loop.");
    assert!(!e.truncated);
    let expected: Vec<Row> = vec![
        (
            13,
            13,
            "const",
            "SYSTEM_PROMPT",
            "pub const SYSTEM_PROMPT: &str",
            "The stable standing instructions.",
            None,
        ),
        (
            15,
            15,
            "static",
            "COUNTER",
            "pub static COUNTER: AtomicU64",
            "",
            None,
        ),
        (
            18,
            18,
            "type",
            "ModelId",
            "pub type ModelId",
            "A model identifier.",
            None,
        ),
        (
            22,
            24,
            "struct",
            "Session",
            "pub struct Session",
            "One loop invocation.",
            None,
        ),
        (26, 26, "struct", "Unit", "pub struct Unit", "", None),
        (27, 27, "struct", "Pair", "pub struct Pair", "", None),
        (
            30,
            33,
            "enum",
            "StopReason",
            "pub enum StopReason",
            "Why a turn ended.",
            None,
        ),
        (
            40,
            46,
            "method",
            "Session::prompt",
            "pub async fn prompt<F>(&mut self, provider: &dyn Provider, text: impl Into<String>) -> Result<(), E> where F: FnMut()",
            "Appends one user turn and retains its history.",
            None,
        ),
        (
            48,
            48,
            "method",
            "Session::private",
            "fn private(&self)",
            "",
            None,
        ),
        (
            52,
            52,
            "method",
            "Wrapper::stream",
            "fn stream(&self)",
            "",
            Some("Provider"),
        ),
        (
            56,
            60,
            "trait",
            "Gate",
            "pub trait Gate",
            "Decides whether a call may run.",
            None,
        ),
        (
            58,
            58,
            "method",
            "Gate::check",
            "fn check(&self) -> bool",
            "Required method.",
            None,
        ),
        (
            59,
            59,
            "method",
            "Gate::default_method",
            "fn default_method(&self)",
            "",
            None,
        ),
        (62, 68, "mod", "inner", "pub mod inner", "", None),
        (63, 63, "fn", "inner::helper", "pub fn helper()", "", None),
        (64, 64, "struct", "inner::Deep", "pub struct Deep", "", None),
        (
            66,
            66,
            "method",
            "inner::Deep::new",
            "pub fn new() -> Self",
            "",
            None,
        ),
        (70, 72, "macro", "bump", "macro_rules! bump", "", None),
        (
            74,
            74,
            "fn",
            "free_function",
            "fn free_function(a: u32) -> u32",
            "",
            None,
        ),
        (78, 78, "fn", "after_blank", "fn after_blank()", "", None),
    ];
    assert_eq!(rows(&e), expected);
}

#[test]
fn use_trees_flatten_to_full_paths_and_mod_declarations_are_imports() {
    let e = extract(include_str!("fixtures/sample.rs"));
    let imports: Vec<(&str, &str)> = e
        .imports
        .iter()
        .map(|i| (i.kind.as_str(), i.spec.as_str()))
        .collect();
    assert_eq!(
        imports,
        [
            ("use", "std::collections::HashMap"),
            ("use", "std::sync::Arc"),
            ("use", "crate::sse::SseDecoder"),
            ("use", "crate::sse::frame::Frame"),
            ("use", "super::util"),
            ("use", "self::inner::helper"),
            ("mod", "sse"),
            ("mod", "openrouter"),
        ]
    );
}

#[test]
fn items_nested_inside_function_bodies_are_not_symbols() {
    let e = extract(include_str!("fixtures/sample.rs"));
    assert!(e.symbols.iter().all(|s| s.name != "nested_helper"));
}

#[test]
fn a_comment_separated_by_a_blank_line_is_not_a_doc() {
    let e = extract("/// Orphan.\n\nfn f() {}\n/// Attached.\nfn g() {}\n");
    assert_eq!(e.symbols[0].doc, "");
    assert_eq!(e.symbols[1].doc, "Attached.");
}

#[test]
fn const_initialisers_and_string_literals_never_reach_the_record() {
    let e = extract(
        "pub const KEY: &str = \"sk-ant-api03-ABCDEFGH12345678\";\n\
         pub fn f(token: &str) -> &'static str { \"sk-ant-api03-ZZZZZZZZ\" }\n\
         pub static TOKEN: &str = \"hunter2hunter2\";\n",
    );
    let json = serde_json::to_string(&e).unwrap();
    for needle in ["sk-ant", "hunter2", "ABCDEFGH"] {
        assert!(!json.contains(needle), "{needle} leaked into {json}");
    }
    assert_eq!(e.symbols[0].signature, "pub const KEY: &str");
    assert_eq!(e.symbols[2].signature, "pub static TOKEN: &str");
}

#[test]
fn a_secret_in_a_doc_comment_is_redacted() {
    let e = extract(
        "//! Uses sk-ant-api03-ABCDEFGH12345678 internally.\n/// Try sk-ant-api03-ABCDEFGH12345678 now.\nfn f() {}\n",
    );
    let json = serde_json::to_string(&e).unwrap();
    assert!(!json.contains("sk-ant"), "{json}");
    assert!(e.file_doc.contains("[REDACTED]"));
    assert!(e.symbols[0].doc.contains("[REDACTED]"));
}

#[test]
fn malformed_source_keeps_the_symbols_that_did_parse() {
    let e = extract("pub struct Before;\nfn broken( {{{\npub struct After;\n");
    let names: Vec<&str> = e.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"Before") && names.contains(&"After"),
        "{names:?}"
    );
}

#[test]
fn impl_targets_are_reduced_to_a_type_name() {
    let e = extract(
        "impl<T> Foo<T> { fn a(&self) {} }\n\
         impl Bar for &mut Baz {}\n\
         impl<'a> Trait for &'a a::b::Qux<u8> { fn b(&self) {} }\n",
    );
    let q: Vec<&str> = e.symbols.iter().map(|s| s.qname.as_str()).collect();
    assert_eq!(q, ["Foo::a", "Qux::b"]);
}

#[test]
fn an_empty_file_yields_an_empty_record() {
    let e = extract("");
    assert!(e.symbols.is_empty() && e.imports.is_empty() && e.file_doc.is_empty());
    assert_eq!(e.v, 1);
}

#[test]
fn the_symbol_cap_truncates_and_says_so() {
    let source = (0..2_500)
        .map(|i| format!("fn f{i}() {{}}\n"))
        .collect::<Vec<_>>()
        .concat();
    let e = extract(&source);
    assert_eq!(e.symbols.len(), elefante_mem::extract::MAX_SYMBOLS);
    assert!(e.truncated);
}
