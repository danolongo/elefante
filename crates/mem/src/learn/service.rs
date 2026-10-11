//! The async face of learned memory: the calls `core` makes. Each runs on the store's actors, so
//! the model call that precedes it never holds a database thread.

use super::{
    Candidate, IngestInput, IngestReport, LearnedNode, Turn, compact_if_due, ingest, nearest,
};
use crate::{error::MemError, store::Store, util::now_ns};

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
    let (mut report, generation) = store
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
            Ok::<_, MemError>((report, generation))
        })
        .await??;
    store.note_generation(generation);
    report.generation = generation;
    Ok(report)
}
