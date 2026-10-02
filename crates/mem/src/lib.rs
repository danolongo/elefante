//! Graph memory for repositories. Design: `docs/M2-DESIGN.md`.

pub mod actor;
pub mod error;
pub mod layout;
pub mod repo;
pub mod schema;
pub mod store;
pub mod terms;

pub use error::MemError;
pub use layout::StoreDir;
pub use repo::RepoId;
pub use schema::Pairing;
pub use store::Store;

/// Marker describing why the `memory_search` tool has no backing store yet.
pub const NOT_AVAILABLE: &str = "memory search is not available until milestone M2";
