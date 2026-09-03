use elefante_guard::{BashClassifier, Verdict};

#[test]
fn classifies_full_fixture_table() {
    let classifier = BashClassifier::new();
    let fixtures = [
        ("pwd", Verdict::Allow),
        ("ls -la", Verdict::Allow),
        ("cargo test", Verdict::Allow),
        ("git status", Verdict::Allow),
        ("touch new-file", Verdict::Escalate),
        ("python script.py", Verdict::Escalate),
        ("rm file", Verdict::Deny),
        ("git reset --hard", Verdict::Deny),
        ("cat .env", Verdict::Deny),
        ("source .env.local", Verdict::Deny),
        ("env", Verdict::Deny),
        ("printenv PATH", Verdict::Deny),
        ("ls && rm -rf /", Verdict::Deny),
        ("ls || rm -rf /", Verdict::Deny),
        ("printf ok | rm file", Verdict::Deny),
        ("(pwd; rm file)", Verdict::Deny),
        ("echo $(rm file)", Verdict::Deny),
        ("echo `rm file`", Verdict::Deny),
        ("echo changed > file", Verdict::Escalate),
        ("printf changed >> file", Verdict::Escalate),
        ("echo changed >| file", Verdict::Escalate),
        ("echo changed 2> file", Verdict::Escalate),
        ("echo changed 2>> file", Verdict::Escalate),
        ("echo changed &> file", Verdict::Escalate),
        ("echo changed &>> file", Verdict::Escalate),
        ("echo changed 2>&1", Verdict::Escalate),
        ("cat <> file", Verdict::Escalate),
        ("(echo changed > file)", Verdict::Escalate),
        ("echo $(printf changed >> file)", Verdict::Escalate),
        ("cat < .env", Verdict::Deny),
        ("printf changed > .env", Verdict::Deny),
        ("cat <<< safe", Verdict::Allow),
        ("cat <<'EOF'\nsafe\nEOF", Verdict::Allow),
    ];
    for (command, expected) in fixtures {
        assert_eq!(classifier.classify(command).verdict, expected, "{command}");
    }
}

#[test]
fn documents_known_not_caught_obfuscation() {
    let classifier = BashClassifier::new();
    // Dynamic dispatch is held for approval, but argument-level semantics of allowlisted
    // programs remain a known gap; the scrubber is still mandatory after execution.
    assert_ne!(
        classifier.classify("alias wipe='rm -rf'; wipe /").verdict,
        Verdict::Allow
    );
    assert_ne!(
        classifier.classify("eval \"r\"\"m file\"").verdict,
        Verdict::Allow
    );
    for known_not_caught in ["grep SECRET .e''nv", "sed -n 1p .e''nv"] {
        assert_eq!(
            classifier.classify(known_not_caught).verdict,
            Verdict::Allow,
            "known-not-caught fixture changed: {known_not_caught}"
        );
    }
    assert!(!BashClassifier::known_gaps().is_empty());
}
