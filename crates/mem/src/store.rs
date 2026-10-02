//! The open pair of databases and the two actors that own them.

use rusqlite::Connection;

use crate::{
    actor::Actor,
    error::MemError,
    layout::StoreDir,
    repo::RepoId,
    schema::{self, Pairing},
};

/// State owned by the writer thread: both write connections.
pub struct WriterState {
    pub memory: Connection,
    pub cache: Connection,
}

/// State owned by the reader thread: a read-only view of the graph. WAL lets it proceed while
/// the writer commits, so retrieval never waits on an index write.
pub struct ReaderState {
    pub memory: Connection,
}

pub struct Store {
    dir: StoreDir,
    pairing: Pairing,
    writer: Actor<WriterState>,
    reader: Actor<ReaderState>,
}

impl Store {
    /// Opens (creating if needed) the store for `repo`.
    ///
    /// Order matters: the self-ignoring `.gitignore` first, then `memory.db`, then `cache.db`,
    /// then the pairing check, and only then the read connection and the threads.
    ///
    /// # Errors
    /// Fails on any I/O or SQLite error, or an unsupported or corrupt `memory.db`.
    pub fn open(repo: &RepoId) -> Result<Self, MemError> {
        let dir = StoreDir::create(repo)?;
        let memory = schema::open_memory(&dir.memory_db())?;
        let mut cache = schema::open_cache(&dir.cache_db())?;
        let pairing = schema::pair(&memory, &mut cache)?;
        let reader = schema::open_memory_reader(&dir.memory_db())?;
        Ok(Self {
            dir,
            pairing,
            writer: Actor::spawn("elefante-mem-writer", WriterState { memory, cache }),
            reader: Actor::spawn("elefante-mem-reader", ReaderState { memory: reader }),
        })
    }

    #[must_use]
    pub fn dir(&self) -> &StoreDir {
        &self.dir
    }

    /// Whether `file_state` survived opening. `Reset` means the next refresh re-examines every file.
    #[must_use]
    pub fn pairing(&self) -> Pairing {
        self.pairing
    }

    #[must_use]
    pub fn writer(&self) -> &Actor<WriterState> {
        &self.writer
    }

    #[must_use]
    pub fn reader(&self) -> &Actor<ReaderState> {
        &self.reader
    }
}
