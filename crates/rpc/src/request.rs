use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;

pub const UNKNOWN_SESSION: i32 = -32001;
pub const TURN_IN_PROGRESS: i32 = -32002;
pub const INVALID_SINCE: i32 = -32003;
pub const NO_API_KEY: i32 = -32004;
pub const UNKNOWN_APPROVAL: i32 = -32005;
pub const UNKNOWN_NOTE: i32 = -32006;
pub const MEMORY_OFF: i32 = -32007;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
pub const PARSE_ERROR: i32 = -32700;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Request {
    pub id: Value,
    #[serde(flatten)]
    pub call: Method,
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::de::Error::custom("expected request object"))?;
        let id = object
            .remove("id")
            .ok_or_else(|| serde::de::Error::custom("request requires id"))?;
        if !matches!(id, Value::Null | Value::String(_) | Value::Number(_)) {
            return Err(serde::de::Error::custom(
                "id must be a string, number, or null",
            ));
        }
        let call = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self { id, call })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Method {
    #[serde(rename = "session.create")]
    SessionCreate(CreateParams),
    #[serde(rename = "session.prompt")]
    SessionPrompt(PromptParams),
    #[serde(rename = "session.interrupt")]
    SessionInterrupt(SessionParams),
    #[serde(rename = "session.attach")]
    SessionAttach(AttachParams),
    #[serde(rename = "session.close")]
    SessionClose(SessionParams),
    #[serde(rename = "session.resolve_approval")]
    SessionResolveApproval(ResolveApprovalParams),
    #[serde(rename = "key.set")]
    KeySet(KeySetParams),
    #[serde(rename = "memory.consent")]
    MemoryConsent(MemoryConsentParams),
    #[serde(rename = "memory.list")]
    MemoryList(MemoryListParams),
    #[serde(rename = "memory.forget")]
    MemoryForget(MemoryIdParams),
    #[serde(rename = "memory.approve")]
    MemoryApprove(MemoryIdParams),
    #[serde(rename = "memory.clear")]
    MemoryClear(MemoryCwdParams),
    #[serde(rename = "memory.status")]
    MemoryStatus(MemoryCwdParams),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateParams {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptParams {
    pub session_id: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionParams {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveApprovalParams {
    pub session_id: String,
    pub approval_id: String,
    pub decision: ApprovalDecision,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySetParams {
    pub api_key: String,
}

impl fmt::Debug for KeySetParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeySetParams")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryConsentAnswer {
    Learn,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsentParams {
    pub answer: MemoryConsentAnswer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCwdParams {
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryIdParams {
    pub cwd: String,
    pub id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryListParams {
    pub cwd: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Response {
    pub id: Value,
    #[serde(flatten)]
    pub payload: ResponsePayload,
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| serde::de::Error::custom("expected response object"))?;
        let id = object
            .remove("id")
            .ok_or_else(|| serde::de::Error::custom("response requires id"))?;
        if !matches!(id, Value::Null | Value::String(_) | Value::Number(_)) {
            return Err(serde::de::Error::custom(
                "id must be a string, number, or null",
            ));
        }
        let payload = match (object.remove("result"), object.remove("error")) {
            (Some(result), None) => ResponsePayload::Result(result),
            (None, Some(error)) => ResponsePayload::Error(
                serde_json::from_value(error).map_err(serde::de::Error::custom)?,
            ),
            _ => {
                return Err(serde::de::Error::custom(
                    "response requires exactly one result or error",
                ));
            }
        };
        Ok(Self { id, payload })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Result(Value),
    Error(RpcError),
}

impl Response {
    /// Build a successful response from a typed method result.
    ///
    /// # Errors
    /// Returns an error if the result cannot be represented as JSON.
    pub fn success(id: Value, result: impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self {
            id,
            payload: ResponsePayload::Result(serde_json::to_value(result)?),
        })
    }

    #[must_use]
    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            id,
            payload: ResponsePayload::Error(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResult {
    pub session_id: String,
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemorySeed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryConsentState {
    Unset,
    Granted,
    Declined,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySeed {
    pub consent: MemoryConsentState,
    pub extractor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLearningState {
    Working,
    Paused,
    Failing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryIndexState {
    Off,
    Building,
    Ready,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryNoteCounts {
    pub active: u64,
    pub pending: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub enabled: bool,
    pub consent: MemoryConsentState,
    pub learning: MemoryLearningState,
    pub extractor: Option<String>,
    pub notes: MemoryNoteCounts,
    pub index: MemoryIndexState,
    pub spend: crate::Usage,
    pub problem: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryNote {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub statement: String,
    pub origin: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryChange {
    Added,
    Promoted,
    Superseded,
    Revived,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRow {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub statement: String,
    pub origin: String,
    pub confidence: f64,
    pub verified: bool,
    pub reinforcements: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryListResult {
    pub notes: Vec<MemoryRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryClearResult {
    pub removed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsentResult {
    pub consent: MemoryConsentState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptResult {
    pub turn_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyResult {}
