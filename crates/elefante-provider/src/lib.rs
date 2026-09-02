//! Provider-neutral request and streaming response types.
//!
//! Provider wire formats stay behind [`Provider`], so the agent loop only has
//! to understand one message model and one event stream.

pub mod anthropic;

use std::fmt;

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Features that may vary by provider and model.
// Booleans are intentional independent capability axes, not exclusive states.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    pub adaptive_thinking: bool,
    pub effort: bool,
    pub prompt_caching: bool,
    pub mid_conversation_system: bool,
    pub structured_output: bool,
    pub cache_metrics: bool,
}

/// A provider-specific model identifier kept opaque to the core.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: ModelId,
    pub max_tokens: u32,
    pub system: Vec<String>,
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<Message>,
    pub effort: Option<Effort>,
}

impl Request {
    #[must_use]
    pub fn new(model: impl Into<ModelId>, max_tokens: u32, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            system: Vec::new(),
            tools: Vec::new(),
            messages,
            effort: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![ContentBlock::Text { text: text.into() }])
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// A complete Anthropic thinking block retained for exact conversation replay.
    Thinking {
        thinking: String,
        signature: String,
    },
    /// An opaque encrypted thinking block retained for exact conversation replay.
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    ThinkingDelta(String),
    /// Delivers replay state to the loop; frontends must not render this event.
    ThinkingBlockComplete {
        thinking: String,
        signature: String,
    },
    /// Delivers opaque replay state to the loop; frontends must not render this event.
    RedactedThinkingBlockComplete {
        data: String,
    },
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInputDelta(String),
    ToolUseComplete {
        id: String,
        input: Value,
    },
    Usage(Usage),
    Stop(StopReason),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl Usage {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    StopSequence,
    Other(String),
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("provider returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("provider protocol error: {0}")]
    Protocol(String),
    #[error("provider returned invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid provider configuration: {0}")]
    Configuration(String),
}

pub trait Provider: Send + Sync {
    fn capabilities(&self, model: &ModelId) -> Capabilities;

    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event, ProviderError>>;
}
