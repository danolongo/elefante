//! One synchronous refresh: walk, diff, hash, extract, materialise. Design: `docs/M2-DESIGN.md` § 2.
//!
//! The expensive work — hashing and parsing — runs on worker threads without touching a
//! database; the database is reached in short calls to the writer actor. Each batch commits to
//! `memory.db` first and `cache.db` second (§ 3.2 rule 2): a crash between the two leaves the
//! cache behind the graph, so the files are re-examined next time and re-upserted identically.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use crate::{
    cochange::{self, CoChangeConfig},
    diff::diff,
    error::MemError,
    extract::{self, Extraction, Language, cache},
    graph::{self, Applied, Batch, FileUpdate},
    hash::hash_files,
    schema,
    state::{self, StateRow},
    stats::Counters,
    store::Store,
    util::{now_ns, par_map},
    walk::{Walk, WalkedFile, walk},
};

/// Observation and fault-injection points. The default does nothing. Tests use them to stop the
/// indexer between the two commits, or to hold it mid-run, without any `cfg(test)` leaking into
/// the library.
pub trait Hooks: Send + Sync {
    /// Called before a batch is built. `index` counts batches from zero within a refresh.
    fn before_batch(&self, _index: usize) {}

    /// Called after a batch has committed to `memory.db` and before `cache.db` is written.
    /// Returning an error abandons the refresh at exactly that point, as a crash would.
    ///
    /// # Errors
    /// Whatever the hook chooses to inject.
    fn after_graph_commit(&self) -> Result<(), MemError> {
        Ok(())
    }
}

struct NoHooks;
impl Hooks for NoHooks {}

#[derive(Clone)]
pub struct IndexOptions {
    /// Files per `memory.db` commit. Each commit is one generation bump.
    pub batch_files: usize,
    pub cochange: CoChangeConfig,
    pub hooks: Arc<dyn Hooks>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            batch_files: 200,
            cochange: CoChangeConfig::default(),
            hooks: Arc::new(NoHooks),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Files whose content changed and were written to the graph.
    pub updated: Vec<String>,
    /// Files removed from the graph.
    pub removed: Vec<String>,
    pub complete: bool,
    pub truncated: bool,
    pub generation: i64,
}

/// The extractor family a path belongs to, stored in `file.attrs` as `lang`.
fn family(extractor_id: &str) -> &str {
    extractor_id.split('@').next().unwrap_or(extractor_id)
}

/// A freshly computed extraction, to be stored once its graph batch has committed.
type Fresh = ([u8; 32], &'static str, Extraction);

struct Pending {
    file: WalkedFile,
    blake3: [u8; 32],
    bytes: Vec<u8>,
    language: Option<&'static dyn Language>,
}

/// Brings the graph in line with the repository at `root`.
///
/// # Errors
/// I/O and SQLite failures, and anything a hook injects. A file that cannot be read is skipped,
/// not an error.
///
/// # Panics
/// Only if a worker thread panics.
pub fn refresh_blocking(
    store: &Store,
    root: &Path,
    counters: &Counters,
    options: &IndexOptions,
) -> Result<RefreshReport, MemError> {
    let walked = walk(root, counters)?;
    let prior = store.writer().call_blocking(|s| state::load(&s.cache))??;
    let plan = diff(&walked, &prior, counters);

    let mut report = RefreshReport {
        complete: walked.complete,
        truncated: walked.truncated,
        ..RefreshReport::default()
    };
    let batch_files = options.batch_files.max(1);
    for (index, files) in plan.to_hash.chunks(batch_files).enumerate() {
        options.hooks.before_batch(index);
        process_batch(store, root, files, &prior, counters, options, &mut report)?;
    }
    finish(store, &walked, &plan.deleted, counters, &mut report)?;
    cochange_phase(store, root, counters, options, &mut report)?;
    Ok(report)
}

/// Re-derives co-change edges when, and only when, `HEAD` has moved since they were computed.
///
/// Uncommitted work has no history, so it cannot change the answer: an ordinary edit-and-refresh
/// never reruns `git log`. `HEAD` is read with a cheap `rev-parse`; the head the edges were
/// computed at is stored in the same transaction as the edges, so a crash cannot leave them
/// claiming a head they were not computed for. Not a repository, no `git`, or a timeout all mean
/// "no co-change edges", never an error.
fn cochange_phase(
    store: &Store,
    root: &Path,
    counters: &Counters,
    options: &IndexOptions,
    report: &mut RefreshReport,
) -> Result<(), MemError> {
    let Some(head) = cochange::head(root) else {
        return Ok(());
    };
    let (stored, current) = store.writer().call_blocking(|s| {
        let stored = schema::get_meta(&s.memory, "meta", "cochange_head")?;
        let mut statement = s
            .memory
            .prepare("SELECT key FROM node WHERE kind = 'file' AND source = 'index'")?;
        let current: HashSet<String> = statement
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok::<_, MemError>((stored, current))
    })??;
    if stored.as_deref() == Some(head.as_str()) {
        return Ok(());
    }
    counters.add_git_log_runs(1);
    let Some(commits) = cochange::log(root, &options.cochange, cochange::GIT_TIMEOUT) else {
        return Ok(());
    };
    let pairs = cochange::compute(&commits, &current, &options.cochange);
    let stamp = now_ns();
    let applied = store
        .writer()
        .call_blocking(move |s| graph::replace_cochange(&mut s.memory, &pairs, &head, stamp))??;
    if applied.changed {
        counters.add_batches(1);
    }
    store.note_generation(applied.generation);
    report.generation = applied.generation;
    Ok(())
}

fn process_batch(
    store: &Store,
    root: &Path,
    files: &[WalkedFile],
    prior: &HashMap<String, StateRow>,
    counters: &Counters,
    options: &IndexOptions,
    report: &mut RefreshReport,
) -> Result<(), MemError> {
    let hashed = hash_files(root, files, counters);
    let stamp = now_ns();
    let mut rows = Vec::new();
    let mut deletes = Vec::new();
    let mut pending = Vec::new();

    for (file, result) in files.iter().zip(hashed) {
        // Unreadable now: leave whatever is recorded about it alone and look again next time.
        let Ok(hashed) = result else { continue };
        let before = prior.get(&file.path);
        let row = |skip| StateRow {
            path: file.path.clone(),
            size: file.size,
            mtime_ns: file.mtime_ns,
            blake3: hashed.blake3,
            indexed_at_ns: stamp,
            skip,
        };
        if hashed.binary {
            if before.is_some_and(|b| !b.skip) {
                deletes.push(file.path.clone());
            }
            rows.push(row(true));
        } else if before.is_some_and(|b| !b.skip && b.blake3 == hashed.blake3) {
            // Touched, not changed: the stat is refreshed and nothing downstream runs.
            rows.push(row(false));
        } else {
            rows.push(row(false));
            pending.push(Pending {
                file: file.clone(),
                blake3: hashed.blake3,
                bytes: hashed.bytes,
                language: extract::language_for(&file.path),
            });
        }
    }

    let (updates, fresh) = extract_all(store, &pending, counters)?;
    report
        .updated
        .extend(updates.iter().map(|u| u.path.clone()));
    report.removed.extend(deletes.iter().cloned());
    commit_batch(
        store,
        Batch { updates, deletes },
        rows,
        fresh,
        counters,
        options,
    )?;
    Ok(())
}

/// Resolves each pending file to a graph update, reading the extraction cache first and parsing
/// only the misses. Returns the updates and the extractions that were freshly computed.
fn extract_all(
    store: &Store,
    pending: &[Pending],
    counters: &Counters,
) -> Result<(Vec<FileUpdate>, Vec<Fresh>), MemError> {
    let keys: Vec<([u8; 32], &'static str)> = pending
        .iter()
        .filter_map(|p| p.language.map(|l| (p.blake3, l.id())))
        .collect();
    let stamp = now_ns();
    let cached: Vec<Option<Extraction>> = store.writer().call_blocking(move |s| {
        keys.iter()
            .map(|(hash, id)| cache::lookup(&s.cache, hash, id, stamp))
            .collect::<Result<Vec<_>, MemError>>()
    })??;

    // Pair each pending file with its cache outcome, then parse the misses in parallel.
    let mut cached = cached.into_iter();
    let work: Vec<(&Pending, Option<Extraction>)> = pending
        .iter()
        .map(|p| (p, p.language.and_then(|_| cached.next().flatten())))
        .collect();
    for (_, hit) in &work {
        if hit.is_some() {
            counters.add_extraction_hits(1);
        }
    }
    let computed = par_map(&work, |(p, hit)| match (p.language, hit) {
        (Some(_), Some(extraction)) => (Some(extraction.clone()), false),
        (Some(language), None) => {
            let source = String::from_utf8_lossy(&p.bytes);
            (Some(language.extract(&source, counters)), true)
        }
        (None, _) => (None, false),
    });

    let mut updates = Vec::with_capacity(pending.len());
    let mut fresh = Vec::new();
    for ((p, _), (extraction, is_new)) in work.iter().zip(computed) {
        let extraction = extraction.unwrap_or_default();
        if is_new && let Some(language) = p.language {
            fresh.push((p.blake3, language.id(), extraction.clone()));
        }
        updates.push(FileUpdate {
            path: p.file.path.clone(),
            language: p.language.map(|l| family(l.id()).to_owned()),
            size: p.file.size,
            file_doc: extraction.file_doc,
            symbols: extraction.symbols,
            imports: extraction.imports,
            truncated: extraction.truncated,
        });
    }
    Ok((updates, fresh))
}

/// Commits a batch: graph first, then the cache. The order is the whole point.
fn commit_batch(
    store: &Store,
    batch: Batch,
    rows: Vec<StateRow>,
    fresh: Vec<Fresh>,
    counters: &Counters,
    options: &IndexOptions,
) -> Result<Applied, MemError> {
    let hooks = Arc::clone(&options.hooks);
    let stamp = now_ns();
    let applied = store.writer().call_blocking(move |s| {
        // A batch that only refreshes stat data (a touched file) has nothing for the graph.
        let applied = if batch.updates.is_empty() && batch.deletes.is_empty() {
            Applied {
                generation: graph::generation(&s.memory)?,
                ..Applied::default()
            }
        } else {
            graph::apply_batch(&mut s.memory, &batch, stamp)?
        };
        hooks.after_graph_commit()?;
        let tx = s.cache.transaction()?;
        state::upsert(&tx, &rows)?;
        for (hash, id, extraction) in &fresh {
            cache::store(&tx, hash, id, extraction, stamp)?;
        }
        tx.commit()?;
        Ok::<_, MemError>(applied)
    })??;
    store.note_generation(applied.generation);
    if applied.changed {
        counters.add_batches(1);
    }
    Ok(applied)
}

/// Deletions, the orphan sweep, and the bookkeeping that closes a refresh.
fn finish(
    store: &Store,
    walked: &Walk,
    deleted: &[String],
    counters: &Counters,
    report: &mut RefreshReport,
) -> Result<(), MemError> {
    let stamp = now_ns();
    let walked_paths: Vec<String> = walked.files.iter().map(|f| f.path.clone()).collect();
    let complete = walked.complete;
    let deleted = deleted.to_vec();
    report.removed.extend(deleted.iter().cloned());
    let truncated = walked.truncated;

    let (generation, changed) = store.writer().call_blocking(move |s| {
        let mut applied = None;
        if complete {
            // A walked file the cache has marked binary has no node: it is not "live".
            let states = state::load(&s.cache)?;
            let live: HashSet<String> = walked_paths
                .into_iter()
                .filter(|p| !states.get(p).is_some_and(|row| row.skip))
                .collect();
            applied = Some(graph::apply_removals(
                &mut s.memory,
                &deleted,
                Some(&live),
                stamp,
            )?);
            // Graph first, then the cache: same order as a batch.
            let tx = s.cache.transaction()?;
            state::delete(&tx, &deleted)?;
            tx.commit()?;
        }
        s.memory.execute(
            "INSERT INTO meta(key, value) VALUES ('truncated', ?1), ('index_complete', ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![
                if truncated { "1" } else { "0" },
                if complete { "1" } else { "0" }
            ],
        )?;
        if complete {
            cache::gc(&s.cache, stamp, cache::GC_MAX_AGE_NS)?;
        }
        let mut changed = applied.as_ref().is_some_and(|a| a.changed);
        let mut generation = match applied {
            Some(a) => a.generation,
            None => graph::generation(&s.memory)?,
        };
        // Edges are derived last, from the final set of paths. The dirty flag is persistent, so
        // this also repairs a refresh that was interrupted after its content batches committed.
        if graph::edges_dirty(&s.memory)? {
            let edges = graph::resolve_edges(&mut s.memory, stamp)?;
            changed |= edges.changed;
            generation = edges.generation;
        }
        Ok::<_, MemError>((generation, changed))
    })??;
    if changed {
        counters.add_batches(1);
    }
    store.note_generation(generation);
    store.index_state().set_complete(complete);
    report.generation = generation;
    Ok(())
}
