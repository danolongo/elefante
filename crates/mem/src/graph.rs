//! Writing the graph. See `docs/PLAN.md` § Memory subsystem, and this crate's README for the contract.
//!
//! Everything here runs inside one `memory.db` transaction per call and bumps
//! `meta.repo_generation` at most once, and only if a row actually changed. Identity is stable:
//! a file node is updated in place and a symbol keeps its id as long as its key does, because a
//! learned edge into either must survive an edit.

use std::{
    collections::{HashMap, HashSet},
    hash::BuildHasher,
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

use crate::{
    error::MemError,
    extract::{Import, Symbol},
    terms::{identifier_terms, path_terms, split_words},
};

pub const SOURCE_INDEX: &str = "index";

/// Everything the graph holds about one file, derived from its content.
#[derive(Clone, Debug, Default)]
pub struct FileUpdate {
    pub path: String,
    /// Extractor family (`rust`, `typescript`, `tsx`, `markdown`), or `None` for a file with no
    /// extractor, which is searchable by path only.
    pub language: Option<String>,
    pub size: u64,
    pub file_doc: String,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<Import>,
    pub truncated: bool,
}

#[derive(Debug, Default)]
pub struct Batch {
    pub updates: Vec<FileUpdate>,
    /// Paths whose file nodes must be removed.
    pub deletes: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Applied {
    /// At least one row was inserted, updated or deleted.
    pub changed: bool,
    /// A file node was added or removed, which can change what other files' imports resolve to.
    pub path_set_changed: bool,
    /// Files whose raw imports changed.
    pub import_changed_files: Vec<String>,
    pub generation: i64,
}

/// The body stored for a symbol: its signature, then its doc sentence on a second line. Both are
/// single-line by construction, so the first `\n` is an unambiguous separator.
#[must_use]
pub fn symbol_body(signature: &str, doc: &str) -> String {
    if doc.is_empty() {
        signature.to_owned()
    } else {
        format!("{signature}\n{doc}")
    }
}

fn symbol_key(path: &str, qname: &str, ordinal: usize) -> String {
    if ordinal <= 1 {
        format!("{path}::{qname}")
    } else {
        format!("{path}::{qname}#{ordinal}")
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn bump_generation(tx: &Transaction<'_>, now_ns: i64) -> Result<i64, MemError> {
    tx.execute(
        "UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         WHERE key = 'repo_generation'",
        [],
    )?;
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('last_indexed_at_ns', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [now_ns.to_string()],
    )?;
    generation(tx)
}

/// Advances `repo_generation` without touching the indexing timestamp. Learned writes use this: they
/// change what retrieval returns, so cached blocks must expire, but they are not an index refresh.
pub(crate) fn bump_generation_only(tx: &Transaction<'_>) -> Result<i64, MemError> {
    tx.execute(
        "UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         WHERE key = 'repo_generation'",
        [],
    )?;
    generation(tx)
}

/// The current `repo_generation`.
///
/// # Errors
/// SQLite errors, or a missing or malformed value.
pub fn generation(conn: &Connection) -> Result<i64, MemError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'repo_generation'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| MemError::Corrupt {
            path: conn.path().map(Into::into).unwrap_or_default(),
            reason: "missing repo_generation".to_owned(),
        })
}

fn insert_fts(
    tx: &Transaction<'_>,
    id: i64,
    name_terms: &str,
    path_terms: &str,
    body: &str,
) -> Result<(), MemError> {
    tx.prepare_cached(
        "INSERT INTO node_fts(rowid, name_terms, path_terms, body_terms) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        id,
        name_terms,
        path_terms,
        split_words(body).join(" ")
    ])?;
    Ok(())
}

fn delete_fts(tx: &Transaction<'_>, id: i64) -> Result<(), MemError> {
    tx.prepare_cached("DELETE FROM node_fts WHERE rowid = ?1")?
        .execute([id])?;
    Ok(())
}

/// Applies one batch of file updates and deletions atomically.
///
/// # Errors
/// SQLite errors; the transaction is rolled back.
pub fn apply_batch(
    memory: &mut Connection,
    batch: &Batch,
    now_ns: i64,
) -> Result<Applied, MemError> {
    let tx = memory.transaction()?;
    let mut applied = Applied::default();
    for path in &batch.deletes {
        if delete_file(&tx, path)? {
            applied.changed = true;
            applied.path_set_changed = true;
        }
    }
    for update in &batch.updates {
        upsert_file(&tx, update, now_ns, &mut applied)?;
    }
    mark_edges_dirty_if_needed(&tx, &applied)?;
    applied.generation = if applied.changed {
        bump_generation(&tx, now_ns)?
    } else {
        generation(&tx)?
    };
    tx.commit()?;
    Ok(applied)
}

/// Removes file nodes by path and, when `live` is given, every index-owned file node whose path
/// is not in it (the orphan sweep, design § 3.2 rule 3).
///
/// Only index-owned rows are ever touched: a node with any other `source` belongs to someone
/// else, and is left alone even if its file is gone.
///
/// # Errors
/// SQLite errors; the transaction is rolled back.
pub fn apply_removals<S: BuildHasher>(
    memory: &mut Connection,
    deletes: &[String],
    live: Option<&HashSet<String, S>>,
    now_ns: i64,
) -> Result<Applied, MemError> {
    let tx = memory.transaction()?;
    let mut applied = Applied::default();
    for path in deletes {
        if delete_file(&tx, path)? {
            applied.changed = true;
            applied.path_set_changed = true;
        }
    }
    if let Some(live) = live {
        let orphans: Vec<String> = {
            let mut statement =
                tx.prepare("SELECT key FROM node WHERE kind = 'file' AND source = ?1")?;
            statement
                .query_map([SOURCE_INDEX], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|key| !live.contains(key))
                .collect()
        };
        for path in orphans {
            if delete_file(&tx, &path)? {
                applied.changed = true;
                applied.path_set_changed = true;
            }
        }
    }
    mark_edges_dirty_if_needed(&tx, &applied)?;
    applied.generation = if applied.changed {
        bump_generation(&tx, now_ns)?
    } else {
        generation(&tx)?
    };
    tx.commit()?;
    Ok(applied)
}

/// Import edges depend on the set of paths and on every file's raw imports, so a change to either
/// makes them stale. The flag is written in the *same transaction* as the change: if the process
/// dies before edges are re-derived, the next run still knows, even though it will see every file
/// as already up to date.
fn mark_edges_dirty_if_needed(tx: &Transaction<'_>, applied: &Applied) -> Result<(), MemError> {
    if applied.path_set_changed || !applied.import_changed_files.is_empty() {
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('edges_dirty', '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            [],
        )?;
    }
    Ok(())
}

/// Whether import edges need re-deriving.
///
/// # Errors
/// SQLite errors.
pub fn edges_dirty(conn: &Connection) -> Result<bool, MemError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'edges_dirty'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value.as_deref() == Some("1"))
}

fn delete_file(tx: &Transaction<'_>, path: &str) -> Result<bool, MemError> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM node WHERE kind = 'file' AND key = ?1 AND source = ?2",
            params![path, SOURCE_INDEX],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(false);
    };
    // FTS rows are not covered by the node cascade, so they go first, for the file and for every
    // symbol the cascade is about to remove.
    let doomed: Vec<i64> = {
        let mut statement = tx.prepare("SELECT id FROM node WHERE id = ?1 OR file_id = ?1")?;
        statement
            .query_map([id], |r| r.get(0))?
            .collect::<Result<_, _>>()?
    };
    for node in doomed {
        delete_fts(tx, node)?;
    }
    tx.execute("DELETE FROM node WHERE id = ?1", [id])?;
    Ok(true)
}

struct ExistingNode {
    id: i64,
    name: String,
    body: String,
    attrs: String,
}

fn upsert_file(
    tx: &Transaction<'_>,
    update: &FileUpdate,
    now_ns: i64,
    applied: &mut Applied,
) -> Result<(), MemError> {
    let attrs = json!({
        "lang": update.language,
        "size": update.size,
        "truncated": update.truncated,
    })
    .to_string();
    let existing: Option<ExistingNode> = tx
        .query_row(
            "SELECT id, name, body, attrs FROM node WHERE kind = 'file' AND key = ?1",
            [&update.path],
            |r| {
                Ok(ExistingNode {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    body: r.get(2)?,
                    attrs: r.get(3)?,
                })
            },
        )
        .optional()?;
    let name = basename(&update.path);
    let file_id = if let Some(node) = existing {
        if node.body != update.file_doc || node.attrs != attrs || node.name != name {
            tx.execute(
                "UPDATE node SET name = ?2, body = ?3, attrs = ?4, updated_at_ns = ?5 WHERE id = ?1",
                params![node.id, name, update.file_doc, attrs, now_ns],
            )?;
            delete_fts(tx, node.id)?;
            insert_fts(
                tx,
                node.id,
                &path_terms(name),
                &path_terms(&update.path),
                &update.file_doc,
            )?;
            applied.changed = true;
        }
        node.id
    } else {
        tx.execute(
            "INSERT INTO node(kind, key, name, body, attrs, source, created_at_ns, updated_at_ns)
             VALUES ('file', ?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                update.path,
                name,
                update.file_doc,
                attrs,
                SOURCE_INDEX,
                now_ns
            ],
        )?;
        let id = tx.last_insert_rowid();
        insert_fts(
            tx,
            id,
            &path_terms(name),
            &path_terms(&update.path),
            &update.file_doc,
        )?;
        applied.changed = true;
        applied.path_set_changed = true;
        id
    };
    sync_symbols(tx, file_id, update, now_ns, applied)?;
    sync_imports(tx, file_id, update, applied)?;
    Ok(())
}

fn sync_symbols(
    tx: &Transaction<'_>,
    file_id: i64,
    update: &FileUpdate,
    now_ns: i64,
    applied: &mut Applied,
) -> Result<(), MemError> {
    let mut existing: HashMap<String, ExistingNode> = {
        let mut statement = tx.prepare(
            "SELECT key, id, name, body, attrs FROM node WHERE file_id = ?1 AND kind = 'symbol'",
        )?;
        statement
            .query_map([file_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    ExistingNode {
                        id: r.get(1)?,
                        name: r.get(2)?,
                        body: r.get(3)?,
                        attrs: r.get(4)?,
                    },
                ))
            })?
            .collect::<Result<_, _>>()?
    };

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for symbol in &update.symbols {
        let ordinal = seen
            .entry(symbol.qname.as_str())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        let key = symbol_key(&update.path, &symbol.qname, *ordinal);
        let body = symbol_body(&symbol.signature, &symbol.doc);
        let attrs = json!({
            "kind": symbol.kind,
            "line": symbol.line,
            "end_line": symbol.end_line,
            "via_trait": symbol.via_trait,
        })
        .to_string();
        let name_terms = identifier_terms(&symbol.qname);
        if let Some(node) = existing.remove(&key) {
            if node.name != symbol.name || node.body != body || node.attrs != attrs {
                tx.execute(
                    "UPDATE node SET name = ?2, body = ?3, attrs = ?4, updated_at_ns = ?5
                     WHERE id = ?1",
                    params![node.id, symbol.name, body, attrs, now_ns],
                )?;
                delete_fts(tx, node.id)?;
                insert_fts(tx, node.id, &name_terms, "", &body)?;
                applied.changed = true;
            }
        } else {
            tx.execute(
                "INSERT INTO node(kind, key, name, body, file_id, attrs, source,
                                  created_at_ns, updated_at_ns)
                 VALUES ('symbol', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![key, symbol.name, body, file_id, attrs, SOURCE_INDEX, now_ns],
            )?;
            let id = tx.last_insert_rowid();
            insert_fts(tx, id, &name_terms, "", &body)?;
            applied.changed = true;
        }
    }
    for (_, vanished) in existing {
        delete_fts(tx, vanished.id)?;
        tx.execute("DELETE FROM node WHERE id = ?1", [vanished.id])?;
        applied.changed = true;
    }
    Ok(())
}

fn sync_imports(
    tx: &Transaction<'_>,
    file_id: i64,
    update: &FileUpdate,
    applied: &mut Applied,
) -> Result<(), MemError> {
    let current: HashSet<(String, String)> = {
        let mut statement = tx.prepare("SELECT kind, spec FROM import_ref WHERE file_id = ?1")?;
        statement
            .query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let desired: HashSet<(String, String)> = update
        .imports
        .iter()
        .map(|i| (i.kind.clone(), i.spec.clone()))
        .collect();
    if current == desired {
        return Ok(());
    }
    tx.execute("DELETE FROM import_ref WHERE file_id = ?1", [file_id])?;
    let mut insert =
        tx.prepare_cached("INSERT INTO import_ref(file_id, kind, spec) VALUES (?1, ?2, ?3)")?;
    for (kind, spec) in &desired {
        insert.execute(params![file_id, kind, spec])?;
    }
    applied.changed = true;
    applied.import_changed_files.push(update.path.clone());
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct EdgeResolution {
    /// Edges were added or removed.
    pub changed: bool,
    pub edges: usize,
    /// Raw imports that resolved to no file in the repository.
    pub unresolved: usize,
    pub generation: i64,
}

/// Re-derives every `depends_on` edge between indexed files from the stored raw imports and the
/// current set of file nodes, writing only the difference.
///
/// Only edges whose two ends are both index-owned file nodes are considered. A `depends_on` edge
/// that involves any other node is somebody else's and is neither read nor changed.
///
/// # Errors
/// SQLite errors; the transaction is rolled back.
pub fn resolve_edges(memory: &mut Connection, now_ns: i64) -> Result<EdgeResolution, MemError> {
    let tx = memory.transaction()?;
    let files: HashMap<String, i64> = {
        let mut statement =
            tx.prepare("SELECT key, id FROM node WHERE kind = 'file' AND source = ?1")?;
        statement
            .query_map([SOURCE_INDEX], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let paths: HashSet<String> = files.keys().cloned().collect();

    let mut desired: HashSet<(i64, i64)> = HashSet::new();
    let mut unresolved = 0;
    {
        let mut statement = tx.prepare(
            "SELECT n.key, i.kind, i.spec FROM import_ref i
             JOIN node n ON n.id = i.file_id WHERE n.source = ?1",
        )?;
        let rows = statement.query_map([SOURCE_INDEX], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (from, kind, spec) = row?;
            match crate::resolve::resolve(&from, &kind, &spec, &paths) {
                Some(target) if target != from => {
                    if let (Some(&src), Some(&dst)) = (files.get(&from), files.get(&target)) {
                        desired.insert((src, dst));
                    }
                }
                Some(_) => {}
                None => unresolved += 1,
            }
        }
    }

    let existing: HashSet<(i64, i64)> = {
        let mut statement = tx.prepare(
            "SELECT e.src, e.dst FROM edge e
             JOIN node s ON s.id = e.src JOIN node d ON d.id = e.dst
             WHERE e.rel = 'depends_on'
               AND s.kind = 'file' AND s.source = ?1 AND d.kind = 'file' AND d.source = ?1",
        )?;
        statement
            .query_map([SOURCE_INDEX], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };

    let mut changed = false;
    for (src, dst) in existing.difference(&desired) {
        tx.execute(
            "DELETE FROM edge WHERE src = ?1 AND dst = ?2 AND rel = 'depends_on'",
            [src, dst],
        )?;
        changed = true;
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO edge(src, dst, rel, weight, created_at_ns)
             VALUES (?1, ?2, 'depends_on', 1.0, ?3)",
        )?;
        for (src, dst) in desired.difference(&existing) {
            insert.execute(params![src, dst, now_ns])?;
            changed = true;
        }
    }
    tx.execute("DELETE FROM meta WHERE key = 'edges_dirty'", [])?;
    let generation = if changed {
        bump_generation(&tx, now_ns)?
    } else {
        generation(&tx)?
    };
    tx.commit()?;
    Ok(EdgeResolution {
        changed,
        edges: desired.len(),
        unresolved,
        generation,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CoChangeApplied {
    pub changed: bool,
    pub edges: usize,
    pub generation: i64,
}

/// Replaces the `co_edited_with` edges between indexed files with `pairs`, writing only the
/// difference, and records the commit they were computed at.
///
/// The head is written in the same transaction as the edges, so "edges are current for this head"
/// is a single fact: a crash leaves either both or neither. Pairs naming a path that has no file
/// node are dropped. Storage order is by node id (`src < dst`), which the schema enforces.
///
/// # Errors
/// SQLite errors; the transaction is rolled back.
pub fn replace_cochange(
    memory: &mut Connection,
    pairs: &[crate::cochange::Pair],
    head: &str,
    now_ns: i64,
) -> Result<CoChangeApplied, MemError> {
    let tx = memory.transaction()?;
    let files: HashMap<String, i64> = {
        let mut statement =
            tx.prepare("SELECT key, id FROM node WHERE kind = 'file' AND source = ?1")?;
        statement
            .query_map([SOURCE_INDEX], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let desired: HashMap<(i64, i64), f64> = pairs
        .iter()
        .filter_map(|p| {
            let (a, b) = (*files.get(&p.a)?, *files.get(&p.b)?);
            Some(((a.min(b), a.max(b)), f64::from(p.weight)))
        })
        .collect();
    let existing: HashMap<(i64, i64), f64> = {
        let mut statement = tx.prepare(
            "SELECT e.src, e.dst, e.weight FROM edge e
             JOIN node s ON s.id = e.src JOIN node d ON d.id = e.dst
             WHERE e.rel = 'co_edited_with'
               AND s.kind = 'file' AND s.source = ?1 AND d.kind = 'file' AND d.source = ?1",
        )?;
        statement
            .query_map([SOURCE_INDEX], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut changed = false;
    for (key, weight) in &existing {
        if desired.get(key) != Some(weight) {
            tx.execute(
                "DELETE FROM edge WHERE src = ?1 AND dst = ?2 AND rel = 'co_edited_with'",
                [key.0, key.1],
            )?;
            changed = true;
        }
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO edge(src, dst, rel, weight, created_at_ns)
             VALUES (?1, ?2, 'co_edited_with', ?3, ?4)",
        )?;
        for (key, weight) in &desired {
            if existing.get(key) != Some(weight) {
                insert.execute(params![key.0, key.1, weight, now_ns])?;
                changed = true;
            }
        }
    }
    tx.execute(
        "INSERT INTO meta(key, value) VALUES ('cochange_head', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [head],
    )?;
    let generation = if changed {
        bump_generation(&tx, now_ns)?
    } else {
        generation(&tx)?
    };
    tx.commit()?;
    Ok(CoChangeApplied {
        changed,
        edges: desired.len(),
        generation,
    })
}

/// A comparison-friendly view of the whole graph with ids and timestamps removed, so two graphs
/// built by different routes can be asserted identical.
#[derive(Debug, PartialEq, Eq)]
pub struct GraphSnapshot {
    /// `(kind, key, name, body, attrs, source)`, sorted.
    pub nodes: Vec<(String, String, String, String, String, String)>,
    /// `(src key, dst key, rel, weight)`, sorted.
    pub edges: Vec<(String, String, String, String)>,
    /// `(file key, kind, spec)`, sorted.
    pub imports: Vec<(String, String, String)>,
}

/// Reads the graph without ids or timestamps.
///
/// # Errors
/// SQLite errors.
pub fn snapshot(conn: &Connection) -> Result<GraphSnapshot, MemError> {
    let nodes = conn
        .prepare("SELECT kind, key, name, body, attrs, source FROM node ORDER BY kind, key")?
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let edges = conn
        .prepare(
            "SELECT s.key, d.key, e.rel, CAST(e.weight AS TEXT) FROM edge e
             JOIN node s ON s.id = e.src JOIN node d ON d.id = e.dst
             ORDER BY s.key, d.key, e.rel",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let imports = conn
        .prepare(
            "SELECT n.key, i.kind, i.spec FROM import_ref i JOIN node n ON n.id = i.file_id
             ORDER BY n.key, i.kind, i.spec",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(GraphSnapshot {
        nodes,
        edges,
        imports,
    })
}
