//! Graph memory for repositories. Design: `docs/M2-DESIGN.md`.

pub mod actor;
pub mod brief;
pub mod cochange;
pub mod diff;
pub mod error;
pub mod extract;
pub mod graph;
pub mod hash;
pub mod indexer;
pub mod layout;
pub mod repo;
pub mod resolve;
pub mod retrieve;
pub mod schema;
pub mod state;
pub mod stats;
pub mod store;
pub mod terms;
mod util;
pub mod walk;

pub use error::MemError;
pub use indexer::Indexer;
pub use layout::StoreDir;
pub use repo::RepoId;
pub use schema::Pairing;
pub use stats::{Counters, IndexStats};
pub use store::Store;

/// Marker describing why the `memory_search` tool has no backing store yet.
pub const NOT_AVAILABLE: &str = "memory search is not available until milestone M2";
