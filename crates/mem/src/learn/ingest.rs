//! Turning verified candidates into rows: the one transaction in which a learned note is added,
//! reinforced, promoted or superseded.
//!
//! The model proposes and this code disposes. A near-identical wording is always a duplicate,
//! whatever the model said; a model verdict (`same_as`, `replaces`) is honoured only when it
//! names a note it was actually shown, of the same kind, that is lexically related, and a
//! `replaces` additionally needs the user's own words as evidence. Everything is re-checked here,
//! inside the writer's transaction, against current state, so two sessions extracting the same
//! fact converge on one note.

use std::collections::{BTreeSet, HashSet};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{
    CONFIDENCE_CAP, Candidate, DUP_JACCARD, MAX_ANCHORS, MAX_CANDIDATES, Origin, REINFORCE_GAIN,
    RELATED_OVERLAP, Relation, Status, Turn,
    sim::{self, tokens},
    store::{self, LearnedNode, NewNode},
    verify::{Reject, verify},
};
use crate::{
    error::MemError,
    graph::{bump_generation_only, generation},
    trust::MemKey,
};

pub use super::verify::Reject as Rejection;

/// What a caller hands to [`ingest`].
#[derive(Clone, Copy, Debug)]
pub struct IngestInput<'a> {
    pub candidates: &'a [Candidate],
    /// The conversation text the candidates were drawn from.
    pub excerpt: &'a [Turn],
    /// Ids of the notes the model was shown, in handle order: `m1` is `shown[0]`.
    pub shown: &'a [i64],
}

/// What one ingest did. Counters, not prose: tests and the soft report read these.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub added: Vec<i64>,
    pub reinforced: Vec<i64>,
    /// Pending notes that became active.
    pub promoted: Vec<i64>,
    /// `(new, old)`.
    pub superseded: Vec<(i64, i64)>,
    /// Superseded notes the user brought back.
    pub revived: Vec<i64>,
    pub rejected: Vec<(usize, Rejection)>,
    /// Duplicates of existing notes that carried no user evidence, so changed nothing.
    pub ignored: usize,
    /// Model verdicts that failed verification and were treated as `new`.
    pub verdicts_rejected: usize,
    /// Anchors that named nothing in the graph.
    pub anchors_dropped: usize,
    /// Stored learned rows that failed their MAC and were treated as absent.
    pub mac_failures: u64,
    pub generation: i64,
    pub changed: bool,
}

struct Live {
    node: LearnedNode,
    tokens: BTreeSet<String>,
}

/// The note key: a hash of the kind and the statement as first written.
fn note_key(kind: super::Kind, statement: &str) -> String {
    let folded = statement
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let digest = blake3::hash(format!("{}\0{folded}", kind.as_str()).as_bytes());
    format!("m:{}", &digest.to_hex()[..16])
}

fn note_tokens(label: &str, statement: &str) -> BTreeSet<String> {
    tokens(&format!("{label} {statement}"))
}

/// Adds, reinforces or supersedes according to `input`, in one transaction.
///
/// # Errors
/// SQLite errors; the transaction is rolled back.
pub fn ingest(
    memory: &mut Connection,
    key: &MemKey,
    input: &IngestInput<'_>,
    now_ns: i64,
) -> Result<IngestReport, MemError> {
    let tx = memory.transaction()?;
    let mut report = IngestReport::default();
    let (nodes, failed) = store::load_active_and_pending(&tx, key)?;
    report.mac_failures = failed;
    let mut live: Vec<Live> = nodes
        .into_iter()
        .map(|node| Live {
            tokens: note_tokens(&node.label, &node.statement),
            node,
        })
        .collect();

    for (index, candidate) in input.candidates.iter().take(MAX_CANDIDATES).enumerate() {
        let verified = match verify(candidate, input.excerpt) {
            Ok(verified) => verified,
            Err(reason) => {
                report.rejected.push((index, reason));
                continue;
            }
        };
        let candidate = &verified.candidate;
        let candidate_tokens = note_tokens(&candidate.label, &candidate.statement);
        if candidate_tokens.is_empty() {
            report.rejected.push((index, Reject::Empty));
            continue;
        }
        handle(
            &tx,
            key,
            input,
            &mut live,
            &mut report,
            candidate,
            &candidate_tokens,
            verified.origin,
            now_ns,
        )?;
    }

    report.changed = !(report.added.is_empty()
        && report.reinforced.is_empty()
        && report.promoted.is_empty()
        && report.superseded.is_empty()
        && report.revived.is_empty());
    report.generation = if report.changed {
        bump_generation_only(&tx)?
    } else {
        generation(&tx)?
    };
    tx.commit()?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn handle(
    tx: &Transaction<'_>,
    key: &MemKey,
    input: &IngestInput<'_>,
    live: &mut Vec<Live>,
    report: &mut IngestReport,
    candidate: &Candidate,
    candidate_tokens: &BTreeSet<String>,
    origin: Origin,
    now_ns: i64,
) -> Result<(), MemError> {
    // 1. A near-identical wording is a duplicate, whatever the model said.
    let duplicate = live
        .iter()
        .enumerate()
        .filter(|(_, l)| l.node.kind == candidate.kind)
        .filter(|(_, l)| sim::is_duplicate(candidate_tokens, &l.tokens, DUP_JACCARD))
        .max_by(|a, b| {
            sim::jaccard(candidate_tokens, &a.1.tokens)
                .total_cmp(&sim::jaccard(candidate_tokens, &b.1.tokens))
                .then(b.1.node.id.cmp(&a.1.node.id))
        })
        .map(|(i, _)| i);
    if let Some(i) = duplicate {
        reinforce(tx, key, live, i, origin, report, now_ns)?;
        return Ok(());
    }

    // 2. A verdict from the model, if it survives verification.
    let anchors = resolve_anchors(tx, &candidate.anchors, report)?;
    let target = match candidate.relation {
        Relation::New => None,
        Relation::SameAs(h) | Relation::Replaces(h) => {
            let found = verified_target(tx, input, live, h, candidate, candidate_tokens, &anchors)?;
            if found.is_none() {
                report.verdicts_rejected += 1;
            }
            found
        }
    };
    if let (Relation::SameAs(_), Some(i)) = (candidate.relation, target) {
        reinforce(tx, key, live, i, origin, report, now_ns)?;
        return Ok(());
    }
    // A replacement needs the user's own words. Without them the note is added alongside.
    let replaces = match (candidate.relation, target) {
        (Relation::Replaces(_), Some(i)) if origin.is_user() => Some(i),
        (Relation::Replaces(_), Some(_)) => {
            report.verdicts_rejected += 1;
            None
        }
        _ => None,
    };

    // 3. A new note (or the revival of one the user brought back).
    let id = add_or_revive(
        tx,
        key,
        live,
        candidate,
        candidate_tokens,
        &anchors,
        origin,
        report,
        now_ns,
    )?;
    if let (Some(i), Some(new_id)) = (replaces, id) {
        let old = live[i].node.id;
        if old != new_id {
            supersede(tx, new_id, old, now_ns)?;
            report.superseded.push((new_id, old));
            live.retain(|l| l.node.id != old);
        }
    }
    Ok(())
}

fn reinforce(
    tx: &Transaction<'_>,
    key: &MemKey,
    live: &mut [Live],
    index: usize,
    origin: Origin,
    report: &mut IngestReport,
    now_ns: i64,
) -> Result<(), MemError> {
    // Only the user's own words can raise a note. This is what stops the assistant restating an
    // injected note and getting it re-confirmed.
    if !origin.is_user() {
        report.ignored += 1;
        return Ok(());
    }
    let node = &mut live[index].node;
    let confidence = (1.0 - (1.0 - node.confidence) * (1.0 - REINFORCE_GAIN)).min(CONFIDENCE_CAP);
    tx.execute(
        "UPDATE node SET confidence = ?2, last_reinforced_ns = ?3, updated_at_ns = ?3,
                attrs = json_set(attrs, '$.reinforcements',
                                 COALESCE(json_extract(attrs, '$.reinforcements'), 0) + 1)
         WHERE id = ?1",
        params![node.id, confidence, now_ns],
    )?;
    node.confidence = confidence;
    node.last_reinforced_ns = now_ns;
    report.reinforced.push(node.id);
    if node.status == Status::Pending {
        // Promotion re-signs the row under the user's origin, so provenance stays truthful.
        let mac = key.row_mac(&crate::trust::RowFields {
            kind: node.kind.as_str(),
            key: &node.key,
            name: &node.label,
            body: &node.statement,
            source: origin.source(),
            created_at_ns: node.created_at_ns,
        });
        tx.execute(
            "UPDATE node SET status = 'active', source = ?2, mac = ?3 WHERE id = ?1",
            params![node.id, origin.source(), mac.as_slice()],
        )?;
        store::put_fts(tx, node.id)?;
        node.status = Status::Active;
        node.origin = origin;
        report.promoted.push(node.id);
    }
    Ok(())
}

fn resolve_anchors(
    tx: &Transaction<'_>,
    anchors: &[String],
    report: &mut IngestReport,
) -> Result<Vec<i64>, MemError> {
    let mut ids = Vec::new();
    for anchor in anchors.iter().take(MAX_ANCHORS) {
        match store::resolve_anchor(tx, anchor.trim())? {
            Some((id, _)) if !ids.contains(&id) => ids.push(id),
            Some(_) => {}
            None => report.anchors_dropped += 1,
        }
    }
    report.anchors_dropped += anchors.len().saturating_sub(MAX_ANCHORS);
    Ok(ids)
}

/// Checks a `same_as`/`replaces` verdict and returns the index in `live` of the note it names.
fn verified_target(
    tx: &Transaction<'_>,
    input: &IngestInput<'_>,
    live: &[Live],
    handle: usize,
    candidate: &Candidate,
    candidate_tokens: &BTreeSet<String>,
    anchors: &[i64],
) -> Result<Option<usize>, MemError> {
    let Some(&id) = handle.checked_sub(1).and_then(|h| input.shown.get(h)) else {
        return Ok(None);
    };
    let Some(index) = live.iter().position(|l| l.node.id == id) else {
        return Ok(None);
    };
    let target = &live[index];
    if target.node.kind != candidate.kind {
        return Ok(None);
    }
    if sim::overlap(candidate_tokens, &target.tokens) >= RELATED_OVERLAP {
        return Ok(Some(index));
    }
    if anchors.is_empty() {
        return Ok(None);
    }
    let mut statement = tx.prepare("SELECT dst FROM edge WHERE src = ?1 AND rel = 'about'")?;
    let theirs: HashSet<i64> = statement
        .query_map([id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(anchors.iter().any(|a| theirs.contains(a)).then_some(index))
}

#[allow(clippy::too_many_arguments)]
fn add_or_revive(
    tx: &Transaction<'_>,
    key: &MemKey,
    live: &mut Vec<Live>,
    candidate: &Candidate,
    candidate_tokens: &BTreeSet<String>,
    anchors: &[i64],
    origin: Origin,
    report: &mut IngestReport,
    now_ns: i64,
) -> Result<Option<i64>, MemError> {
    let note = note_key(candidate.kind, &candidate.statement);
    let status = if origin.is_user() {
        Status::Active
    } else {
        Status::Pending
    };

    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT id, status FROM node WHERE kind = ?1 AND key = ?2",
            params![candidate.kind.as_str(), note],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let id = match existing {
        Some((old_id, old_status)) if old_status == "superseded" && origin.is_user() => {
            // The user brought back something they had retired. Re-sign it under the new origin.
            let created: i64 = tx.query_row(
                "SELECT created_at_ns FROM node WHERE id = ?1",
                [old_id],
                |r| r.get(0),
            )?;
            let mac = key.row_mac(&crate::trust::RowFields {
                kind: candidate.kind.as_str(),
                key: &note,
                name: &candidate.label,
                body: &candidate.statement,
                source: origin.source(),
                created_at_ns: created,
            });
            tx.execute(
                "UPDATE node SET status = 'active', source = ?2, mac = ?3, name = ?4, body = ?5,
                        confidence = ?6, last_reinforced_ns = ?7, updated_at_ns = ?7 WHERE id = ?1",
                params![
                    old_id,
                    origin.source(),
                    mac.as_slice(),
                    candidate.label,
                    candidate.statement,
                    origin.initial_confidence(),
                    now_ns
                ],
            )?;
            tx.execute(
                "DELETE FROM edge WHERE dst = ?1 AND rel = 'supersedes'",
                [old_id],
            )?;
            report.revived.push(old_id);
            old_id
        }
        Some((_, old_status)) if old_status == "superseded" => {
            // The assistant cannot bring back what the user retired.
            report.ignored += 1;
            return Ok(None);
        }
        Some((old_id, _)) => {
            // Same key, but not a note we can build on (unverifiable, or the assistant's claim
            // about something the user retired). Replace it so the unique key is free.
            store::delete_node(tx, old_id)?;
            live.retain(|l| l.node.id != old_id);
            insert_fresh(tx, key, candidate, &note, origin, status, now_ns)?
        }
        None => insert_fresh(tx, key, candidate, &note, origin, status, now_ns)?,
    };
    if !report.revived.contains(&id) {
        report.added.push(id);
    }
    for anchor in anchors {
        store::add_about_edge(tx, id, *anchor, now_ns)?;
    }
    if status == Status::Active {
        store::put_fts(tx, id)?;
    }
    live.push(Live {
        node: LearnedNode {
            id,
            kind: candidate.kind,
            key: note,
            label: candidate.label.clone(),
            statement: candidate.statement.clone(),
            origin,
            status,
            confidence: origin.initial_confidence(),
            last_reinforced_ns: now_ns,
            created_at_ns: now_ns,
            updated_at_ns: now_ns,
            reinforcements: 0,
        },
        tokens: candidate_tokens.clone(),
    });
    Ok(Some(id))
}

fn insert_fresh(
    tx: &Transaction<'_>,
    key: &MemKey,
    candidate: &Candidate,
    note: &str,
    origin: Origin,
    status: Status,
    now_ns: i64,
) -> Result<i64, MemError> {
    store::insert_node(
        tx,
        key,
        &NewNode {
            kind: candidate.kind,
            key: note,
            label: &candidate.label,
            statement: &candidate.statement,
            origin,
            status,
            quote: &candidate.quote,
        },
        now_ns,
    )
}

fn supersede(tx: &Transaction<'_>, new: i64, old: i64, now_ns: i64) -> Result<(), MemError> {
    tx.execute(
        "INSERT OR IGNORE INTO edge(src, dst, rel, weight, created_at_ns)
         VALUES (?1, ?2, 'supersedes', 1.0, ?3)",
        params![new, old, now_ns],
    )?;
    tx.execute(
        "UPDATE node SET status = 'superseded', updated_at_ns = ?2 WHERE id = ?1",
        params![old, now_ns],
    )?;
    store::drop_fts(tx, old)?;
    Ok(())
}
