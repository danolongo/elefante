//! Hermetic git fixture builder shared by the `mem` integration tests.
//!
//! Every git invocation runs with an empty global/system config, a throwaway `HOME`, and fixed
//! identities and dates. A test that passed only because the developer's own git config supplied
//! something is the failure shape `MILESTONES.md` records three times; this builder exists so
//! that cannot happen here.

#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::TempDir;

pub mod tree;

pub struct GitRepo {
    dir: TempDir,
    home: TempDir,
    tick: u64,
}

impl GitRepo {
    /// Creates an empty repository on branch `main`.
    pub fn init() -> Self {
        let repo = Self {
            dir: tempfile::tempdir().expect("repo dir"),
            home: tempfile::tempdir().expect("home dir"),
            tick: 0,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let target = self.dir.path().join(relative);
        fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        fs::write(target, contents).expect("write");
    }

    pub fn rename(&self, from: &str, to: &str) {
        let target = self.dir.path().join(to);
        fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        self.git(&["mv", from, to]);
    }

    /// Stages everything and commits with a strictly increasing, fixed timestamp.
    pub fn commit(&mut self, message: &str) {
        self.tick += 1;
        let date = format!("{} +0000", 1_700_000_000 + self.tick * 60);
        self.git_dated(&["add", "-A"], &date);
        self.git_dated(&["commit", "-q", "-m", message], &date);
    }

    pub fn git(&self, args: &[&str]) -> Vec<u8> {
        self.git_dated(args, "1700000000 +0000")
    }

    fn git_dated(&self, args: &[&str], date: &str) -> Vec<u8> {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.dir.path())
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("git is installed");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }
}

pub fn repo_path(repo: &GitRepo) -> PathBuf {
    repo.path().to_path_buf()
}
