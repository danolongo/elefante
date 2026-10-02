//! The content-addressed extraction cache in `cache.db`.
//!
//! Every function takes a plain connection and does no parsing: parsing is the expensive step
//! and must not run on the writer thread. The indexer looks up a batch of hashes, parses the
//! misses elsewhere, and stores the results in a second short call.

use rusqlite::{Connection, OptionalExtension};

use super::Extraction;
use crate::error::MemError;

/// Unreferenced extractions older than this are collected.
pub const GC_MAX_AGE_NS: i64 = 14 * 24 * 3600 * 1_000_000_000;

/// Returns the cached extraction for `(hash, extractor)` and marks it used.
///
/// A row that no longer deserialises is deleted and reported as a miss: the cache is
/// disposable, and a reparse is cheaper than an error.
///
/// # Errors
/// SQLite errors.
pub fn lookup(
    cache: &Connection,
    hash: &[u8; 32],
    extractor: &str,
    now_ns: i64,
) -> Result<Option<Extraction>, MemError> {
    let record: Option<String> = cache
        .query_row(
            "SELECT record FROM extraction WHERE blake3 = ?1 AND extractor = ?2",
            rusqlite::params![hash.as_slice(), extractor],
            |r| r.get(0),
        )
        .optional()?;
    let Some(record) = record else {
        return Ok(None);
    };
    if let Ok(extraction) = serde_json::from_str::<Extraction>(&record) {
        cache.execute(
            "UPDATE extraction SET last_used_ns = ?3 WHERE blake3 = ?1 AND extractor = ?2",
            rusqlite::params![hash.as_slice(), extractor, now_ns],
        )?;
        return Ok(Some(extraction));
    }
    cache.execute(
        "DELETE FROM extraction WHERE blake3 = ?1 AND extractor = ?2",
        rusqlite::params![hash.as_slice(), extractor],
    )?;
    Ok(None)
}

/// Stores an extraction. Re-storing identical content is harmless and refreshes its use time.
///
/// # Errors
/// SQLite errors.
///
/// # Panics
/// Never in practice: an `Extraction` always serialises.
pub fn store(
    cache: &Connection,
    hash: &[u8; 32],
    extractor: &str,
    extraction: &Extraction,
    now_ns: i64,
) -> Result<(), MemError> {
    let record = serde_json::to_string(extraction).expect("extraction serialises");
    cache.execute(
        "INSERT INTO extraction(blake3, extractor, record, last_used_ns) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(blake3, extractor) DO UPDATE SET
           record = excluded.record, last_used_ns = excluded.last_used_ns",
        rusqlite::params![hash.as_slice(), extractor, record, now_ns],
    )?;
    Ok(())
}

/// Deletes extractions that no `file_state` row references and that have been unused for
/// longer than `max_age_ns`. Unreferenced rows are kept on purpose for a while — they make
/// switching back to a previously indexed branch free — so collection is by age.
///
/// # Errors
/// SQLite errors.
pub fn gc(cache: &Connection, now_ns: i64, max_age_ns: i64) -> Result<usize, MemError> {
    Ok(cache.execute(
        "DELETE FROM extraction
         WHERE last_used_ns < ?1 AND blake3 NOT IN (SELECT blake3 FROM file_state)",
        [now_ns.saturating_sub(max_age_ns)],
    )?)
}
