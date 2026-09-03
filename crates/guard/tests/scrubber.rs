use elefante_guard::Scrubber;

#[test]
fn scrubs_all_supported_secret_shapes() {
    let input = concat!(
        "key sk-ant-api03_abcdefghijklmnopqrstuvwxyz\n",
        "router sk-or-v1-0123456789abcdef0123456789abcdef\n",
        "aws AKIAABCDEFGHIJKLMNOP\n",
        "API_TOKEN=top-secret\n",
        "db_password = hunter2\n",
        "QUOTED_SECRET=\"two words remain private\"\n",
        "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
    );
    let scrubbed = Scrubber::new().scrub(input);
    for secret in [
        "sk-ant-",
        "sk-or-v1-",
        "AKIAABCDEFGHIJKLMNOP",
        "top-secret",
        "hunter2",
        "two words remain private",
        "abc",
    ] {
        assert!(!scrubbed.contains(secret), "secret survived: {secret}");
    }
    assert!(scrubbed.contains("API_TOKEN=[REDACTED]"));
    assert!(scrubbed.contains("[REDACTED PEM]"));
}

#[test]
fn scrubs_openrouter_keys_without_dropping_anthropic_keys() {
    let scrubbed = Scrubber::new().scrub(concat!(
        "OPENROUTER_API_KEY: sk-or-v1-0123456789abcdef0123456789abcdef\n",
        "ANTHROPIC_API_KEY: sk-ant-api03_abcdefghijklmnopqrstuvwxyz\n",
    ));
    assert!(!scrubbed.contains("sk-or-v1-"), "openrouter key survived");
    assert!(!scrubbed.contains("sk-ant-"), "anthropic key survived");
    assert_eq!(scrubbed.matches("[REDACTED]").count(), 2);
}

#[test]
fn leaves_normal_output_unchanged() {
    let value = "cargo test: 12 passed";
    assert_eq!(Scrubber::new().scrub(value), value);
}
