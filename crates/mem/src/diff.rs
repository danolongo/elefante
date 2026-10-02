//! Deciding which walked files need reading. Design: `docs/M2-DESIGN.md` § 6.1.

use std::{
    collections::{HashMap, HashSet},
    hash::BuildHasher,
};

use crate::{
    state::StateRow,
    stats::Counters,
    walk::{Walk, WalkedFile},
};

/// A file whose mtime is within this window of the moment it was last hashed cannot be trusted
/// on stat alone.
///
/// This is git's "racy timestamp" problem: an edit that lands within the filesystem's timestamp
/// granularity of the previous hash, and leaves the size unchanged, is invisible to a stat
/// comparison forever. Such entries are rehashed. A file left alone for longer than this window
/// is then skipped on stat from the next refresh on.
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

#[derive(Debug, Default)]
pub struct Diff {
    /// New, stat-changed, or racy: these must be read and hashed.
    pub to_hash: Vec<WalkedFile>,
    /// Remembered but no longer present. Meaningful only when the walk was complete.
    pub deleted: Vec<String>,
}

/// Compares a walk with `file_state`.
///
/// A file is skipped without being read only when a row exists, its size and mtime both match,
/// and its mtime is older than the racy window before the row's `indexed_at_ns`.
#[must_use]
pub fn diff<S: BuildHasher>(
    walk: &Walk,
    state: &HashMap<String, StateRow, S>,
    counters: &Counters,
) -> Diff {
    let mut result = Diff::default();
    let mut skipped = 0u64;
    for file in &walk.files {
        let trusted = state.get(&file.path).is_some_and(|row| {
            row.size == file.size
                && row.mtime_ns == file.mtime_ns
                && file.mtime_ns < row.indexed_at_ns.saturating_sub(RACY_WINDOW_NS)
        });
        if trusted {
            skipped += 1;
        } else {
            result.to_hash.push(file.clone());
        }
    }
    counters.add_stat_unchanged(skipped);
    if walk.complete {
        let seen: HashSet<&str> = walk.files.iter().map(|f| f.path.as_str()).collect();
        let mut deleted: Vec<String> = state
            .keys()
            .filter(|path| !seen.contains(path.as_str()))
            .cloned()
            .collect();
        deleted.sort();
        result.deleted = deleted;
    }
    result
}
