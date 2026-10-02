//! The open pair of databases and the two actors that own them.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use rusqlite::Connection;

use crate::{
    actor::Actor,
    error::MemError,
    graph,
    layout::StoreDir,
    repo::RepoId,
    schema::{self, Pairing},
};

/// Whether the index can be trusted to be complete, readable without a database round trip.
///
/// Retrieval reports `partial` from this, and a cached result is keyed by it, so a cache hit never
/// has to ask the database anything.
#[derive(Debug, Default)]
pub struct IndexState {
    running: AtomicBool,
    complete: AtomicBool,
}

impl IndexState {
    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn set_complete(&self, complete: bool) {
        self.complete.store(complete, Ordering::SeqCst);
    }

    /// A first full index has finished.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::SeqCst)
    }

    /// Retrieval from an index that is being built, or was never finished, is flagged.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.is_running() || !self.is_complete()
    }
}

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
    index_state: IndexState,
    generation: AtomicI64,
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
        let generation = graph::generation(&memory)?;
        let index_state = IndexState::default();
        index_state.set_complete(
            schema::get_meta(&memory, "meta", "index_complete")?.as_deref() == Some("1"),
        );
        Ok(Self {
            dir,
            pairing,
            index_state,
            generation: AtomicI64::new(generation),
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
    pub fn index_state(&self) -> &IndexState {
        &self.index_state
    }

    /// The `repo_generation` as this process last saw it, without touching the database.
    ///
    /// It is advanced by this process's own commits. Another process writing the same repository
    /// would not move it, which is why cached retrievals also expire by age.
    #[must_use]
    pub fn generation(&self) -> i64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub(crate) fn note_generation(&self, generation: i64) {
        self.generation.fetch_max(generation, Ordering::SeqCst);
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
