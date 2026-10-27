use super::{MemoryStatus, Notification, SandboxStatus, StopReason, Usage, notification::is_false};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachResult {
    Resumed {
        from: u64,
        events: Vec<Notification>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory: Option<MemoryStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<SandboxStatus>,
    },
    Snapshot {
        seq: u64,
        snapshot: Box<SessionSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        memory: Option<MemoryStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<SandboxStatus>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub seq: u64,
    pub cwd: String,
    pub model: String,
    pub turns: Vec<CompletedTurn>,
    pub active_turn: Option<ActiveTurn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxStatus>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompletedTurn {
    pub turn_id: String,
    pub prompt: String,
    pub text: String,
    pub tools: Vec<ToolState>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_note_ids: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveTurn {
    pub turn_id: String,
    pub text_so_far: String,
    pub reasoning_so_far: String,
    pub tools: Vec<ToolState>,
    pub pending_approval: Option<PendingApproval>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_note_ids: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub command: String,
    pub rule: String,
    /// The command asked for network access; a reattached client shows the same badge.
    #[serde(default, skip_serializing_if = "is_false")]
    pub network: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolState {
    pub tool_call_id: String,
    pub name: String,
    pub state: ToolStatus,
    pub is_error: bool,
}
