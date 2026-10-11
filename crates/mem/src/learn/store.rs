//! Reading and writing learned rows. Every row read back is MAC-checked; a row that fails is
//! treated as if it were not there.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use super::{KINDS_SQL, Kind, Origin, SOURCE_PREFIX, Status, sim};
use crate::{
    error::MemError,
    terms::{path_terms, split_words},
    trust::{MemKey, RowFields},
};

/// A learned note as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct LearnedNode {
    pub id: i64,
    pub kind: Kind,
    pub key: String,
    pub label: String,
    pub statement: String,
    pub origin: Origin,
    pub status: Status,
    pub confidence: f64,
    pub last_reinforced_ns: i64,
    pub created_at_ns: i64,
    pub updated_at_ns: i64,
    pub reinforcements: u32,
}

pub(super) const SELECT_COLUMNS: &str = "id, kind, key, name, body, source, status, confidence,
     COALESCE(last_reinforced_ns, created_at_ns), created_at_ns, updated_at_ns, mac,
     COALESCE(json_extract(attrs, '$.reinforcements'), 0)";

/// Reads a row selected with [`SELECT_COLUMNS`]. `None` when it is malformed or its MAC is wrong.
pub(super) fn node_from_row(row: &Row<'_>, key: &MemKey) -> rusqlite::Result<Option<LearnedNode>> {
    let kind: String = row.get(1)?;
    let source: String = row.get(5)?;
    let status: String = row.get(6)?;
    let mac: Option<Vec<u8>> = row.get(11)?;
    let node_key: String = row.get(2)?;
    let label: String = row.get(3)?;
    let statement: String = row.get(4)?;
    let created: i64 = row.get(9)?;
    let verified = mac.is_some_and(|mac| {
        key.row_mac_matches(
            &RowFields {
                kind: &kind,
                key: &node_key,
                name: &label,
                body: &statement,
                source: &source,
                created_at_ns: created,
            },
            &mac,
        )
    });
    let (Some(kind), Some(origin), Some(status), true) = (
        Kind::parse(&kind),
        Origin::from_source(&source),
        Status::parse(&status),
        verified,
    ) else {
        return Ok(None);
    };
    Ok(Some(LearnedNode {
        id: row.get(0)?,
        kind,
        key: node_key,
        label,
        statement,
        origin,
        status,
        confidence: row.get(7)?,
        last_reinforced_ns: row.get(8)?,
        created_at_ns: created,
        updated_at_ns: row.get(10)?,
        reinforcements: u32::try_from(row.get::<_, i64>(12)?).unwrap_or(0),
    }))
}

/// Loads learned rows with one of `statuses`. Returns the nodes that verified and the number that
/// did not.
pub(super) fn load(
    conn: &Connection,
    key: &MemKey,
    statuses: &[Status],
) -> Result<(Vec<LearnedNode>, u64), MemError> {
    let list = statuses
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM node
         WHERE kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%' AND status IN ({list})
         ORDER BY id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], |row| node_from_row(row, key))?;
    let (mut nodes, mut failed) = (Vec::new(), 0);
    for row in rows {
        match row? {
            Some(node) => nodes.push(node),
            None => failed += 1,
        }
    }
    Ok((nodes, failed))
}

/// Every active and pending learned note that verifies, and how many failed verification.
///
/// # Errors
/// SQLite errors.
pub fn load_active_and_pending(
    conn: &Connection,
    key: &MemKey,
) -> Result<(Vec<LearnedNode>, u64), MemError> {
    load(conn, key, &[Status::Active, Status::Pending])
}

/// The active and pending notes that share the most words with `text`, best first, for showing a
/// model what is already known. Ties break on id so the same inputs give the same list.
///
/// # Errors
/// SQLite errors.
pub fn nearest(
    conn: &Connection,
    key: &MemKey,
    text: &str,
    limit: usize,
) -> Result<Vec<LearnedNode>, MemError> {
    let wanted = sim::tokens(text);
    let (nodes, _) = load_active_and_pending(conn, key)?;
    let mut scored: Vec<(f64, f64, LearnedNode)> = nodes
        .into_iter()
        .filter_map(|node| {
            let have = sim::tokens(&format!("{} {}", node.label, node.statement));
            let shared = sim::overlap(&wanted, &have);
            (shared > 0.0).then(|| (shared, sim::jaccard(&wanted, &have), node))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.id.cmp(&b.2.id))
    });
    Ok(scored.into_iter().take(limit).map(|(_, _, n)| n).collect())
}

// ----- writes, always inside the caller's transaction -----

pub(super) struct NewNode<'a> {
    pub kind: Kind,
    pub key: &'a str,
    pub label: &'a str,
    pub statement: &'a str,
    pub origin: Origin,
    pub status: Status,
    pub quote: &'a str,
}

pub(super) fn insert_node(
    tx: &Transaction<'_>,
    key: &MemKey,
    new: &NewNode<'_>,
    now_ns: i64,
) -> Result<i64, MemError> {
    let mac = key.row_mac(&RowFields {
        kind: new.kind.as_str(),
        key: new.key,
        name: new.label,
        body: new.statement,
        source: new.origin.source(),
        created_at_ns: now_ns,
    });
    let attrs = serde_json::json!({
        "evidence": { "quote": new.quote },
        "reinforcements": 0,
    })
    .to_string();
    tx.execute(
        "INSERT INTO node(kind, key, name, body, attrs, confidence, source, created_at_ns,
                          updated_at_ns, status, last_reinforced_ns, mac)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?8, ?10)",
        params![
            new.kind.as_str(),
            new.key,
            new.label,
            new.statement,
            attrs,
            new.origin.initial_confidence(),
            new.origin.source(),
            now_ns,
            new.status.as_str(),
            mac.as_slice(),
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// The file and symbol a repository path or `path::qname` anchor names, if the graph has one.
pub(super) fn resolve_anchor(
    tx: &Transaction<'_>,
    anchor: &str,
) -> Result<Option<(i64, String)>, MemError> {
    let found: Option<(i64, String, Option<String>)> = tx
        .query_row(
            "SELECT n.id, n.key, f.key FROM node n LEFT JOIN node f ON f.id = n.file_id
             WHERE n.key = ?1 AND n.kind IN ('file', 'symbol') AND n.source = 'index'",
            [anchor],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(found.map(|(id, key, file)| (id, file.unwrap_or(key))))
}

pub(super) fn add_about_edge(
    tx: &Transaction<'_>,
    from: i64,
    to: i64,
    now_ns: i64,
) -> Result<(), MemError> {
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, rel, weight, created_at_ns)
         VALUES (?1, ?2, 'about', 1.0, ?3)",
        params![from, to, now_ns],
    )?;
    Ok(())
}

/// The repository paths a note is anchored to.
fn anchored_paths(tx: &Transaction<'_>, id: i64) -> Result<Vec<String>, MemError> {
    let mut statement = tx.prepare(
        "SELECT COALESCE(f.key, n.key) FROM edge e JOIN node n ON n.id = e.dst
         LEFT JOIN node f ON f.id = n.file_id
         WHERE e.src = ?1 AND e.rel = 'about' ORDER BY 1",
    )?;
    let paths = statement
        .query_map([id], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(paths)
}

/// Gives a note its full-text entry. Only active notes have one.
pub(super) fn put_fts(tx: &Transaction<'_>, id: i64) -> Result<(), MemError> {
    let (label, statement): (String, String) =
        tx.query_row("SELECT name, body FROM node WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let paths = anchored_paths(tx, id)?
        .iter()
        .map(|p| path_terms(p))
        .collect::<Vec<_>>()
        .join(" ");
    drop_fts(tx, id)?;
    tx.execute(
        "INSERT INTO node_fts(rowid, name_terms, path_terms, body_terms) VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            split_words(&label).join(" "),
            paths,
            split_words(&statement).join(" ")
        ],
    )?;
    Ok(())
}

pub(super) fn drop_fts(tx: &Transaction<'_>, id: i64) -> Result<(), MemError> {
    tx.execute("DELETE FROM node_fts WHERE rowid = ?1", [id])?;
    Ok(())
}

/// Deletes a note, its full-text entry and (by cascade) its edges.
pub(super) fn delete_node(tx: &Transaction<'_>, id: i64) -> Result<(), MemError> {
    drop_fts(tx, id)?;
    tx.execute("DELETE FROM node WHERE id = ?1", [id])?;
    Ok(())
}

// ----- reads for retrieval -----

use std::collections::HashMap;

/// Active learned notes with these ids that verify, in id order.
pub(crate) fn load_active_by_ids(
    conn: &Connection,
    key: &MemKey,
    ids: &[i64],
) -> Result<Vec<LearnedNode>, MemError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM node
         WHERE id IN ({list}) AND kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%'
           AND status = 'active' ORDER BY id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], |row| node_from_row(row, key))?;
    let mut nodes = Vec::new();
    for row in rows {
        nodes.extend(row?);
    }
    Ok(nodes)
}

/// The top `limit` active learned notes matching a full-text expression, best first.
pub(crate) fn seed_ids(
    conn: &Connection,
    expression: &str,
    limit: usize,
) -> Result<Vec<i64>, MemError> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT n.id FROM node_fts f JOIN node n ON n.id = f.rowid
         WHERE node_fts MATCH ?1 AND n.status = 'active'
           AND n.kind IN {KINDS_SQL} AND n.source LIKE '{SOURCE_PREFIX}%'
         ORDER BY bm25(node_fts, 10.0, 4.0, 1.0), n.key
         LIMIT ?2"
    ))?;
    let ids = statement
        .query_map(
            params![expression, i64::try_from(limit).unwrap_or(i64::MAX)],
            |r| r.get(0),
        )?
        .collect::<Result<_, _>>()?;
    Ok(ids)
}

/// `(learned, code)` pairs for `about` edges leaving the given learned notes.
pub(crate) fn about_targets(
    conn: &Connection,
    learned: &[i64],
) -> Result<Vec<(i64, i64)>, MemError> {
    if learned.is_empty() {
        return Ok(Vec::new());
    }
    let list = learned
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = conn.prepare(&format!(
        "SELECT src, dst FROM edge WHERE src IN ({list}) AND rel = 'about' ORDER BY src, dst"
    ))?;
    let pairs = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(pairs)
}

/// `(code, learned)` pairs for active learned notes anchored at the given code nodes.
pub(crate) fn learned_about(conn: &Connection, code: &[i64]) -> Result<Vec<(i64, i64)>, MemError> {
    if code.is_empty() {
        return Ok(Vec::new());
    }
    let list = code
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = conn.prepare(&format!(
        "SELECT e.dst, e.src FROM edge e JOIN node s ON s.id = e.src
         WHERE e.dst IN ({list}) AND e.rel = 'about' AND s.status = 'active'
           AND s.kind IN {KINDS_SQL} AND s.source LIKE '{SOURCE_PREFIX}%'
         ORDER BY e.dst, e.src"
    ))?;
    let pairs = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(pairs)
}

/// Repository paths each learned note is anchored to (a symbol anchor reports its file).
pub(crate) fn anchor_paths(
    conn: &Connection,
    learned: &[i64],
) -> Result<HashMap<i64, Vec<String>>, MemError> {
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    if learned.is_empty() {
        return Ok(out);
    }
    let list = learned
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = conn.prepare(&format!(
        "SELECT e.src, COALESCE(f.key, n.key) FROM edge e JOIN node n ON n.id = e.dst
         LEFT JOIN node f ON f.id = n.file_id
         WHERE e.src IN ({list}) AND e.rel = 'about' ORDER BY e.src, 2"
    ))?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (id, path) = row?;
        out.entry(id).or_default().push(path);
    }
    Ok(out)
}
