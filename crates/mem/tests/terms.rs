use elefante_mem::terms::{identifier_terms, path_terms, split_words};

fn words(input: &str) -> Vec<String> {
    split_words(input)
}

#[test]
fn splits_the_identifier_shapes_found_in_real_code() {
    let table: &[(&str, &[&str])] = &[
        ("TurnLock", &["turn", "lock"]),
        ("turn_lock", &["turn", "lock"]),
        ("turnLock", &["turn", "lock"]),
        ("TURN_LOCK", &["turn", "lock"]),
        ("HTTPServer", &["http", "server"]),
        ("parseHTTPResponse", &["parse", "http", "response"]),
        ("XMLHttpRequest", &["xml", "http", "request"]),
        ("IOError", &["io", "error"]),
        ("getURL", &["get", "url"]),
        ("ABC", &["abc"]),
        ("v2Api", &["v2", "api"]),
        ("Foo2Bar", &["foo2", "bar"]),
        ("sha256sum", &["sha256sum"]),
        ("abc123Def", &["abc123", "def"]),
        ("__init__", &["init"]),
        ("foo.bar-baz", &["foo", "bar", "baz"]),
        ("a", &["a"]),
        ("", &[]),
        ("___", &[]),
        ("Straße", &["straße"]),
        ("日本語Name", &["日本語", "name"]),
    ];
    for (input, expected) in table {
        assert_eq!(words(input), *expected, "{input:?}");
    }
}

#[test]
fn identifier_terms_adds_the_joined_form_only_for_multiword_names() {
    assert_eq!(identifier_terms("TurnLock"), "turn lock turnlock");
    assert_eq!(identifier_terms("turn_lock"), "turn lock turnlock");
    assert_eq!(identifier_terms("session"), "session");
    assert_eq!(identifier_terms(""), "");
}

#[test]
fn path_terms_cover_every_component_without_repeats() {
    assert_eq!(
        path_terms("crates/core/src/lib.rs"),
        "crates core src lib rs"
    );
    assert_eq!(
        path_terms("crates/mem/src/mem_store.rs"),
        "crates mem src store memstore rs"
    );
    assert_eq!(
        path_terms(".github/workflows/ci.yml"),
        "github workflows ci yml"
    );
    assert_eq!(path_terms("Cargo.toml"), "cargo toml");
    assert_eq!(path_terms(""), "");
}
