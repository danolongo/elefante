//! Harness git under its own sandbox profile. `GitWrite::unchecked` runs commands the router
//! refuses, so each test shows that the OS confines them, with a positive control beside it.
use elefante_core::harness_git::HarnessGit;
use elefante_core::sandbox::SessionSandbox;
use elefante_core::tools::ToolExecutor;
use elefante_guard::git::{GitWrite, Identity};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

fn identity() -> Identity {
    Identity {
        name: "Fixture".into(),
        email: "fixture@example.invalid".into(),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("/usr/bin/git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=f@example.invalid",
        ])
        .args(["-c", "commit.gpgSign=false"])
        .args(args)
        .output()
        .expect("fixture git launches");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(dir: &Path, file: &str, message: &str) {
    fs::write(dir.join(file), message).expect("fixture file");
    git(dir, &["add", file]);
    git(dir, &["commit", "-qm", message]);
}

/// A repository on `main` with a commit, and a branch `other` that has diverged from it.
fn diverged(root: &Path) {
    fs::create_dir_all(root).expect("fixture root");
    git(root, &["init", "-q", "-b", "main"]);
    commit(root, "base", "base");
    git(root, &["switch", "-qc", "other"]);
    commit(root, "other", "other");
    git(root, &["switch", "-q", "main"]);
    commit(root, "main", "main");
}

/// Runs a write the router would refuse, under the git profile.
async fn unchecked(root: &Path, args: &[impl AsRef<str>]) -> Result<String, String> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    let session = SessionSandbox::new(root).expect("session");
    let harness = HarnessGit::new(root, Path::new("/usr/bin/git"), identity()).expect("harness");
    let scanned = session.scan().expect("scan");
    harness
        .execute_sandboxed(&GitWrite::unchecked(&args), &session, scanned)
        .await
}

/// Arguments that make git run an in-root script as an alias, which the router never allows.
fn script(dir: &Path, body: &str) -> [String; 3] {
    let path = dir.join("evil.sh");
    fs::write(&path, format!("#!/bin/sh\n{body}")).expect("script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("script mode");
    [
        "-c".into(),
        format!("alias.evil=!{}", path.display()),
        "evil".into(),
    ]
}

/// Quarantine moves entries under HOME, so these tests rerun in a child with a fake HOME. The
/// parent returns `None` once the child has passed; the child returns its fixture directory.
fn in_fake_home(test: &str) -> Option<PathBuf> {
    const KEY: &str = "ELEFANTE_GIT_SANDBOX_FIXTURE";
    if let Some(base) = std::env::var_os(KEY) {
        return Some(base.into());
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let base = fixture.path().canonicalize().expect("canonical fixture");
    fs::create_dir(base.join("home")).expect("fake home");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", test])
        .env(KEY, &base)
        .env("HOME", base.join("home"))
        .output()
        .expect("child launches");
    assert!(
        output.status.success(),
        "{test} child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

#[tokio::test]
async fn diff_output_cannot_write_outside_the_root() {
    let base = tempfile::tempdir().expect("fixture");
    let base = base.path().canonicalize().expect("canonical fixture");
    let root = base.join("root");
    diverged(&root);
    fs::write(root.join("main"), "changed").expect("change");
    let inside = root.join("out");
    let outside = base.join("out");
    unchecked(
        &root,
        &[
            "diff",
            "--no-ext-diff",
            &format!("--output={}", outside.display()),
        ],
    )
    .await
    .expect_err("the output file cannot be created");
    assert!(!outside.exists());
    // Control: the same output path inside the root is written.
    let _ = unchecked(
        &root,
        &[
            "diff",
            "--no-ext-diff",
            &format!("--output={}", inside.display()),
        ],
    )
    .await;
    assert!(
        fs::read_to_string(inside)
            .expect("control output")
            .contains("changed")
    );
}

#[tokio::test]
async fn diff_no_index_cannot_read_outside_the_root() {
    let base = tempfile::tempdir().expect("fixture");
    let base = base.path().canonicalize().expect("canonical fixture");
    let root = base.join("root");
    diverged(&root);
    fs::write(base.join("secret"), "OUTSIDE-SECRET").expect("outside fixture");
    fs::write(root.join("file"), "inside").expect("inside fixture");
    let outside = unchecked(
        &root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-index",
            base.join("secret").to_str().unwrap(),
            "file",
        ],
    )
    .await
    .expect_err("the outside file cannot be read");
    assert!(!outside.contains("OUTSIDE-SECRET"), "{outside}");
    // Control: git exits 1 when two in-root files differ, and prints the difference.
    let inside = unchecked(
        &root,
        &["diff", "--no-ext-diff", "--no-index", "base", "file"],
    )
    .await
    .expect_err("differing files exit 1");
    assert!(inside.contains("+inside"), "{inside}");
}

#[tokio::test]
async fn alias_script_cannot_escape() {
    let Some(base) = in_fake_home("alias_script_cannot_escape") else {
        return;
    };
    let root = base.join("root");
    let outside = base.join("outside");
    fs::create_dir(&outside).expect("outside directory");
    diverged(&root);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("host listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("address").port();
    let attack = script(
        &root,
        &format!(
            r#"echo ran > "{root}/marker"
try() {{ label=$1; shift; ( "$@" ) >/dev/null 2>&1 && echo "$label wrote" >> "{root}/marker"; }}
try outside sh -c 'echo x > "{outside}/x"'
try hook sh -c 'echo x > .git/hooks/x'
try config sh -c 'echo x > .git/config'
try commondir sh -c 'echo ../x > .git/commondir'
try rename mv .git .git-moved
try network bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'
exit 2
"#,
            root = root.display(),
            outside = outside.display(),
        ),
    );
    let config = fs::read(root.join(".git/config")).expect("config");
    let error = unchecked(&root, &attack)
        .await
        .expect_err("the strategy fails");
    // Control: the script ran and wrote its marker inside the root.
    let marker =
        fs::read_to_string(root.join("marker")).unwrap_or_else(|_| panic!("no marker: {error}"));
    assert!(marker.starts_with("ran"), "{marker}");
    for attempt in ["outside", "hook", "config", "rename", "network"] {
        assert!(!marker.contains(attempt), "{attempt} succeeded: {marker}");
    }
    assert!(!outside.join("x").exists());
    assert!(!root.join(".git/hooks/x").exists());
    assert_eq!(fs::read(root.join(".git/config")).expect("config"), config);
    assert!(root.join(".git").is_dir() && !root.join(".git-moved").exists());
    // A new protected name is caught after the run, wherever the OS allowed it.
    assert!(!root.join(".git/commondir").exists());
    assert!(error.contains("quarantined"), "{error}");
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[tokio::test]
async fn protected_names_and_new_repositories_are_quarantined() {
    let Some(base) = in_fake_home("protected_names_and_new_repositories_are_quarantined") else {
        return;
    };
    let root = base.join("root");
    diverged(&root);
    // With no `hooks` directory, `HOOKS` is an absent protected name on every platform.
    fs::remove_dir_all(root.join(".git/hooks")).expect("remove hooks");
    let attack = script(
        &root,
        "echo x > .git/commondir; mkdir .git/HOOKS; mkdir -p sub/.git; exit 2\n",
    );
    let error = unchecked(&root, &attack).await.expect_err("the call fails");
    assert!(error.contains("quarantined"), "{error}");
    let names: Vec<_> = fs::read_dir(root.join(".git"))
        .expect(".git")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    for name in ["commondir", "HOOKS"] {
        assert!(
            !names.iter().any(|n| n.eq_ignore_ascii_case(name)),
            "{name} stayed: {names:?}"
        );
    }
    // macOS refuses the new repository at the OS; Linux quarantines it.
    assert!(!root.join("sub/.git").exists());
    let quarantined: usize = fs::read_dir(base.join("home/.elefante/quarantine"))
        .expect("quarantine under the fake HOME")
        .map(|session| {
            fs::read_dir(session.expect("session").path())
                .expect("entries")
                .count()
        })
        .sum();
    assert_eq!(quarantined, if cfg!(target_os = "linux") { 3 } else { 2 });
}

async fn bash(tools: &ToolExecutor, command: &str) -> String {
    let result = tools.execute("bash", json!({ "command": command })).await;
    assert!(!result.is_error, "{command}: {}", result.content);
    result.content
}

fn tools(root: &Path) -> ToolExecutor {
    ToolExecutor::new(root)
        .expect("tool session")
        .with_git_identity(identity())
        .expect("harness git")
}

#[tokio::test]
async fn commit_rebase_merge_and_stash_round_trip_through_the_router() {
    let root = tempfile::tempdir().expect("fixture");
    let root = root.path().canonicalize().expect("canonical fixture");
    fs::create_dir_all(&root).expect("root");
    git(&root, &["init", "-q", "-b", "main"]);
    commit(&root, "base", "base");
    let tools = tools(&root);
    bash(&tools, "git switch -c feat").await;
    fs::write(root.join("feat"), "feat").expect("file");
    bash(&tools, "git add feat").await;
    bash(&tools, "git commit -m feat").await;
    bash(&tools, "git switch main").await;
    fs::write(root.join("one"), "one").expect("file");
    bash(&tools, "git add one").await;
    bash(&tools, "git commit -m one").await;
    bash(&tools, "git rebase feat").await;
    bash(&tools, "git switch -c side").await;
    fs::write(root.join("side"), "side").expect("file");
    bash(&tools, "git add side").await;
    bash(&tools, "git commit -m side").await;
    bash(&tools, "git switch main").await;
    fs::write(root.join("two"), "two").expect("file");
    bash(&tools, "git add two").await;
    bash(&tools, "git commit -m two").await;
    bash(&tools, "git merge --no-ff side -m merged").await;
    fs::write(root.join("base"), "changed").expect("file");
    bash(&tools, "git stash push").await;
    assert_eq!(fs::read_to_string(root.join("base")).unwrap(), "base");
    bash(&tools, "git stash pop").await;
    assert_eq!(fs::read_to_string(root.join("base")).unwrap(), "changed");
    let log = bash(&tools, "git log --oneline").await;
    for message in ["merged", "two", "one", "feat", "side"] {
        assert!(log.contains(message), "{message} missing: {log}");
    }
}

/// A linked worktree `wt` on branch `wtb` and a sibling worktree `wt2`, both of a repository
/// outside `wt`. Branch `other` has diverged from `wtb`.
fn worktrees(base: &Path) -> (PathBuf, PathBuf) {
    let main = base.join("main");
    diverged(&main);
    let (wt, wt2) = (base.join("wt"), base.join("wt2"));
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "wtb",
            "main~1",
        ],
    );
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            wt2.to_str().unwrap(),
            "-b",
            "wt2b",
            "main~1",
        ],
    );
    commit(&wt, "wt", "wt");
    (main, wt)
}

#[tokio::test]
async fn a_linked_worktree_commits_through_the_router() {
    let base = tempfile::tempdir().expect("fixture");
    let base = base.path().canonicalize().expect("canonical fixture");
    let (main, wt) = worktrees(&base);
    let tools = tools(&wt);
    fs::write(wt.join("linked"), "linked").expect("file");
    bash(&tools, "git add linked").await;
    bash(&tools, "git commit -m linked").await;
    let log = Command::new("/usr/bin/git")
        .args(["-C", main.to_str().unwrap(), "log", "--oneline", "wtb"])
        .output()
        .expect("log");
    assert!(String::from_utf8_lossy(&log.stdout).contains("linked"));
}

#[tokio::test]
async fn a_sibling_worktree_admin_directory_stays_read_only() {
    let base = tempfile::tempdir().expect("fixture");
    let base = base.path().canonicalize().expect("canonical fixture");
    let (main, wt) = worktrees(&base);
    let sibling = main.join(".git/worktrees/wt2");
    let own = main.join(".git/worktrees/wt");
    let attack = script(
        &wt,
        &format!(
            r#"echo ran > "{wt}/marker"
try() {{ label=$1; shift; ( "$@" ) >/dev/null 2>&1 && echo "$label wrote" >> "{wt}/marker"; }}
try sibling-edit sh -c 'echo x > "{sibling}/commondir"'
try sibling-new sh -c 'echo x > "{sibling}/new"'
try own-edit sh -c 'echo x > "{own}/commondir"'
try own-new sh -c 'echo x > "{own}/control"'
exit 2
"#,
            wt = wt.display(),
            sibling = sibling.display(),
            own = own.display(),
        ),
    );
    let error = unchecked(&wt, &attack)
        .await
        .expect_err("the strategy fails");
    let marker =
        fs::read_to_string(wt.join("marker")).unwrap_or_else(|_| panic!("no marker: {error}"));
    // Control: the session's own admin directory takes a new file, and only that write succeeds.
    assert_eq!(marker, "ran\nown-new wrote\n");
    assert!(!sibling.join("new").exists());
}
