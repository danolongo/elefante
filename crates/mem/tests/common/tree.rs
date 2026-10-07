//! A real directory, a real store, and the synchronous refresh: the harness for tests that run the
//! pipeline end to end.

#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use elefante_mem::{
    Counters, IndexStats, MemError, RepoId, Store,
    graph::{self, GraphSnapshot},
    indexer::{IndexOptions, RefreshReport, refresh_blocking},
};

pub fn age(path: &Path, seconds: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(seconds))
        .unwrap();
}

pub struct Tree {
    _tmp: tempfile::TempDir,
    pub root: PathBuf,
    store: Option<Store>,
}

impl Tree {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        let tree = Self {
            root: RepoId::resolve_with_home(tmp.path(), None)
                .unwrap()
                .root()
                .to_path_buf(),
            _tmp: tmp,
            store: None,
        };
        for (path, contents) in files {
            tree.write(path, contents, 3600);
        }
        Self {
            store: Some(
                Store::open(&RepoId::resolve_with_home(&tree.root, None).unwrap()).unwrap(),
            ),
            ..tree
        }
    }

    /// Files are aged into the past so the racy-timestamp window does not apply unless a test
    /// is specifically about it.
    pub fn write(&self, path: &str, contents: &str, age_seconds: u64) {
        let target = self.root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents).unwrap();
        age(&target, age_seconds);
    }

    pub fn store(&self) -> &Store {
        self.store.as_ref().unwrap()
    }

    pub fn refresh(&self) -> (RefreshReport, IndexStats) {
        self.refresh_with(&IndexOptions::default()).unwrap()
    }

    pub fn refresh_with(
        &self,
        options: &IndexOptions,
    ) -> Result<(RefreshReport, IndexStats), MemError> {
        let counters = Counters::default();
        let report = refresh_blocking(self.store(), &self.root, &counters, options)?;
        Ok((report, counters.snapshot()))
    }

    pub fn snapshot(&self) -> GraphSnapshot {
        self.store()
            .writer()
            .call_blocking(|s| graph::snapshot(&s.memory).unwrap())
            .unwrap()
    }

    pub fn keys(&self) -> Vec<String> {
        self.snapshot().nodes.into_iter().map(|n| n.1).collect()
    }

    pub fn generation(&self) -> i64 {
        self.store()
            .writer()
            .call_blocking(|s| graph::generation(&s.memory).unwrap())
            .unwrap()
    }

    pub fn meta(&self, key: &'static str) -> Option<String> {
        self.store()
            .writer()
            .call_blocking(move |s| {
                s.memory
                    .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                    .ok()
            })
            .unwrap()
    }

    pub fn ids(&self) -> std::collections::HashMap<String, i64> {
        self.store()
            .writer()
            .call_blocking(|s| {
                s.memory
                    .prepare("SELECT key, id FROM node")
                    .unwrap()
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            })
            .unwrap()
    }

    /// Closes the store, as a process exit would.
    pub fn close(&mut self) -> elefante_mem::StoreDir {
        let dir = self.store.as_ref().unwrap().dir().clone();
        self.store = None;
        dir
    }

    pub fn reopen(&mut self) {
        self.store =
            Some(Store::open(&RepoId::resolve_with_home(&self.root, None).unwrap()).unwrap());
    }
}

pub fn node_keys(snapshot: &GraphSnapshot) -> Vec<(&str, &str)> {
    snapshot
        .nodes
        .iter()
        .map(|n| (n.0.as_str(), n.1.as_str()))
        .collect()
}
