//! Unix-socket daemon, durable session state, and bounded client delivery.

mod credentials;
mod queue;
mod search;
mod server;
#[cfg(debug_assertions)]
mod stub;

pub use server::{DaemonConfig, LearnMode, run, run_from_env, socket_path, token_path};

pub const RING_CAPACITY: usize = 4096;
pub const CLIENT_QUEUE_CAPACITY: usize = 256;
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
