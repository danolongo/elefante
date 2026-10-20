//! The `elefante` binary's library: the wire contract, the daemon, and the headless client.
//!
//! One binary plays both roles. `elefante daemon` runs [`daemon`]; every other invocation is a
//! client that speaks [`wire`] to a daemon over a Unix socket, starting one by running its own
//! executable when none is listening.

pub mod client;
pub mod daemon;
pub mod wire;
