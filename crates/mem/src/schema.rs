//! DDL, versioning, and the pairing between the two databases. See `docs/PLAN.md` § Memory subsystem, and this crate's README for the contract.

use std::{
    collections::hash_map::RandomState,
    fs,
    hash::{BuildHasher, Hasher},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::{error::MemError, layout::touch_private, trust::MemKey};

pub const MEMORY_SCHEMA_VERSION: i64 = 2;
pub const CACHE_SCHEMA_VERSION: i64 = 2;

/// `meta` / `cache_meta` key holding the keyed stamp that marks a database as ours.
pub const STAMP_KEY: &str = "store_stamp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryResetReason {
    Untrusted,
    SchemaVersion,
}

const MEMORY_DDL: &str = "
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE node (
  id            INTEGER PRIMARY KEY,
  kind          TEXT    NOT NULL,
  key           TEXT    NOT NULL,
  name          TEXT    NOT NULL,
  body          TEXT    NOT NULL DEFAULT '',
  file_id       INTEGER REFERENCES node(id) ON DELETE CASCADE,
  attrs         TEXT    NOT NULL DEFAULT '{}',
  confidence    REAL    NOT NULL DEFAULT 1.0,
  source        TEXT    NOT NULL DEFAULT 'index',
  created_at_ns INTEGER NOT NULL,
  updated_at_ns INTEGER NOT NULL,
  status        TEXT    NOT NULL DEFAULT 'active',
  last_reinforced_ns INTEGER,
  mac           BLOB,
  UNIQUE (kind, key)
);
CREATE INDEX node_file ON node(file_id);
CREATE INDEX node_kind_name ON node(kind, name);

CREATE TABLE edge (
  src           INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  dst           INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  rel           TEXT    NOT NULL,
  weight        REAL    NOT NULL DEFAULT 1.0,
  created_at_ns INTEGER NOT NULL,
  PRIMARY KEY (src, dst, rel),
  CHECK (rel <> 'co_edited_with' OR src < dst)
) WITHOUT ROWID;
CREATE INDEX edge_dst ON edge(dst, rel);

CREATE TABLE import_ref (
  file_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  kind    TEXT    NOT NULL,
  spec    TEXT    NOT NULL,
  PRIMARY KEY (file_id, kind, spec)
) WITHOUT ROWID;

CREATE VIRTUAL TABLE node_fts USING fts5(
  name_terms, path_terms, body_terms,
  tokenize = 'porter unicode61 remove_diacritics 2'
);
";

const CACHE_DDL: &str = "
CREATE TABLE cache_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE file_state (
  path          TEXT PRIMARY KEY,
  size          INTEGER NOT NULL,
  mtime_ns      INTEGER NOT NULL,
  blake3        BLOB    NOT NULL,
  indexed_at_ns INTEGER NOT NULL,
  skip          INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

CREATE TABLE extraction (
  blake3       BLOB    NOT NULL,
  extractor    TEXT    NOT NULL,
  record       TEXT    NOT NULL,
  last_used_ns INTEGER NOT NULL,
  PRIMARY KEY (blake3, extractor)
) WITHOUT ROWID;

CREATE TABLE brief (
  tree_root  BLOB PRIMARY KEY,
  text       TEXT    NOT NULL,
  created_ns INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Whether the two databases describe the same graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pairing {
    /// `cache.db` was written against this `memory.db`; `file_state` is trustworthy.
    Matched,
    /// `file_state` was cleared (or never stamped): `memory.db` is new, was deleted, or was
    /// replaced. `extraction` rows are kept — they are keyed by content, not by graph.
    Reset,
}

fn configure(conn: &Connection) -> Result<(), MemError> {
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn enable_wal(conn: &Connection) -> Result<(), MemError> {
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if mode.eq_ignore_ascii_case("wal") {
        Ok(())
    } else {
        Err(MemError::Corrupt {
            path: conn.path().map(Into::into).unwrap_or_default(),
            reason: format!("could not enable WAL (journal_mode is {mode})"),
        })
    }
}

fn user_version(conn: &Connection) -> Result<i64, MemError> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, MemError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE name = ?1 AND type IN ('table', 'view')",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A fresh identifier. Uniqueness, not secrecy, is the requirement, so process-seeded hashing
/// plus the clock and pid is enough.
fn new_epoch() -> String {
    let mut bytes = [0u8; 24];
    for chunk in bytes.chunks_mut(8) {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    blake3::hash(&bytes).to_hex().as_str()[..32].to_owned()
}

/// Opens the graph database for writing, creating and initialising it when new.
///
/// A database is kept only if it is at the current schema **and** carries a stamp made with `key`.
/// Anything else — a version-1 store, a database a repository shipped, one made under another key —
/// is discarded and rebuilt. That loses nothing irreplaceable at schema 1, and it is the only way to
/// be sure rows the model will read were written by this harness.
///
/// # Errors
/// Fails on I/O or SQLite errors, on a schema newer than this build understands, and on a
/// database that has tables but no recognisable version.
pub fn open_memory(path: &Path, key: &MemKey) -> Result<Connection, MemError> {
    Ok(open_memory_with_reason(path, key)?.0)
}

/// Opens the graph and reports when an existing database had to be discarded.
///
/// # Errors
/// The same errors as [`open_memory`].
pub fn open_memory_with_reason(
    path: &Path,
    key: &MemKey,
) -> Result<(Connection, Option<MemoryResetReason>), MemError> {
    touch_private(path)?;
    let conn = Connection::open(path)?;
    configure(&conn)?;
    enable_wal(&conn)?;
    match user_version(&conn)? {
        MEMORY_SCHEMA_VERSION => {
            if memory_stamp_valid(&conn, key) {
                Ok((conn, None))
            } else {
                Ok((
                    rebuild_memory(conn, path, key)?,
                    Some(MemoryResetReason::Untrusted),
                ))
            }
        }
        0 => {
            if table_exists(&conn, "meta")? || table_exists(&conn, "node")? {
                return Err(MemError::Corrupt {
                    path: path.to_owned(),
                    reason: "has tables but no schema version".to_owned(),
                });
            }
            create_memory(&conn, key)?;
            Ok((conn, None))
        }
        found if found > MEMORY_SCHEMA_VERSION => Err(MemError::UnsupportedSchema {
            path: path.to_owned(),
            found,
            supported: MEMORY_SCHEMA_VERSION,
        }),
        _ => Ok((
            rebuild_memory(conn, path, key)?,
            Some(MemoryResetReason::SchemaVersion),
        )),
    }
}

fn create_memory(conn: &Connection, key: &MemKey) -> Result<(), MemError> {
    let epoch = new_epoch();
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(MEMORY_DDL)?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES
           ('graph_epoch', ?1), ('repo_generation', '0'), ('index_complete', '0'),
           (?2, ?3)",
        rusqlite::params![epoch, STAMP_KEY, key.stamp("memory", &epoch)],
    )?;
    tx.pragma_update(None, "user_version", MEMORY_SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

fn rebuild_memory(conn: Connection, path: &Path, key: &MemKey) -> Result<Connection, MemError> {
    drop(conn);
    remove_db_files(path)?;
    touch_private(path)?;
    let conn = Connection::open(path)?;
    configure(&conn)?;
    enable_wal(&conn)?;
    create_memory(&conn, key)?;
    Ok(conn)
}

fn memory_stamp_valid(conn: &Connection, key: &MemKey) -> bool {
    let epoch = get_meta(conn, "meta", "graph_epoch").ok().flatten();
    let stamp = get_meta(conn, "meta", STAMP_KEY).ok().flatten();
    matches!((epoch, stamp), (Some(epoch), Some(stamp)) if key.stamp_matches("memory", &epoch, &stamp))
}

/// Opens a read-only connection to the graph. The writer must have opened it first, so that the
/// WAL index exists.
///
/// # Errors
/// Fails when the database cannot be opened read-only.
pub fn open_memory_reader(path: &Path) -> Result<Connection, MemError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(conn)
}

fn remove_db_files(path: &Path) -> Result<(), MemError> {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match fs::remove_file(Path::new(&name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(MemError::io(Path::new(&name), e)),
        }
    }
    Ok(())
}

/// Opens the disposable cache database. A version mismatch, or a missing or foreign stamp,
/// deletes and recreates it: extraction records are read back into the graph, so a cache a
/// repository shipped is as untrusted as a graph it shipped.
///
/// # Errors
/// Fails on I/O or SQLite errors.
pub fn open_cache(path: &Path, key: &MemKey) -> Result<Connection, MemError> {
    touch_private(path)?;
    let conn = Connection::open(path)?;
    configure(&conn)?;
    enable_wal(&conn)?;
    if user_version(&conn)? == CACHE_SCHEMA_VERSION && cache_stamp_valid(&conn, key) {
        return Ok(conn);
    }
    // Disposable by design: anything not at the current version is rebuilt, never migrated.
    drop(conn);
    remove_db_files(path)?;
    touch_private(path)?;
    let conn = Connection::open(path)?;
    configure(&conn)?;
    enable_wal(&conn)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CACHE_DDL)?;
    tx.execute(
        "INSERT INTO cache_meta(key, value) VALUES (?1, ?2)",
        [STAMP_KEY, &key.stamp("cache", "")],
    )?;
    tx.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(conn)
}

fn cache_stamp_valid(conn: &Connection, key: &MemKey) -> bool {
    get_meta(conn, "cache_meta", STAMP_KEY)
        .ok()
        .flatten()
        .is_some_and(|stamp| key.stamp_matches("cache", "", &stamp))
}

pub(crate) fn get_meta(
    conn: &Connection,
    table: &str,
    key: &str,
) -> Result<Option<String>, MemError> {
    let sql = format!("SELECT value FROM {table} WHERE key = ?1");
    Ok(conn.query_row(&sql, [key], |r| r.get(0)).optional()?)
}

pub(crate) fn set_meta(
    conn: &Connection,
    table: &str,
    key: &str,
    value: &str,
) -> Result<(), MemError> {
    let sql = format!(
        "INSERT INTO {table}(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    );
    conn.execute(&sql, [key, value])?;
    Ok(())
}

/// Reconciles `cache.db` with the `memory.db` it is used beside (design § 3.2, rule 1).
///
/// If the cache was not written against this graph's epoch, its `file_state` is cleared — a
/// stale "already indexed" claim about a graph that no longer holds the result is exactly the
/// silent loss this guards against — and the cache is stamped with the current epoch.
/// `extraction` rows are untouched, which is why a rebuilt graph needs no reparsing.
///
/// # Errors
/// Fails on SQLite errors or when `memory.db` has no epoch.
pub fn pair(memory: &Connection, cache: &mut Connection) -> Result<Pairing, MemError> {
    let epoch = get_meta(memory, "meta", "graph_epoch")?.ok_or_else(|| MemError::Corrupt {
        path: memory.path().map(Into::into).unwrap_or_default(),
        reason: "no graph_epoch".to_owned(),
    })?;
    if get_meta(cache, "cache_meta", "graph_epoch")?.as_deref() == Some(epoch.as_str()) {
        return Ok(Pairing::Matched);
    }
    let tx = cache.transaction()?;
    tx.execute("DELETE FROM file_state", [])?;
    set_meta(&tx, "cache_meta", "graph_epoch", &epoch)?;
    tx.commit()?;
    Ok(Pairing::Reset)
}
