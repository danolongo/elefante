//! The async face of learned memory: the calls `core` makes. Each runs on the store's actors, so
//! the model call that precedes it never holds a database thread.

use super::{
    Candidate, IngestInput, IngestReport, LearnedNode, LearnedRow, NoteCounts, Turn, approve,
    clear, compact_if_due, counts, forget, ingest, list, load_by_ids, nearest,
};
use crate::{error::MemError, graph, store::Store, util::now_ns};

/// The notes closest to `text`, for showing a model what is already known.
///
/// # Errors
/// SQLite errors, or a reader thread that is gone.
pub async fn nearest_async(
    store: &Store,
    text: String,
    limit: usize,
) -> Result<Vec<LearnedNode>, MemError> {
    let key = store.key().clone();
    store
        .reader()
        .call(move |s| nearest(&s.memory, &key, &text, limit))
        .await?
}

/// Loads verified learned notes by id in one reader-actor call.
///
/// # Errors
/// SQLite errors, or a reader thread that is gone.
pub async fn load_by_ids_async(store: &Store, ids: Vec<i64>) -> Result<Vec<LearnedNode>, MemError> {
    let key = store.key().clone();
    store
        .reader()
        .call(move |state| load_by_ids(&state.memory, &key, &ids))
        .await?
}

/// Counts active and pending learned notes in one reader call.
///
/// # Errors
/// SQLite errors, or a reader thread that is gone.
pub async fn counts_async(store: &Store) -> Result<NoteCounts, MemError> {
    store.reader().call(|state| counts(&state.memory)).await?
}

/// Lists learned rows on the reader actor.
///
/// # Errors
/// SQLite errors, or a reader thread that is gone.
pub async fn list_async(store: &Store, all: bool) -> Result<Vec<LearnedRow>, MemError> {
    let key = store.key().clone();
    store
        .reader()
        .call(move |state| list(&state.memory, &key, all, now_ns()))
        .await?
}

/// Forgets one learned note and advances the generation mirror.
///
/// # Errors
/// SQLite errors, or a writer thread that is gone.
pub async fn forget_async(store: &Store, id: i64) -> Result<bool, MemError> {
    let (changed, generation, counts) = store
        .writer()
        .call(move |state| {
            let changed = forget(&mut state.memory, id)?;
            Ok::<_, MemError>((
                changed,
                graph::generation(&state.memory)?,
                counts(&state.memory)?,
            ))
        })
        .await??;
    store.note_generation(generation);
    store.note_learned_counts(counts);
    Ok(changed)
}

/// Approves one pending note and advances the generation mirror.
///
/// # Errors
/// SQLite errors, or a writer thread that is gone.
pub async fn approve_async(store: &Store, id: i64) -> Result<bool, MemError> {
    let key = store.key().clone();
    let (changed, generation, counts) = store
        .writer()
        .call(move |state| {
            let changed = approve(&mut state.memory, &key, id, now_ns())?;
            Ok::<_, MemError>((
                changed,
                graph::generation(&state.memory)?,
                counts(&state.memory)?,
            ))
        })
        .await??;
    store.note_generation(generation);
    store.note_learned_counts(counts);
    Ok(changed)
}

/// Clears every learned note and advances the generation mirror.
///
/// # Errors
/// SQLite errors, or a writer thread that is gone.
pub async fn clear_async(store: &Store) -> Result<usize, MemError> {
    let (removed, generation, counts) = store
        .writer()
        .call(|state| {
            let removed = clear(&mut state.memory)?;
            Ok::<_, MemError>((
                removed,
                graph::generation(&state.memory)?,
                counts(&state.memory)?,
            ))
        })
        .await??;
    store.note_generation(generation);
    store.note_learned_counts(counts);
    Ok(removed)
}

/// Ingests candidates on the writer, advances the store's generation mirror, and compacts when it
/// is due. Returns the ingest report.
///
/// # Errors
/// SQLite errors, or a writer thread that is gone.
pub async fn ingest_async(
    store: &Store,
    candidates: Vec<Candidate>,
    excerpt: Vec<Turn>,
    shown: Vec<i64>,
) -> Result<IngestReport, MemError> {
    let key = store.key().clone();
    let (mut report, generation, counts) = store
        .writer()
        .call(move |s| {
            let now = now_ns();
            let report = ingest(
                &mut s.memory,
                &key,
                &IngestInput {
                    candidates: &candidates,
                    excerpt: &excerpt,
                    shown: &shown,
                },
                now,
            )?;
            let compacted = compact_if_due(&mut s.memory, &key, now)?;
            let generation = compacted.map_or(report.generation, |c| c.generation);
            Ok::<_, MemError>((report, generation, counts(&s.memory)?))
        })
        .await??;
    store.note_generation(generation);
    store.note_learned_counts(counts);
    report.generation = generation;
    Ok(report)
}
