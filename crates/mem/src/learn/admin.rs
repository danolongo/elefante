//! What a person can do with what was learned: look at it, approve the assistant's unconfirmed
//! claims, and remove anything. These are the controls that make persisted free-form text
//! acceptable to inject.

use rusqlite::{Connection, params};

use super::{
    CONFIDENCE_STATED, KINDS_SQL, Origin, SOURCE_PREFIX, Status, display_safe,
    effective_confidence,
    store::{self, SELECT_COLUMNS},
};
use crate::{
    error::MemError,
    graph::{bump_generation_only, generation},
    schema::STAMP_KEY,
    trust::{MemKey, RowFields},
};

/// A learned row as shown to a person. Unlike what retrieval sees, this includes rows that fail
/// verification, flagged, so they can be removed.
#[derive(Clone, Debug, PartialEq)]
pub struct LearnedRow {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub origin: String,
    pub label: String,
    pub statement: String,
    pub confidence: f64,
    pub effective_confidence: f64,
    pub reinforcements: u32,
    pub verified: bool,
}

/// Whether a read-only memory connection carries this installation's keyed store stamp.
///
/// Administrative readers use this before displaying any database-supplied text. A missing table,
/// epoch, stamp, or foreign stamp is untrusted rather than an error worth exposing to the person.
#[must_use]
pub fn store_stamp_valid(conn: &Connection, key: &MemKey) -> bool {
    let value = |name| {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [name], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    };
    matches!(
        (value("graph_epoch"), value(STAMP_KEY)),
        (Some(epoch), Some(stamp)) if key.stamp_matches("memory", &epoch, &stamp)
    )
}

/// Lists learned rows: active and pending, or every status when `all`.
///
/// # Errors
/// SQLite errors.
pub fn list(
    conn: &Connection,
    key: &MemKey,
    all: bool,
    now_ns: i64,
) -> Result<Vec<LearnedRow>, MemError> {
    let filter = if all {
        String::new()
    } else {
        "AND status IN ('active','pending')".to_owned()
    };
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM node
         WHERE kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%' {filter} ORDER BY id"
    );
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let verified = store::node_from_row(row, key)?;
        let kind: String = row.get(1)?;
        let confidence: f64 = row.get(7)?;
        let reinforced: i64 = row.get(8)?;
        let effective = Kind::parse(&kind).map_or(0.0, |k| {
            effective_confidence(confidence, k, reinforced, now_ns)
        });
        out.push(LearnedRow {
            id: row.get(0)?,
            kind: display_safe(&kind),
            status: display_safe(&row.get::<_, String>(6)?),
            origin: display_safe(&row.get::<_, String>(5)?),
            label: display_safe(&row.get::<_, String>(3)?),
            statement: display_safe(&row.get::<_, String>(4)?),
            confidence,
            effective_confidence: effective,
            reinforcements: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
            verified: verified.is_some(),
        });
    }
    Ok(out)
}

use super::Kind;

fn is_learned(tx: &rusqlite::Transaction<'_>, id: i64) -> Result<bool, MemError> {
    let count: i64 = tx.query_row(
        &format!(
            "SELECT count(*) FROM node
             WHERE id = ?1 AND kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%'"
        ),
        [id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Deletes one learned note. Returns whether it existed. Code nodes cannot be removed this way.
///
/// # Errors
/// SQLite errors.
pub fn forget(memory: &mut Connection, id: i64) -> Result<bool, MemError> {
    let tx = memory.transaction()?;
    if !is_learned(&tx, id)? {
        return Ok(false);
    }
    store::delete_node(&tx, id)?;
    bump_generation_only(&tx)?;
    tx.commit()?;
    Ok(true)
}

/// Turns a pending note into an active one, on the user's say-so. The row is re-signed under the
/// `stated` origin so its provenance records that a person confirmed it. Returns whether a pending
/// note with that id existed and verified.
///
/// # Errors
/// SQLite errors.
pub fn approve(
    memory: &mut Connection,
    key: &MemKey,
    id: i64,
    now_ns: i64,
) -> Result<bool, MemError> {
    let tx = memory.transaction()?;
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM node
         WHERE id = ?1 AND kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%'"
    );
    let node = {
        let mut statement = tx.prepare(&sql)?;
        let mut rows = statement.query([id])?;
        match rows.next()? {
            Some(row) => store::node_from_row(row, key)?,
            None => None,
        }
    };
    let Some(node) = node.filter(|n| n.status == Status::Pending) else {
        return Ok(false);
    };
    let origin = Origin::Stated;
    let mac = key.row_mac(&RowFields {
        kind: node.kind.as_str(),
        key: &node.key,
        name: &node.label,
        body: &node.statement,
        source: origin.source(),
        created_at_ns: node.created_at_ns,
    });
    tx.execute(
        "UPDATE node SET status = 'active', source = ?2, mac = ?3, confidence = ?4,
                last_reinforced_ns = ?5, updated_at_ns = ?5 WHERE id = ?1",
        params![
            id,
            origin.source(),
            mac.as_slice(),
            CONFIDENCE_STATED,
            now_ns
        ],
    )?;
    store::put_fts(&tx, id)?;
    bump_generation_only(&tx)?;
    tx.commit()?;
    Ok(true)
}

/// Deletes every learned note. Returns how many.
///
/// # Errors
/// SQLite errors.
pub fn clear(memory: &mut Connection) -> Result<usize, MemError> {
    let tx = memory.transaction()?;
    let ids: Vec<i64> = {
        let mut statement = tx.prepare(&format!(
            "SELECT id FROM node WHERE kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%'"
        ))?;
        statement
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for id in &ids {
        store::delete_node(&tx, *id)?;
    }
    if ids.is_empty() {
        generation(&tx)?;
    } else {
        bump_generation_only(&tx)?;
    }
    tx.commit()?;
    Ok(ids.len())
}
