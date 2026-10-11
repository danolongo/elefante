//! A real directory with an `Arc<Store>`, for tests that hand the store to a retriever or indexer.

#![allow(dead_code)]

use std::{fs, sync::Arc};

use elefante_mem::{
    Counters, RepoId, Store,
    indexer::{IndexOptions, refresh_blocking},
};

pub struct Live {
    _tmp: tempfile::TempDir,
    pub root: std::path::PathBuf,
    pub store: Arc<Store>,
}

pub fn live(files: &[(&str, &str)]) -> Live {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    for (path, contents) in files {
        let target = tmp.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
    }
    let repo = RepoId::resolve_with_home(tmp.path(), None).unwrap();
    Live {
        root: repo.root().to_path_buf(),
        store: Arc::new(
            Store::open_with_key(&repo, elefante_mem::MemKey::from_bytes([7; 32])).unwrap(),
        ),
        _tmp: tmp,
    }
}

pub fn refresh(live: &Live) {
    refresh_blocking(
        &live.store,
        &live.root,
        &Counters::default(),
        &IndexOptions::default(),
    )
    .unwrap();
}
