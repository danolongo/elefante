//! `file_state`: what the cache remembers about each file's last indexing.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::error::MemError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateRow {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub blake3: [u8; 32],
    /// Wall clock when the content was last hashed; drives the racy-mtime rule.
    pub indexed_at_ns: i64,
    /// Binary: remembered so it is never re-read, never indexed.
    pub skip: bool,
}

/// Loads every row.
///
/// # Errors
/// SQLite errors.
pub fn load(cache: &Connection) -> Result<HashMap<String, StateRow>, MemError> {
    let mut statement = cache
        .prepare("SELECT path, size, mtime_ns, blake3, indexed_at_ns, skip FROM file_state")?;
    let rows = statement.query_map([], |r| {
        let hash: Vec<u8> = r.get(3)?;
        Ok(StateRow {
            path: r.get(0)?,
            size: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
            mtime_ns: r.get(2)?,
            // A malformed hash can never equal a real one, so the row is reprocessed.
            blake3: hash.try_into().unwrap_or([0; 32]),
            indexed_at_ns: r.get(4)?,
            skip: r.get::<_, i64>(5)? != 0,
        })
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let row = row?;
        map.insert(row.path.clone(), row);
    }
    Ok(map)
}

/// Inserts or replaces rows. The caller owns the transaction.
///
/// # Errors
/// SQLite errors.
pub fn upsert(cache: &Connection, rows: &[StateRow]) -> Result<(), MemError> {
    let mut statement = cache.prepare_cached(
        "INSERT INTO file_state(path, size, mtime_ns, blake3, indexed_at_ns, skip)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET size = excluded.size, mtime_ns = excluded.mtime_ns,
           blake3 = excluded.blake3, indexed_at_ns = excluded.indexed_at_ns, skip = excluded.skip",
    )?;
    for row in rows {
        statement.execute(rusqlite::params![
            row.path,
            i64::try_from(row.size).unwrap_or(i64::MAX),
            row.mtime_ns,
            row.blake3.as_slice(),
            row.indexed_at_ns,
            i64::from(row.skip),
        ])?;
    }
    Ok(())
}

/// Deletes rows by path. The caller owns the transaction.
///
/// # Errors
/// SQLite errors.
pub fn delete(cache: &Connection, paths: &[String]) -> Result<(), MemError> {
    let mut statement = cache.prepare_cached("DELETE FROM file_state WHERE path = ?1")?;
    for path in paths {
        statement.execute([path])?;
    }
    Ok(())
}
