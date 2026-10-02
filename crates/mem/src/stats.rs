//! Counters the gate asserts on. Gates assert counts, never timing (`PLAN.md`).

use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time copy of the indexer's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Regular files examined by the walk.
    pub walked: u64,
    /// Files skipped on stat alone, never read.
    pub stat_unchanged: u64,
    /// `blake3` computations (a file was read in full).
    pub hashed: u64,
    /// Extractions satisfied from `cache.db`.
    pub extraction_hits: u64,
    /// Tree-sitter parse invocations.
    pub parsed: u64,
    /// `memory.db` batch commits.
    pub batches: u64,
}

#[derive(Debug, Default)]
pub struct Counters {
    walked: AtomicU64,
    stat_unchanged: AtomicU64,
    hashed: AtomicU64,
    extraction_hits: AtomicU64,
    parsed: AtomicU64,
    batches: AtomicU64,
}

macro_rules! bump {
    ($name:ident, $field:ident) => {
        pub fn $name(&self, by: u64) {
            self.$field.fetch_add(by, Ordering::Relaxed);
        }
    };
}

impl Counters {
    bump!(add_walked, walked);
    bump!(add_stat_unchanged, stat_unchanged);
    bump!(add_hashed, hashed);
    bump!(add_extraction_hits, extraction_hits);
    bump!(add_parsed, parsed);
    bump!(add_batches, batches);

    #[must_use]
    pub fn snapshot(&self) -> IndexStats {
        IndexStats {
            walked: self.walked.load(Ordering::Relaxed),
            stat_unchanged: self.stat_unchanged.load(Ordering::Relaxed),
            hashed: self.hashed.load(Ordering::Relaxed),
            extraction_hits: self.extraction_hits.load(Ordering::Relaxed),
            parsed: self.parsed.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
        }
    }
}
