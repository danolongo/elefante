//! Provider-neutral request and streaming response types.
//!
//! Provider wire formats stay behind [`Provider`], so the agent loop only has
//! to understand one message model and one event stream. The crate ships a
//! single transport, [`OpenRouterProvider`].

pub mod openrouter;
pub mod sse;

use std::fmt;

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use openrouter::OpenRouterProvider;

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

/// One reasoning block, preserved exactly as the model produced it.
///
/// The replay rule is strict: the sequence of reasoning blocks must be returned
/// unmodified and unreordered. Lossless round-tripping therefore outranks
/// ergonomic access, so the original object is kept verbatim in [`Self::raw`].
/// A typed enum was rejected because unknown future `reasoning.*` kinds would
/// fail to round-trip, silently violating the replay rule rather than erroring.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReasoningDetail {
    /// Wire discriminator: `reasoning.text`, `reasoning.summary`, or `reasoning.encrypted`.
    pub kind: String,
    /// The complete original object. Replayed unmodified; never edited or reordered.
    pub raw: Value,
}

impl ReasoningDetail {
    /// Wraps a wire object, reading its `type` field as the discriminator.
    #[must_use]
    pub fn new(raw: Value) -> Self {
        Self {
            kind: raw
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            raw,
        }
    }

    /// Human-visible text, if this block carries any.
    ///
    /// Reads `.text` or `.summary`; encrypted blocks carry neither and return `None`.
    #[must_use]
    pub fn display_text(&self) -> Option<&str> {
        self.raw
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| self.raw.get("summary").and_then(Value::as_str))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Reasoning retained for exact conversation replay.
    ///
    /// Ordering within an assistant turn is significant: the blocks are replayed
    /// in the order the model produced them.
    Reasoning {
        details: Vec<ReasoningDetail>,
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
    /// Renderable reasoning fragments.
    ReasoningDelta(String),
    /// Delivers replay state to the loop; frontends must not render this event.
    ReasoningDetails(Vec<ReasoningDetail>),
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// `prompt_tokens`.
    pub input_tokens: u64,
    /// `completion_tokens`.
    pub output_tokens: u64,
    /// `completion_tokens_details.reasoning_tokens`.
    pub reasoning_tokens: u64,
    /// `prompt_tokens_details.cached_tokens` — the cache-hit signal.
    pub cached_input_tokens: u64,
    /// `cache_write_tokens`.
    pub cache_write_tokens: u64,
    /// Real dollars, reported by the provider rather than derived from a price table.
    pub cost: Option<f64>,
}

impl Usage {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// Fold another report into this one.
    ///
    /// Usage arrives per-round and, on `OpenRouter`, only after the stop event, so a
    /// whole-run total is a sum rather than a single terminal reading. `cost` stays
    /// `None` until at least one report carries it, so "not reported" and "reported
    /// as zero" remain distinguishable.
    pub fn accumulate(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        if let Some(cost) = other.cost {
            *self.cost.get_or_insert(0.0) += cost;
        }
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
