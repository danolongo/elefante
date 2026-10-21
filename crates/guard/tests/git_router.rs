use elefante_guard::git::{self, GitRoute, Identity};
use std::path::Path;

#[test]
fn routes_only_literal_local_writes() {
    let root = tempfile::tempdir().unwrap();
    for source in [
        "git add -A",
        "git add -- src/lib.rs",
        "git rm --cached -r src",
        "git mv old new",
        "git restore --staged -- src",
        "git commit -a -m 'message with spaces' --amend --no-edit",
        "git switch -c topic",
        "git checkout -b topic",
        "git branch -d topic",
        "git stash push -m 'saved work'",
        "git stash pop",
        "git stash apply",
        "git tag release",
        "git tag -a release -m release",
        "git merge --no-ff topic -m merge",
        "git merge --ff-only topic",
        "git merge --abort",
        "git cherry-pick abc123",
        "git cherry-pick --continue",
        "git rebase topic",
        "git rebase --abort",
        "git add -u",
        "git rm -- src",
        "git restore --worktree src",
        "git commit -m first -m second",
        "git switch topic",
        "git checkout topic",
        "git branch topic",
        "git stash push",
        "git merge topic",
        "git cherry-pick --abort",
        "git rebase --continue",
    ] {
        assert!(
            matches!(git::route(source, root.path()), GitRoute::Harness(_)),
            "{source}"
        );
    }
}

#[test]
fn refuses_unsupported_or_dynamic_writes() {
    let root = tempfile::tempdir().unwrap();
    for source in [
        "git commit",
        "git commit --amen -m x",
        "git commit -S -m x",
        "git merge -s x",
        "git rebase --exec x topic",
        "git add .GIT/config",
        "git add :/src",
        "git add ../outside",
        "git add '*.rs'",
        "git add ~",
        "git add $'src'",
        "git add \"src\"",
        "git add $FILE",
        "git add $(pwd)",
        "git add src > output",
        "git add src | cat",
        "git log | git commit -m x",
        "git add src; true",
        "X=y git add src",
        "git -C . add src",
        "git push",
        "git fetch",
        "git config x y",
        "git alias",
        "git checkout topic -- src",
        "git branch --delete topic",
        "git commit -m x\ntrue",
        "git add src && git commit -m x",
        "git add",
        "git add --output=outside src",
        "git add {src,tests}",
        "git add `pwd`",
        "git add .env",
        "git add .elefante/state",
        "git commit --message x",
        "git commit -m",
        "git switch --orphan topic",
        "git switch -c '-bad'",
        "git checkout topic~1",
        "git stash drop",
        "git tag -s release",
        "git merge topic other",
        "git reset --hard",
        "git clean -fd",
        "git --git-dir=other add src",
        "/usr/bin/git add src",
        "git add src &",
        "git log $(git commit -m x)",
        "X=y git -C . add src",
        "\"git\" add src",
        "$'git' add src",
        "g''it add src",
        "g\\it add src",
        "env git add src",
        "command git add src",
    ] {
        assert!(
            matches!(git::route(source, root.path()), GitRoute::Refused(_)),
            "{source}"
        );
    }
    for source in [
        "git status",
        "git log -p",
        "git branch",
        "git tag",
        "echo hi",
        "echo git",
        "git log | cat",
        "git diff --output=$HOME/x",
        "git branch | cat",
        "git log branch | cat",
        "git log --format=commit | cat",
    ] {
        assert_eq!(
            git::route(source, root.path()),
            GitRoute::NotHarness,
            "{source}"
        );
    }
}

#[test]
fn invocation_is_explicit_and_hardened() {
    let root = tempfile::tempdir().unwrap();
    let GitRoute::Harness(write) = git::route("git commit -m 'a message'", root.path()) else {
        panic!("route")
    };
    let invocation = git::invocation(
        Path::new("/usr/bin/git"),
        root.path(),
        &write,
        &Identity {
            name: "Agent".into(),
            email: "agent@example.invalid".into(),
        },
    );
    assert_eq!(invocation.program, Path::new("/usr/bin/git"));
    assert!(
        invocation
            .args
            .windows(2)
            .any(|args| args == ["commit", "-m"])
    );
    assert_eq!(invocation.args.last().unwrap(), "a message");
    assert!(
        invocation
            .args
            .iter()
            .any(|arg| arg == "core.hooksPath=/dev/null")
    );
    for setting in [
        "core.fsmonitor=false",
        "core.attributesFile=/dev/null",
        "core.pager=cat",
        "core.editor=false",
        "sequence.editor=false",
        "protocol.allow=never",
        "credential.helper=",
        "commit.gpgSign=false",
        "tag.gpgSign=false",
        "gpg.program=false",
        "gpg.ssh.program=false",
        "core.sshCommand=false",
        "diff.external=",
        "merge.default=text",
    ] {
        assert!(
            invocation
                .args
                .windows(2)
                .any(|args| args[0] == "-c" && args[1] == setting),
            "missing hardened setting {setting}"
        );
    }
    for (key, expected) in [
        ("PATH", "/usr/bin:/bin"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS", "1"),
    ] {
        assert_eq!(
            invocation
                .environment
                .get(std::ffi::OsStr::new(key))
                .expect("hardened environment value"),
            expected,
            "{key}"
        );
    }
    assert!(
        !invocation
            .environment
            .contains_key(std::ffi::OsStr::new("OPENROUTER_API_KEY"))
    );
    assert_eq!(
        invocation
            .environment
            .get(std::ffi::OsStr::new("GIT_AUTHOR_NAME"))
            .unwrap(),
        "Agent"
    );
}

#[test]
fn ceiling_allows_root_repository_but_not_ancestor_discovery() {
    let root = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "--quiet"])
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
    let child = root.path().join("child");
    std::fs::create_dir(&child).unwrap();
    for (cwd, succeeds) in [(root.path(), true), (child.as_path(), false)] {
        let invocation = git::hardened(Path::new("/usr/bin/git"), cwd);
        let output = std::process::Command::new(invocation.program)
            .args(invocation.args)
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(invocation.cwd)
            .env_clear()
            .envs(invocation.environment)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
