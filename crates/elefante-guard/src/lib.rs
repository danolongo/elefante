//! Deterministic policy primitives for filesystem access, shell commands, and output.
//!
//! This crate decides whether an operation is acceptable, but never performs it.

mod bash;
mod path;
mod scrub;

pub use bash::{BashClassifier, BashDecision, Verdict};
pub use path::{PathPolicy, PolicyError};
pub use scrub::Scrubber;
