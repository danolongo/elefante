use elefante_guard::{PathPolicy, PolicyError};
use std::fs;

#[test]
fn resolves_safe_existing_and_new_paths() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    let policy = PathPolicy::new(temp.path()).unwrap();
    let canonical_root = temp.path().canonicalize().unwrap();
    assert_eq!(policy.resolve("src").unwrap(), canonical_root.join("src"));
    assert_eq!(
        policy.resolve("src/new.rs").unwrap(),
        canonical_root.join("src/new.rs")
    );
}

#[test]
fn denies_traversal_and_secret_variants() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("x")).unwrap();
    fs::write(temp.path().join(".env"), "SECRET=value").unwrap();
    let policy = PathPolicy::new(temp.path()).unwrap();

    assert!(matches!(
        policy.resolve("../outside"),
        Err(PolicyError::OutsideRoot { .. })
    ));
    for path in [
        "./x/../.env",
        ".env.local",
        "server.pem",
        "private.key",
        ".ssh/config",
        ".aws/credentials",
        ".elefante/credentials.toml",
        ".elefante/memory.key",
        ".elefante/memory.db",
        ".elefante/cache.db-wal",
        "sub/.elefante/anything",
        ".elefante",
        "sub/.elefante",
    ] {
        assert!(
            matches!(policy.resolve(path), Err(PolicyError::Denied { .. })),
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn denies_credentials_when_workspace_is_home_or_credential_directory() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join(".elefante");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("credentials.toml"), "fake-key").unwrap();
    std::os::unix::fs::symlink(".elefante/credentials.toml", root.path().join("alias")).unwrap();
    assert!(matches!(
        PathPolicy::new(root.path()).unwrap().resolve("alias"),
        Err(PolicyError::Denied { .. })
    ));
    assert!(matches!(
        PathPolicy::new(&dir).unwrap().resolve("credentials.toml"),
        Err(PolicyError::Denied { .. })
    ));
}

#[cfg(unix)]
#[test]
fn denies_canonical_credentials_target_with_isolated_home() {
    const CHILD: &str = "ELEFANTE_CREDENTIAL_POLICY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert!(matches!(
            PathPolicy::new(&root).unwrap().resolve("ordinary.toml"),
            Err(PolicyError::Denied { .. })
        ));
        assert_eq!(
            elefante_guard::BashClassifier::new()
                .classify_in("sed s/fake/changed/ ordinary.toml", &root)
                .verdict,
            elefante_guard::Verdict::Deny
        );
        return;
    }
    // A subprocess isolates HOME without unsafe process-wide environment mutation.
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".elefante")).unwrap();
    fs::write(
        root.path().join("ordinary.toml"),
        "fake-secret-outside-scrubber-patterns",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        "../ordinary.toml",
        root.path().join(".elefante/credentials.toml"),
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "denies_canonical_credentials_target_with_isolated_home",
        ])
        .env("HOME", root.path())
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(unix)]
#[test]
fn denies_symlink_alias_to_secret_and_escape() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join(".env"), "SECRET=value").unwrap();
    symlink(temp.path().join(".env"), temp.path().join("alias")).unwrap();
    let policy = PathPolicy::new(temp.path()).unwrap();
    assert!(matches!(
        policy.resolve("alias"),
        Err(PolicyError::Denied { .. })
    ));

    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), temp.path().join("escape")).unwrap();
    assert!(matches!(
        policy.resolve("escape/file"),
        Err(PolicyError::OutsideRoot { .. })
    ));
}

#[cfg(unix)]
#[test]
fn denies_secret_name_even_when_symlink_target_is_safe() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("safe.txt"), "ordinary").unwrap();
    symlink(temp.path().join("safe.txt"), temp.path().join(".env")).unwrap();
    let policy = PathPolicy::new(temp.path()).unwrap();
    assert!(matches!(
        policy.resolve(".env"),
        Err(PolicyError::Denied { .. })
    ));
}

#[cfg(unix)]
#[test]
fn rejects_dangling_symlink_ancestor() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path().join("missing"), temp.path().join("link")).unwrap();
    let policy = PathPolicy::new(temp.path()).unwrap();
    assert!(matches!(
        policy.resolve("link/file.txt"),
        Err(PolicyError::Resolve { .. })
    ));
}

#[test]
fn supports_custom_denied_globs() {
    let temp = tempfile::tempdir().unwrap();
    let policy = PathPolicy::with_denied_patterns(temp.path(), ["private/**"]).unwrap();
    assert!(matches!(
        policy.resolve("private/note.txt"),
        Err(PolicyError::Denied {
            rule: "custom-pattern",
            ..
        })
    ));
}

#[test]
fn lexical_secret_check_agrees_with_resolve_on_names() {
    use std::path::Path;
    for path in [
        ".env",
        ".env.local",
        "app/.env.production",
        "server.pem",
        "deep/dir/private.KEY",
        ".ssh/config",
        "home/.aws/credentials",
        ".elefante/credentials.toml",
        ".elefante/memory.db",
        ".elefante",
        "sub/.elefante",
        "id_rsa",
        "id_rsa.pub",
        ".netrc",
        ".npmrc",
        ".git-credentials",
    ] {
        assert!(PathPolicy::is_secret_path(Path::new(path)), "{path}");
    }
    for path in [
        "src/lib.rs",
        "environment.rs",
        ".envrc.md.bak/readme.txt",
        "docs/keys.md",
        "monkey.rs",
        ".github/workflows/ci.yml",
        ".elefanteish/x",
        ".aws/config",
    ] {
        // `.envrc.md.bak` is a directory name that merely begins with `.env`; it does not match
        // `.env` exactly nor `.env.` prefix, so it must stay indexable.
        assert!(!PathPolicy::is_secret_path(Path::new(path)), "{path}");
    }
}

#[test]
fn workspace_under_an_elefante_directory_still_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".elefante/proj");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "fn main() {}").unwrap();
    let policy = PathPolicy::new(&root).unwrap();
    let canonical_root = root.canonicalize().unwrap();
    assert_eq!(
        policy.resolve("src/a.rs").unwrap(),
        canonical_root.join("src/a.rs")
    );
    assert_eq!(
        policy.resolve("src/new.rs").unwrap(),
        canonical_root.join("src/new.rs")
    );
    // Harness state inside that workspace, and secrets by name, are still denied.
    for path in [".elefante/memory.db", ".elefante", ".env", "../x"] {
        assert!(policy.resolve(path).is_err(), "{path}");
    }
}

#[cfg(unix)]
#[test]
fn denies_symlink_into_elefante_inside_workspace() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".elefante")).unwrap();
    fs::write(root.path().join(".elefante/memory.db"), "notes").unwrap();
    std::os::unix::fs::symlink(".elefante/memory.db", root.path().join("alias")).unwrap();
    std::os::unix::fs::symlink(".elefante", root.path().join("dir")).unwrap();
    let policy = PathPolicy::new(root.path()).unwrap();
    for path in ["alias", "dir", "dir/memory.db", "x/../.elefante/memory.db"] {
        assert!(
            matches!(policy.resolve(path), Err(PolicyError::Denied { .. })),
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn denies_user_level_harness_directory_by_prefix() {
    const CHILD: &str = "ELEFANTE_HOME_HARNESS_POLICY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        // A workspace inside the user-level harness directory is denied, and so are absolute
        // paths into it from a workspace elsewhere, whether or not the file exists.
        let policy = PathPolicy::new(&home).unwrap();
        for path in [".elefante/memory.db", ".elefante/memory.key", ".elefante"] {
            assert!(
                matches!(policy.resolve(path), Err(PolicyError::Denied { .. })),
                "{path}"
            );
        }
        let inside = PathPolicy::new(home.join(".elefante/proj")).unwrap();
        assert!(matches!(
            inside.resolve("src/a.rs"),
            Err(PolicyError::Denied { .. })
        ));
        let other = PathPolicy::new(home.join("work")).unwrap();
        assert!(other.resolve("src/a.rs").is_ok());
        let classifier = elefante_guard::BashClassifier::new();
        let work = home.join("work");
        for command in [
            "cat ~/.elefante/memory.db",
            "cat $HOME/.elefante/memory.key",
            "ls ~/.elefante",
            "cat ../.elefante/memory.db",
        ] {
            assert_eq!(
                classifier.classify_in(command, &work).verdict,
                elefante_guard::Verdict::Deny,
                "{command}"
            );
        }
        return;
    }
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".elefante/proj")).unwrap();
    fs::create_dir_all(home.path().join("work/src")).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "denies_user_level_harness_directory_by_prefix"])
        .env("HOME", home.path())
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
