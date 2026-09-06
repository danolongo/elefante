//! The M1 JSON-RPC-shaped wire contract and bounded NDJSON framing.
mod framing;
mod notification;
mod request;
mod snapshot;

pub use elefante_provider::{StopReason, Usage};
pub use framing::*;
pub use notification::*;
pub use request::*;
pub use snapshot::*;

pub const SESSION_RING_CAPACITY: usize = 4096;
pub const CLIENT_QUEUE_CAPACITY: usize = 256;
