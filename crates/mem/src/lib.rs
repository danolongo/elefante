//! Graph memory for repositories. Design: `docs/M2-DESIGN.md`.

pub mod actor;
pub mod diff;
pub mod error;
pub mod hash;
pub mod layout;
pub mod repo;
pub mod schema;
pub mod state;
pub mod stats;
pub mod store;
pub mod terms;
pub mod walk;

pub use error::MemError;
pub use layout::StoreDir;
pub use repo::RepoId;
pub use schema::Pairing;
pub use stats::{Counters, IndexStats};
pub use store::Store;

/// Marker describing why the `memory_search` tool has no backing store yet.
pub const NOT_AVAILABLE: &str = "memory search is not available until milestone M2";
