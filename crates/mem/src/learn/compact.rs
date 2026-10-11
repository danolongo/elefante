//! Keeping the learned set small. Decay decides what stops being retrieved; compaction decides
//! what stops being stored.

use rusqlite::Connection;

use super::{
    CONFIDENCE_FLOOR, KINDS_SQL, NS_PER_DAY, SOURCE_PREFIX, Status, effective_confidence,
    store::{self, SELECT_COLUMNS, node_from_row},
};
use crate::{
    error::MemError,
    graph::{bump_generation_only, generation},
    schema::{get_meta, set_meta},
    trust::MemKey,
};

/// Learned rows beyond which compaction runs even if it ran recently.
pub const SOFT_CAP: usize = 400;
/// Active and pending notes kept after compaction; the lowest effective confidence go first.
pub const ACTIVE_CAP: usize = 300;
/// How long a superseded note is kept for inspection.
pub const SUPERSEDED_RETENTION_DAYS: i64 = 90;
/// At most this often, compaction also runs for notes that merely decayed below the floor.
pub const COMPACT_INTERVAL_NS: i64 = NS_PER_DAY;

const META_KEY: &str = "learned_compacted_ns";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactReport {
    pub below_floor: usize,
    pub expired_superseded: usize,
    pub over_cap: usize,
    /// Rows whose MAC did not verify; they are never shown, so they are removed.
    pub unverified: usize,
    pub generation: i64,
}

impl CompactReport {
    #[must_use]
    pub fn removed(&self) -> usize {
        self.below_floor + self.expired_superseded + self.over_cap + self.unverified
    }
}

fn learned_count(conn: &Connection) -> Result<usize, MemError> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM node WHERE kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%'"
        ),
        [],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(0))
}

/// Removes decayed, expired, excess and unverifiable learned rows, in one transaction.
///
/// # Errors
/// SQLite errors; the transaction is rolled back.
pub fn compact(
    memory: &mut Connection,
    key: &MemKey,
    now_ns: i64,
) -> Result<CompactReport, MemError> {
    let tx = memory.transaction()?;
    let mut report = CompactReport::default();

    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM node
         WHERE kind IN {KINDS_SQL} AND source LIKE '{SOURCE_PREFIX}%' ORDER BY id"
    );
    let mut verified = Vec::new();
    let mut doomed: Vec<i64> = Vec::new();
    {
        let mut statement = tx.prepare(&sql)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            if let Some(node) = node_from_row(row, key)? {
                verified.push(node);
            } else {
                report.unverified += 1;
                doomed.push(id);
            }
        }
    }

    let retention = SUPERSEDED_RETENTION_DAYS * NS_PER_DAY;
    let mut live = Vec::new();
    for node in verified {
        match node.status {
            Status::Superseded => {
                if node.updated_at_ns < now_ns - retention {
                    report.expired_superseded += 1;
                    doomed.push(node.id);
                }
            }
            Status::Active | Status::Pending => {
                let effective = effective_confidence(
                    node.confidence,
                    node.kind,
                    node.last_reinforced_ns,
                    now_ns,
                );
                if effective < CONFIDENCE_FLOOR {
                    report.below_floor += 1;
                    doomed.push(node.id);
                } else {
                    live.push((effective, node.id));
                }
            }
        }
    }
    if live.len() > ACTIVE_CAP {
        // Lowest effective confidence first; the older note loses a tie.
        live.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let excess = live.len() - ACTIVE_CAP;
        for (_, id) in live.iter().take(excess) {
            report.over_cap += 1;
            doomed.push(*id);
        }
    }

    for id in &doomed {
        store::delete_node(&tx, *id)?;
    }
    report.generation = if doomed.is_empty() {
        generation(&tx)?
    } else {
        bump_generation_only(&tx)?
    };
    set_meta(&tx, "meta", META_KEY, &now_ns.to_string())?;
    tx.commit()?;
    Ok(report)
}

/// Runs [`compact`] when the learned set is over its soft cap, or once a day.
///
/// # Errors
/// SQLite errors.
pub fn compact_if_due(
    memory: &mut Connection,
    key: &MemKey,
    now_ns: i64,
) -> Result<Option<CompactReport>, MemError> {
    let last: i64 = get_meta(memory, "meta", META_KEY)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let over = learned_count(memory)? > SOFT_CAP;
    if !over && now_ns - last < COMPACT_INTERVAL_NS {
        return Ok(None);
    }
    compact(memory, key, now_ns).map(Some)
}
