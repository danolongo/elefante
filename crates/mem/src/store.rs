//! The open pair of databases and the two actors that own them.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use rusqlite::Connection;

use crate::{
    actor::Actor,
    error::MemError,
    graph,
    layout::StoreDir,
    repo::RepoId,
    schema::{self, MemoryResetReason, Pairing},
    trust::MemKey,
};

/// What retrieval needs to know about the index, readable without a database round trip.
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

    /// Whether a first full index has not yet finished.
    ///
    /// This deliberately ignores whether a refresh is *running*. Refreshes start at every session
    /// create, turn start and turn end, so on a complete index one is nearly always in flight, and
    /// calling that "partial" would mark almost every block and teach the model to distrust an
    /// index that is complete and merely being kept current. Partial means: absence from this
    /// index is not evidence of absence from the repository.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.is_complete()
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
    key: MemKey,
    reset_reason: Option<MemoryResetReason>,
    active_notes: AtomicU64,
    pending_notes: AtomicU64,
}

impl Store {
    /// Opens (creating if needed) the store for `repo`, authenticating it with the user's key at
    /// `~/.elefante/memory.key` (created on first use).
    ///
    /// # Errors
    /// Fails when there is no home directory to keep the key in, or for any reason
    /// [`Store::open_with_key`] fails.
    pub fn open(repo: &RepoId) -> Result<Self, MemError> {
        let path = MemKey::default_path().ok_or_else(|| MemError::Refused {
            path: repo.root().to_path_buf(),
            reason: "HOME is not set, so there is nowhere to keep the memory key",
        })?;
        Self::open_with_key(repo, MemKey::load_or_create(&path)?)
    }

    /// Opens (creating if needed) the store for `repo`.
    ///
    /// Order matters: the self-ignoring `.gitignore` first, then `memory.db`, then `cache.db`,
    /// then the pairing check, and only then the read connection and the threads. A database that
    /// does not carry a stamp made with `key` is discarded and rebuilt (see [`crate::trust`]).
    ///
    /// # Errors
    /// Fails on any I/O or SQLite error, or an unsupported or corrupt `memory.db`.
    pub fn open_with_key(repo: &RepoId, key: MemKey) -> Result<Self, MemError> {
        let dir = StoreDir::create(repo)?;
        let (memory, reset_reason) = schema::open_memory_with_reason(&dir.memory_db(), &key)?;
        let mut cache = schema::open_cache(&dir.cache_db(), &key)?;
        let pairing = schema::pair(&memory, &mut cache)?;
        let reader = schema::open_memory_reader(&dir.memory_db())?;
        let generation = graph::generation(&memory)?;
        let learned_counts = crate::learn::counts(&memory)?;
        let index_state = IndexState::default();
        index_state.set_complete(
            schema::get_meta(&memory, "meta", "index_complete")?.as_deref() == Some("1"),
        );
        Ok(Self {
            dir,
            pairing,
            index_state,
            generation: AtomicI64::new(generation),
            key,
            reset_reason,
            active_notes: AtomicU64::new(learned_counts.active),
            pending_notes: AtomicU64::new(learned_counts.pending),
            writer: Actor::spawn("elefante-mem-writer", WriterState { memory, cache }),
            reader: Actor::spawn("elefante-mem-reader", ReaderState { memory: reader }),
        })
    }

    /// The key this store's rows are authenticated with.
    #[must_use]
    pub fn key(&self) -> &MemKey {
        &self.key
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

    /// Why an existing graph was discarded while opening, if it was.
    #[must_use]
    pub fn reset_reason(&self) -> Option<MemoryResetReason> {
        self.reset_reason
    }

    /// Active and pending learned-note counts as this process last committed them.
    #[must_use]
    pub fn learned_counts(&self) -> crate::learn::NoteCounts {
        crate::learn::NoteCounts {
            active: self.active_notes.load(Ordering::SeqCst),
            pending: self.pending_notes.load(Ordering::SeqCst),
        }
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

    pub(crate) fn note_learned_counts(&self, counts: crate::learn::NoteCounts) {
        self.active_notes.store(counts.active, Ordering::SeqCst);
        self.pending_notes.store(counts.pending, Ordering::SeqCst);
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
