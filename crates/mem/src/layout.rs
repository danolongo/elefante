//! The `.elefante/` directory: its creation order, modes, and file locations.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{error::MemError, repo::RepoId};

pub const DIR_NAME: &str = ".elefante";

/// A repository's `.elefante/` directory.
#[derive(Clone, Debug)]
pub struct StoreDir {
    dir: PathBuf,
}

impl StoreDir {
    /// Creates `<repo>/.elefante/` if absent, **writing its `.gitignore` before anything else**.
    ///
    /// The ignore file contains `*`, so the directory ignores itself and the host repository's
    /// own rules are never touched. Writing it first means a crash at any later instant cannot
    /// leave untracked files behind: `git status` is clean at every moment, not only after a
    /// clean exit.
    ///
    /// # Errors
    /// Fails when the directory or ignore file cannot be created.
    pub fn create(repo: &RepoId) -> Result<Self, MemError> {
        let dir = repo.root().join(DIR_NAME);
        create_private_dir(&dir)?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "*\n").map_err(|e| MemError::io(&ignore, e))?;
        }
        Ok(Self { dir })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn memory_db(&self) -> PathBuf {
        self.dir.join("memory.db")
    }

    #[must_use]
    pub fn cache_db(&self) -> PathBuf {
        self.dir.join("cache.db")
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), MemError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| MemError::io(dir, e))?;
    // `create` leaves an existing directory's mode alone. This directory is ours, and it holds a
    // map of the repository, so a looser pre-existing mode is tightened rather than trusted.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| MemError::io(dir, e))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), MemError> {
    fs::create_dir_all(dir).map_err(|e| MemError::io(dir, e))
}

/// Creates `path` empty with mode `0600` if it does not exist.
///
/// SQLite would create it `0644 & ~umask`. Pre-creating it is how the mode is fixed, and SQLite
/// gives the `-wal` and `-shm` sidecars the main file's mode.
#[cfg(unix)]
pub(crate) fn touch_private(path: &Path) -> Result<(), MemError> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map(drop)
        .map_err(|e| MemError::io(path, e))
}

#[cfg(not(unix))]
pub(crate) fn touch_private(path: &Path) -> Result<(), MemError> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map(drop)
        .map_err(|e| MemError::io(path, e))
}
