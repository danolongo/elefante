//! Wire-level primitives shared by future daemon and client implementations.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC-shaped request envelope. M1 will add the method catalog and transport.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}
