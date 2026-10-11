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
        ("cat ~/.elefante/memory.key", Verdict::Deny),
        ("xxd .elefante/memory.key", Verdict::Deny),
        ("printf changed > .env", Verdict::Deny),
        ("cat <<< safe", Verdict::Allow),
        ("cat <<'EOF'\nsafe\nEOF", Verdict::Allow),
        ("cat .elefante", Verdict::Deny),
        ("ls ./.elefante/", Verdict::Deny),
        ("rg --hidden x", Verdict::Escalate),
        ("grep -rn x src/", Verdict::Allow),
        ("rg x", Verdict::Allow),
        ("find . -name '*.rs'", Verdict::Allow),
        ("ls -R", Verdict::Allow),
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
    for known_not_caught in ["grep SECRET \"$secret_path\"", "sed -n 1p \"$secret_path\""] {
        assert_eq!(
            classifier.classify(known_not_caught).verdict,
            Verdict::Allow,
            "known-not-caught fixture changed: {known_not_caught}"
        );
    }
    assert_ne!(BashClassifier::known_gaps().len(), 0);
}

#[test]
fn denies_credentials_for_readers_transformers_and_redirects() {
    for command in [
        "cat ~/.elefante/credentials.toml",
        "head $HOME/.elefante/credentials.toml",
        "sed 's/sk-ant/changed/' ${HOME}/.elefante/credentials.toml",
        "tr a b < '.elefante/credentials.toml'",
        "base64 .elefante/credentials.toml",
        "sqlite3 .elefante/memory.db '.schema'",
        "cat .elefante/memory.db",
        "grep -c note ./.elefante/cache.db",
        "grep SECRET .e''nv",
        "sed -n 1p .e''nv",
    ] {
        assert_eq!(
            BashClassifier::new().classify(command).verdict,
            Verdict::Deny,
            "{command}"
        );
    }
}

#[cfg(unix)]
#[test]
fn denies_workspace_relative_symlink_to_credentials() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".elefante")).unwrap();
    std::fs::write(root.path().join(".elefante/credentials.toml"), "fake-key").unwrap();
    std::os::unix::fs::symlink(".elefante/credentials.toml", root.path().join("alias")).unwrap();
    for command in ["head alias", "sed s/fake/changed/ alias", "tr a b < alias"] {
        assert_eq!(
            BashClassifier::new()
                .classify_in(command, root.path())
                .verdict,
            Verdict::Deny
        );
    }
}

#[test]
fn denies_final_component_and_glob_spellings_of_harness_state() {
    for command in [
        "cat .elefante",
        "grep -ra statement .elefante",
        "grep -ra statement ./.elefante/",
        "ls -a .elefante",
        "cat .elef*/memory.db",
        "cat .elefant?/memory.db",
        "cat .[e]lefante/memory.db",
        "cat ./.[e]lefante/memory.db",
        "cat .*/memory.db",
        "sqlite3 .elef*/memory.db .schema",
    ] {
        assert_eq!(
            BashClassifier::new().classify(command).verdict,
            Verdict::Deny,
            "{command}"
        );
    }
}

#[test]
fn glob_check_leaves_ordinary_patterns_alone() {
    for command in [
        "grep '.*' Cargo.toml",
        "sed -n 's/.*foo//p' Cargo.toml",
        "grep -E '.[a-z]+' Cargo.toml",
        "find . -name '.*'",
        "ls src/*.rs",
        "cat .git*",
    ] {
        assert_eq!(
            BashClassifier::new().classify(command).verdict,
            Verdict::Allow,
            "{command}"
        );
    }
}

#[test]
fn escalates_recursive_content_readers_over_the_workspace_root() {
    let classifier = BashClassifier::new();
    for command in [
        "grep -r x .",
        "grep -r x ./",
        "grep -rn x",
        "grep -rn x .",
        "grep -ra x",
        "grep -R x .",
        "grep --recursive x .",
        "grep -d recurse x .",
        "grep --directories=recurse x .",
        "grep -r --include='*.rs' x .",
        "grep -r -e x",
        "grep -r x . src/",
        "egrep -r x .",
        "fgrep -r x .",
        "grep -r x ..",
        "grep -r x ~",
        "rg -u x",
        "rg -uu x .",
        "rg -uuu x",
        "rg --no-ignore x",
        "rg --no-ignore-vcs x .",
        "rg --hidden x",
        "rg -g '*.rs' -uuu x",
        "rg --max-depth 3 -uuu x",
    ] {
        let decision = classifier.classify(command);
        assert_eq!(decision.verdict, Verdict::Escalate, "{command}");
        assert_eq!(
            decision.rule, "recursive-search-includes-harness-state",
            "{command}"
        );
    }
    let root = tempfile::tempdir().unwrap();
    let absolute = format!("grep -r x {}", root.path().display());
    assert_eq!(
        classifier.classify_in(&absolute, root.path()).rule,
        "recursive-search-includes-harness-state"
    );
}

#[test]
fn recursive_readers_stay_allowed_when_they_cannot_reach_harness_state() {
    let classifier = BashClassifier::new();
    for command in [
        "grep -rn x src/",
        "grep -rn x src/ tests/",
        "grep -r x ./src",
        "grep -rn --include='*.rs' x crates/guard",
        "grep -rn x . --exclude-dir=.elefante",
        "grep -rn x --exclude-dir .elefante .",
        "grep -rn --exclude-dir=.git --exclude-dir=.elefante x",
        "grep x Cargo.toml",
        "grep -n x",
        "grep -erun src/",
        "rg x",
        "rg -n x src/",
        "rg -uuu x src/",
        "rg -r replacement x",
        "rg -e x",
        "find . -name '*.rs'",
        "ls -R",
        "ls -R .",
        "cargo test",
    ] {
        assert_eq!(
            classifier.classify(command).verdict,
            Verdict::Allow,
            "{command}"
        );
    }
}

#[test]
fn documents_remaining_harness_state_gaps() {
    let classifier = BashClassifier::new();
    // None of these name the protected path in the command text, so the guard cannot see it. The
    // guard reduces accidental reads; it is not a sandbox.
    for (command, gap, verdict) in [
        (
            "D=.elefante; cat $D/memory.db",
            "shell variables",
            Verdict::Allow,
        ),
        ("cd \"$D\" && cat memory.db", "`cd` into", Verdict::Escalate),
        ("cat memory.db", "`cd` into", Verdict::Allow),
        (
            "sqlite3 \"$DB\" .schema",
            "interpreters and `sqlite3`",
            Verdict::Escalate,
        ),
        (
            "python -c 'import os; open(os.environ[\"P\"])'",
            "interpreters and `sqlite3`",
            Verdict::Escalate,
        ),
        (
            "echo \"$P\" | xargs cat",
            "`xargs` or command substitution",
            Verdict::Escalate,
        ),
        (
            "cat $(printf '%s' \"$P\")",
            "`xargs` or command substitution",
            Verdict::Allow,
        ),
    ] {
        assert!(
            BashClassifier::known_gaps()
                .iter()
                .any(|known| known.contains(gap)),
            "{gap} is not listed in known_gaps()"
        );
        assert_eq!(
            classifier.classify(command).verdict,
            verdict,
            "known-not-caught fixture changed: {command}"
        );
    }
}

#[test]
fn workspace_under_an_elefante_directory_classifies_normally() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".elefante/proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let classifier = BashClassifier::new();
    for command in [
        "ls src",
        "cat src/a.rs",
        "cargo test",
        "grep -rn x src/",
        &format!("cat {}/src/a.rs", root.canonicalize().unwrap().display()),
    ] {
        assert_eq!(
            classifier.classify_in(command, &root).verdict,
            Verdict::Allow,
            "{command}"
        );
    }
    for command in [
        "cat .elefante/memory.db",
        "cat .elefante",
        "cat ../memory.db.elefante/../../.elefante/x",
        "cat ~/.elefante/memory.db",
    ] {
        assert_eq!(
            classifier.classify_in(command, &root).verdict,
            Verdict::Deny,
            "{command}"
        );
    }
    // `grep -r x .` still reaches the workspace's own `.elefante/`.
    assert_eq!(
        classifier.classify_in("grep -r x .", &root).verdict,
        Verdict::Escalate
    );
}

#[cfg(unix)]
#[test]
fn denies_symlinks_into_elefante_from_a_nested_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".elefante/proj");
    std::fs::create_dir_all(root.join(".elefante")).unwrap();
    std::fs::write(root.join(".elefante/memory.db"), "notes").unwrap();
    std::os::unix::fs::symlink(".elefante/memory.db", root.join("alias")).unwrap();
    std::os::unix::fs::symlink(".elefante", root.join("dir")).unwrap();
    for command in ["cat alias", "ls dir", "cat dir/memory.db"] {
        assert_eq!(
            BashClassifier::new().classify_in(command, &root).verdict,
            Verdict::Deny,
            "{command}"
        );
    }
}
