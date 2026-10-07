use elefante_mem::{
    Counters,
    extract::{Extraction, language_for},
};

fn extract(path: &str, source: &str) -> Extraction {
    language_for(path)
        .unwrap()
        .extract(source, &Counters::default())
}

type Row<'a> = (u32, u32, &'a str, &'a str, &'a str, &'a str);

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
            )
        })
        .collect()
}

// The expected table is the point of this test; splitting it would only hide it.
#[allow(clippy::too_many_lines)]
#[test]
fn extracts_declarations_methods_and_function_valued_constants() {
    let e = extract("x.tsx", include_str!("fixtures/sample.tsx"));
    assert_eq!(e.file_doc, "Terminal client entry.");
    assert!(!e.truncated);
    let expected: Vec<Row> = vec![
        (
            13,
            13,
            "function",
            "Panel",
            "export const Panel = (props: { n: number; token?: string }): JSX.Element",
            "Renders one turn.",
        ),
        (
            15,
            15,
            "function",
            "withDefault",
            "export const withDefault = (key = \"…\", n = 3)",
            "",
        ),
        (
            17,
            17,
            "function",
            "helper",
            "const helper = function inner(a: string): string",
            "",
        ),
        (
            23,
            25,
            "function",
            "connect",
            "export async function connect<T extends object>(path: string, opts?: T): Promise<void>",
            "Connects to the daemon.",
        ),
        (
            27,
            31,
            "interface",
            "Transport",
            "export interface Transport",
            "",
        ),
        (
            29,
            29,
            "method",
            "Transport::send",
            "send(frame: Frame): void",
            "Sends a frame.",
        ),
        (33, 33, "type", "Handler", "export type Handler<T>", ""),
        (35, 35, "enum", "Mode", "export enum Mode", ""),
        (
            38,
            44,
            "class",
            "Session",
            "export abstract class Session<T> extends Base implements Transport",
            "A session.",
        ),
        (
            41,
            41,
            "method",
            "Session::start",
            "async start(opts: Options = { key: \"…\" }): Promise<void>",
            "Starts it.",
        ),
        (
            42,
            42,
            "method",
            "Session::create",
            "static create(): Session<any>",
            "",
        ),
        (
            43,
            43,
            "method",
            "Session::close",
            "abstract close(): void",
            "",
        ),
        (46, 48, "namespace", "Util", "namespace Util", ""),
        (
            47,
            47,
            "function",
            "Util::clamp",
            "export function clamp(n: number): number",
            "",
        ),
        (
            50,
            50,
            "function",
            "legacy",
            "declare function legacy(x: number): void",
            "",
        ),
    ];
    assert_eq!(rows(&e), expected);
}

#[test]
fn imports_and_reexports_are_recorded_with_their_kind() {
    let e = extract("x.tsx", include_str!("fixtures/sample.tsx"));
    let imports: Vec<(&str, &str)> = e
        .imports
        .iter()
        .map(|i| (i.kind.as_str(), i.spec.as_str()))
        .collect();
    assert_eq!(
        imports,
        [
            ("import", "react"),
            ("import", "./protocol"),
            ("reexport", "./reducer"),
            ("reexport", "./client"),
        ]
    );
}

#[test]
fn non_function_constants_and_their_literals_are_skipped_entirely() {
    let e = extract("x.tsx", include_str!("fixtures/sample.tsx"));
    assert!(
        e.symbols
            .iter()
            .all(|s| s.name != "API_KEY" && s.name != "LIMIT")
    );
    let json = serde_json::to_string(&e).unwrap();
    assert!(!json.contains("sk-ant"), "{json}");
}

#[test]
fn string_defaults_in_parameters_are_elided_in_every_quote_style() {
    let e = extract(
        "x.ts",
        "export function f(a = 'sk-ant-aaaaaaaaaaaa', b = \"sk-ant-bbbbbbbbbbbb\", c = `sk-ant-cccccccccccc`) {}\n",
    );
    assert_eq!(
        e.symbols[0].signature,
        "export function f(a = '…', b = \"…\", c = `…`)"
    );
}

#[test]
fn jsdoc_tags_end_the_description() {
    let e = extract(
        "x.ts",
        "/**\n * Does the thing.\n * @param x ignored\n */\nexport function f(x: number) {}\n",
    );
    assert_eq!(e.symbols[0].doc, "Does the thing.");
}

#[test]
fn a_leading_comment_attached_to_code_is_not_a_file_doc() {
    let attached = extract("x.ts", "/** Describes f. */\nexport function f() {}\n");
    assert_eq!(attached.file_doc, "");
    assert_eq!(attached.symbols[0].doc, "Describes f.");
}

#[test]
fn ts_and_tsx_are_separate_grammars_with_separate_ids() {
    let ts = language_for("a.ts").unwrap();
    let tsx = language_for("a.tsx").unwrap();
    assert_eq!(ts.id(), "typescript@1");
    assert_eq!(tsx.id(), "tsx@1");
    // `<T>x` is a type assertion in TS and a JSX element in TSX: a function after it only
    // parses cleanly in one of them.
    let source = "const a = <string>b;\nexport function after() {}\n";
    let in_ts = ts.extract(source, &Counters::default());
    assert!(in_ts.symbols.iter().any(|s| s.name == "after"));
}

#[test]
fn malformed_source_keeps_the_symbols_that_did_parse() {
    let e = extract(
        "x.ts",
        "export function before() {}\nexport function broken( {{{\nexport function after() {}\n",
    );
    let names: Vec<&str> = e.symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"before"), "{names:?}");
}
