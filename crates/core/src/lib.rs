//! Provider-neutral agent loop for the first usable Elefante milestone.

use std::collections::HashMap;

use elefante_provider::{
    ContentBlock, Effort, Event, Message, ModelId, Provider, ProviderError, ReasoningDetail,
    Request, Role, StopReason, ToolDefinition, Usage,
};
use elefante_tools::{ToolExecutor, ToolResult, definitions as tool_definitions};
use futures_util::{StreamExt, future::join_all};
use serde_json::Value;
use thiserror::Error;

/// The stable standing instructions. Safety and filesystem policy live in code.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are Elefante, a coding agent working in the current repository. Inspect before editing. Use tools when they provide evidence or are needed to make a change. Keep changes focused, preserve existing behavior unless the request changes it, and verify completed work. Report the result and any remaining uncertainty plainly.";

/// Limits and provider settings for one loop invocation.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub model: ModelId,
    pub effort: Option<Effort>,
    pub max_tokens: u32,
    pub max_rounds: usize,
    pub system_prompt: String,
}

impl RunConfig {
    #[must_use]
    pub fn new(model: impl Into<ModelId>) -> Self {
        Self {
            model: model.into(),
            effort: Some(Effort::High),
            max_tokens: 16_384,
            max_rounds: 24,
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_owned(),
        }
    }
}

/// Observable loop output. Frontends decide how each event is rendered.
///
/// Only renderable output appears here. Reasoning replay state
/// ([`Event::ReasoningDetails`]) is accumulated into the assistant message so
/// the next request can return it unmodified, and is deliberately never
/// surfaced as an [`OutputEvent`]: replaying it is a protocol obligation,
/// displaying it is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    /// A display fragment of the model's reasoning. Safe to render.
    ReasoningDelta(String),
    TextDelta(String),
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub stop_reason: StopReason,
    pub rounds: usize,
    pub text: String,
    /// Token and cost totals summed across every round of the run.
    ///
    /// The provider reports usage on its own schedule — for `OpenRouter`, after the
    /// stop event — so this is accumulated by draining each stream to completion
    /// rather than by reading a single terminal event.
    pub usage: Usage,
}

#[derive(Debug, Error)]
pub enum LoopError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("provider protocol error: {0}")]
    Protocol(String),
    #[error("agent loop exceeded its {max_rounds}-round limit")]
    RoundLimit { max_rounds: usize },
}

#[derive(Clone, Debug)]
struct ToolCall {
    id: String,
    name: String,
    input: Value,
}

#[derive(Debug)]
struct RoundAccumulator {
    blocks: Vec<ContentBlock>,
    tool_positions: HashMap<String, usize>,
    input_deltas: HashMap<String, String>,
    active_tool_id: Option<String>,
    text: String,
}

impl RoundAccumulator {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            tool_positions: HashMap::new(),
            input_deltas: HashMap::new(),
            active_tool_id: None,
            text: String::new(),
        }
    }

    fn push_text(&mut self, delta: String) {
        self.text.push_str(&delta);
        match self.blocks.last_mut() {
            Some(ContentBlock::Text { text }) => text.push_str(&delta),
            _ => self.blocks.push(ContentBlock::Text { text: delta }),
        }
    }

    /// Records reasoning replay state on the assistant turn.
    ///
    /// The blocks are stored verbatim and in arrival order: the provider
    /// requires them back unmodified and unreordered. Adjacent batches are
    /// appended to the open reasoning block so a single assistant turn keeps
    /// one contiguous run, which preserves order either way.
    fn push_reasoning(&mut self, details: Vec<ReasoningDetail>) {
        match self.blocks.last_mut() {
            Some(ContentBlock::Reasoning { details: existing }) => existing.extend(details),
            _ => self.blocks.push(ContentBlock::Reasoning { details }),
        }
    }

    fn start_tool(&mut self, id: String, name: String) -> Result<(), LoopError> {
        if self.tool_positions.contains_key(&id) {
            return Err(LoopError::Protocol(format!(
                "provider reused tool use id {id:?}"
            )));
        }
        let position = self.blocks.len();
        self.blocks.push(ContentBlock::ToolUse {
            id: id.clone(),
            name,
            input: Value::Null,
        });
        self.tool_positions.insert(id.clone(), position);
        self.active_tool_id = Some(id);
        Ok(())
    }

    fn push_tool_input(&mut self, id: &str, delta: &str) {
        self.input_deltas
            .entry(id.to_owned())
            .or_default()
            .push_str(delta);
    }

    fn complete_tool(&mut self, id: &str, input: Value) -> Result<(), LoopError> {
        let Some(position) = self.tool_positions.get(id).copied() else {
            return Err(LoopError::Protocol(format!(
                "tool use {id:?} completed before it started"
            )));
        };
        let ContentBlock::ToolUse {
            input: block_input, ..
        } = &mut self.blocks[position]
        else {
            return Err(LoopError::Protocol(format!(
                "tool use {id:?} did not map to a tool block"
            )));
        };
        *block_input = input;
        if self.active_tool_id.as_deref() == Some(id) {
            self.active_tool_id = None;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(Vec<ContentBlock>, Vec<ToolCall>, String), LoopError> {
        let mut calls = Vec::with_capacity(self.tool_positions.len());
        for block in &mut self.blocks {
            let ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            if input.is_null() {
                let raw = self.input_deltas.get(id).ok_or_else(|| {
                    LoopError::Protocol(format!("tool use {id:?} has no complete input"))
                })?;
                *input = serde_json::from_str(raw).map_err(|error| {
                    LoopError::Protocol(format!("tool use {id:?} has invalid input: {error}"))
                })?;
            }
            calls.push(ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
        }
        Ok((self.blocks, calls, self.text))
    }
}

struct CompletedRound {
    assistant_content: Vec<ContentBlock>,
    calls: Vec<ToolCall>,
    text: String,
    stop_reason: StopReason,
    usage: Usage,
}

/// Run a bounded provider/tool conversation from one user prompt.
///
/// `emit` is deliberately synchronous: stdout and RPC fan-out should never own
/// provider parsing, and tests can capture the exact visible stream directly.
///
/// # Errors
///
/// Returns an error when the provider fails, violates the normalized streaming
/// protocol, or consumes the configured number of rounds without stopping.
pub async fn run_agent_loop<F>(
    provider: &dyn Provider,
    tools: &ToolExecutor,
    prompt: impl Into<String>,
    config: RunConfig,
    mut emit: F,
) -> Result<RunOutcome, LoopError>
where
    F: FnMut(OutputEvent),
{
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: prompt.into(),
        }],
    }];
    let definitions = provider_tool_definitions();
    let mut full_text = String::new();
    let mut run_usage = Usage::default();

    for round_index in 0..config.max_rounds {
        let request = Request {
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            system: vec![config.system_prompt.clone()],
            tools: definitions.clone(),
            messages: messages.clone(),
            effort: config.effort,
        };
        let completed = receive_round(provider, request, &mut emit).await?;
        full_text.push_str(&completed.text);
        run_usage.accumulate(completed.usage);

        if completed.stop_reason == StopReason::Refusal {
            return Ok(RunOutcome {
                stop_reason: completed.stop_reason,
                rounds: round_index + 1,
                text: full_text,
                usage: run_usage,
            });
        }

        if !completed.assistant_content.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content: completed.assistant_content,
            });
        }

        if completed.stop_reason != StopReason::ToolUse {
            return Ok(RunOutcome {
                stop_reason: completed.stop_reason,
                rounds: round_index + 1,
                text: full_text,
                usage: run_usage,
            });
        }
        if completed.calls.is_empty() {
            return Err(LoopError::Protocol(
                "provider stopped for tool use without emitting a tool call".to_owned(),
            ));
        }

        let results = execute_calls(tools, &completed.calls).await;
        let mut result_blocks = Vec::with_capacity(results.len());
        for (call, result) in completed.calls.iter().zip(results) {
            emit(OutputEvent::ToolFinished {
                id: call.id.clone(),
                name: call.name.clone(),
                is_error: result.is_error,
            });
            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: result.content,
                is_error: result.is_error,
            });
        }
        messages.push(Message {
            role: Role::User,
            content: result_blocks,
        });
    }

    Err(LoopError::RoundLimit {
        max_rounds: config.max_rounds,
    })
}

async fn receive_round<F>(
    provider: &dyn Provider,
    request: Request,
    emit: &mut F,
) -> Result<CompletedRound, LoopError>
where
    F: FnMut(OutputEvent),
{
    let mut stream = provider.stream(request);
    let mut accumulator = RoundAccumulator::new();
    let mut stop_reason = None;
    let mut round_usage = Usage::default();

    while let Some(event) = stream.next().await {
        match event? {
            Event::ReasoningDelta(delta) => emit(OutputEvent::ReasoningDelta(delta)),
            // Replay state, not display: accumulated for the next request, never emitted.
            Event::ReasoningDetails(details) => accumulator.push_reasoning(details),
            Event::TextDelta(delta) => {
                emit(OutputEvent::TextDelta(delta.clone()));
                accumulator.push_text(delta);
            }
            Event::ToolUseStart { id, name } => {
                emit(OutputEvent::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
                accumulator.start_tool(id, name)?;
            }
            Event::ToolUseInputDelta(delta) => {
                let Some(id) = accumulator.active_tool_id.clone() else {
                    return Err(LoopError::Protocol(
                        "tool input arrived before a tool start".to_owned(),
                    ));
                };
                accumulator.push_tool_input(&id, &delta);
            }
            Event::ToolUseComplete { id, input } => accumulator.complete_tool(&id, input)?,
            Event::Usage(usage) => round_usage.accumulate(usage),
            Event::Stop(reason) => {
                if stop_reason.replace(reason).is_some() {
                    return Err(LoopError::Protocol(
                        "provider emitted more than one stop event".to_owned(),
                    ));
                }
            }
        }
    }

    let stop_reason =
        stop_reason.ok_or_else(|| LoopError::Protocol("stream ended without stop".to_owned()))?;
    if stop_reason == StopReason::Refusal {
        return Ok(CompletedRound {
            assistant_content: Vec::new(),
            calls: Vec::new(),
            text: String::new(),
            stop_reason,
            usage: round_usage,
        });
    }
    let (assistant_content, calls, text) = accumulator.finish()?;
    Ok(CompletedRound {
        assistant_content,
        calls,
        text,
        stop_reason,
        usage: round_usage,
    })
}

fn provider_tool_definitions() -> Vec<ToolDefinition> {
    tool_definitions()
        .into_iter()
        .map(|definition| ToolDefinition {
            name: definition.name,
            description: definition.description,
            input_schema: definition.input_schema,
        })
        .collect()
}

async fn execute_calls(tools: &ToolExecutor, calls: &[ToolCall]) -> Vec<ToolResult> {
    join_all(
        calls
            .iter()
            .map(|call| tools.execute(&call.name, call.input.clone())),
    )
    .await
}
