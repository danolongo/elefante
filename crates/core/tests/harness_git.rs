use elefante_core::harness_git::HarnessGit;
use elefante_guard::git::{GitRoute, Identity, route};
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("/usr/bin/git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn harness(root: &Path) -> HarnessGit {
    HarnessGit::new(
        root,
        Path::new("/usr/bin/git"),
        Identity {
            name: "Test".into(),
            email: "test@example.invalid".into(),
        },
    )
    .unwrap()
}

async fn add(root: &Path, runner: &HarnessGit) -> Result<String, String> {
    let GitRoute::Harness(write) = route("git add .", root) else {
        panic!("route")
    };
    runner.execute(&write).await
}

#[tokio::test]
async fn ordinary_add_and_commit_disable_hooks() {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = temp.path().join(".git/hooks/pre-commit");
        fs::write(&hook, "#!/bin/sh\ntouch hook-marker\n").unwrap();
        fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(temp.path().join("file"), "hello").unwrap();
    let runner = harness(temp.path());
    add(temp.path(), &runner).await.unwrap();
    let GitRoute::Harness(write) = route("git commit -m initial", temp.path()) else {
        panic!("route")
    };
    runner.execute(&write).await.unwrap();
    assert!(!temp.path().join("hook-marker").exists());
    #[cfg(unix)]
    {
        git(
            temp.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "hook-control",
            ],
        );
        assert!(temp.path().join("hook-marker").exists());
    }
}

#[tokio::test]
async fn late_repository_does_not_gain_harness_writes() {
    let temp = tempfile::tempdir().unwrap();
    let runner = harness(temp.path());
    git(temp.path(), &["init", "-q"]);
    assert!(add(temp.path(), &runner).await.is_err());
}

#[tokio::test]
async fn custom_attribute_drivers_are_refused_before_execution() {
    for attribute in ["filter=trap", "merge=trap", "filter=lfs"] {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "filter.trap.clean", "touch marker; cat"],
        );
        git(
            temp.path(),
            &["config", "merge.trap.driver", "touch marker"],
        );
        fs::write(
            temp.path().join(".gitattributes"),
            format!("* {attribute}\n"),
        )
        .unwrap();
        fs::write(temp.path().join("file"), "hello").unwrap();
        assert!(add(temp.path(), &harness(temp.path())).await.is_err());
        assert!(!temp.path().join("marker").exists());
        if attribute == "filter=trap" {
            git(temp.path(), &["add", "file"]);
            assert!(
                temp.path().join("marker").exists(),
                "positive filter control"
            );
        }
    }
}

#[tokio::test]
async fn index_fallback_info_and_unpruned_attributes_are_checked() {
    for location in ["index", "info", "ignored"] {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        match location {
            "index" => {
                fs::write(temp.path().join(".gitattributes"), "* filter=trap\n").unwrap();
                git(temp.path(), &["add", ".gitattributes"]);
                fs::remove_file(temp.path().join(".gitattributes")).unwrap();
            }
            "info" => {
                fs::write(temp.path().join(".git/info/attributes"), "* merge=trap\n").unwrap();
            }
            _ => {
                fs::create_dir(temp.path().join("target")).unwrap();
                fs::write(temp.path().join("target/.gitattributes"), "* filter=trap\n").unwrap();
            }
        }
        assert!(
            add(temp.path(), &harness(temp.path())).await.is_err(),
            "{location}"
        );
    }
}

#[tokio::test]
async fn builtin_attribute_drivers_remain_usable() {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    fs::write(
        temp.path().join(".gitattributes"),
        "text merge=text\nbinary merge=binary\nunion merge=union\n* -filter\n",
    )
    .unwrap();
    fs::write(temp.path().join("text"), "hello").unwrap();
    add(temp.path(), &harness(temp.path())).await.unwrap();
}

#[tokio::test]
async fn in_root_config_targets_are_refused() {
    for (key, value) in [
        ("core.hooksPath", ".husky"),
        ("include.path", "../included"),
        ("includeIf.gitdir:/.path", "../included"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", key, value]);
        assert!(
            add(temp.path(), &harness(temp.path())).await.is_err(),
            "{key}"
        );
    }
}

#[tokio::test]
async fn merge_driver_refusal_has_an_executable_positive_control() {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "user.name", "Test"]);
    git(
        temp.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(temp.path(), &["branch", "-M", "main"]);
    fs::write(temp.path().join(".gitattributes"), "file merge=trap\n").unwrap();
    fs::write(temp.path().join("file"), "base\n").unwrap();
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-qm", "base"]);
    git(temp.path(), &["checkout", "-qb", "other"]);
    fs::write(temp.path().join("file"), "other\n").unwrap();
    git(temp.path(), &["commit", "-qam", "other"]);
    git(temp.path(), &["checkout", "-q", "main"]);
    fs::write(temp.path().join("file"), "main\n").unwrap();
    git(temp.path(), &["commit", "-qam", "main"]);
    git(
        temp.path(),
        &["config", "merge.trap.driver", "touch marker; false"],
    );
    let GitRoute::Harness(write) = route("git merge other", temp.path()) else {
        panic!("route")
    };
    assert!(harness(temp.path()).execute(&write).await.is_err());
    assert!(!temp.path().join("marker").exists());
    assert!(
        !Command::new("/usr/bin/git")
            .current_dir(temp.path())
            .args(["merge", "other"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        temp.path().join("marker").exists(),
        "positive merge control"
    );
}

#[tokio::test]
async fn target_revision_attributes_cannot_activate_configured_driver() {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    git(temp.path(), &["config", "user.name", "Test"]);
    git(
        temp.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(temp.path(), &["branch", "-M", "main"]);
    fs::write(temp.path().join("file"), "base").unwrap();
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-qm", "base"]);
    git(temp.path(), &["checkout", "-qb", "other"]);
    fs::write(temp.path().join(".gitattributes"), "file filter=trap\n").unwrap();
    fs::write(temp.path().join("file"), "other").unwrap();
    git(temp.path(), &["add", "."]);
    git(temp.path(), &["commit", "-qm", "other"]);
    git(temp.path(), &["checkout", "-q", "main"]);
    git(
        temp.path(),
        &["config", "filter.trap.smudge", "touch marker; cat"],
    );
    let GitRoute::Harness(write) = route("git checkout other", temp.path()) else {
        panic!("route")
    };
    assert!(harness(temp.path()).execute(&write).await.is_err());
    assert!(!temp.path().join("marker").exists());
    git(temp.path(), &["checkout", "-q", "other"]);
    assert!(
        temp.path().join("marker").exists(),
        "positive checkout control"
    );
}

#[tokio::test]
async fn harness_git_uses_the_configured_command_timeout() {
    if std::env::var_os("ELEFANTE_TIMEOUT_FIXTURE_CHILD").is_some() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        fs::write(temp.path().join("file"), "hello").unwrap();
        let error = add(temp.path(), &harness(temp.path()))
            .await
            .expect_err("a zero timeout expires");
        assert!(error.starts_with("command timed out after 0 s"), "{error}");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    fs::write(temp.path().join("file"), "hello").unwrap();
    add(temp.path(), &harness(temp.path()))
        .await
        .expect("the default timeout lets the same write finish");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "harness_git_uses_the_configured_command_timeout"])
        .env("ELEFANTE_TIMEOUT_FIXTURE_CHILD", "1")
        .env("ELEFANTE_BASH_TIMEOUT_SECS", "0")
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "timeout child failed: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}
