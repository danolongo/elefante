//! Provider-neutral agent loop.

pub mod memory;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use elefante_guard::{BashClassifier, Verdict};
use elefante_provider::{
    ContentBlock, Effort, Event, Message, ModelId, Provider, ProviderError, ReasoningDetail,
    Request, Role, StopReason, ToolDefinition, Usage,
};
use elefante_tools::{ToolExecutor, ToolResult, definitions as tool_definitions};
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::JoinSet,
};
pub use tokio_util::sync::CancellationToken;

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
#[derive(Clone, Debug, PartialEq)]
pub enum OutputEvent {
    TurnStarted,
    /// A display fragment of the model's reasoning. Safe to render.
    ReasoningDelta(String),
    TextDelta(String),
    ToolStarted {
        id: String,
        name: String,
        input_preview: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        output_preview: String,
        duration_ms: u64,
    },
    ApprovalRequested {
        approval_id: String,
        command: String,
        rule: &'static str,
    },
    ApprovalResolved {
        approval_id: String,
        approved: bool,
    },
    TurnFinished {
        stop_reason: StopReason,
        usage: Usage,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
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

/// Compatibility name for single-turn callers.
pub type RunOutcome = TurnOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    Cancelled,
}

pub trait ApprovalSink: Send + Sync {
    fn request<'a>(
        &'a self,
        id: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome>;
}

/// Headless callers cannot grant an escalation.
pub struct DenyApprovalSink;

impl ApprovalSink for DenyApprovalSink {
    fn request<'a>(
        &'a self,
        _id: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, ApprovalOutcome> {
        Box::pin(async { ApprovalOutcome::Denied })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    Deny { rule: &'static str },
    Escalate { rule: &'static str, command: String },
}

#[derive(Clone)]
pub struct ToolGate {
    classifier: BashClassifier,
    approvals: Arc<dyn ApprovalSink>,
}

impl Default for ToolGate {
    fn default() -> Self {
        Self::new(Arc::new(DenyApprovalSink))
    }
}

impl ToolGate {
    #[must_use]
    pub fn new(approvals: Arc<dyn ApprovalSink>) -> Self {
        Self {
            classifier: BashClassifier::new(),
            approvals,
        }
    }

    #[must_use]
    pub fn with_approval_sink(approvals: Arc<dyn ApprovalSink>) -> Self {
        Self::new(approvals)
    }

    #[must_use]
    pub fn check(&self, name: &str, input: &Value) -> GateDecision {
        self.check_in(name, input, std::path::Path::new("."))
    }

    fn check_in(&self, name: &str, input: &Value, root: &std::path::Path) -> GateDecision {
        if name != "bash" {
            return GateDecision::Allow;
        }
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return GateDecision::Deny {
                rule: "invalid-bash-input",
            };
        };
        let decision = self.classifier.classify_in(command, root);
        match decision.verdict {
            Verdict::Allow => GateDecision::Allow,
            Verdict::Deny => GateDecision::Deny {
                rule: decision.rule,
            },
            Verdict::Escalate => GateDecision::Escalate {
                rule: decision.rule,
                command: command.to_owned(),
            },
        }
    }
}

/// Where per-turn context comes from. Implemented over the repository index in [`memory`]; tests
/// supply their own.
///
/// What it returns is **repository-derived data**. It is placed in a request as an ephemeral
/// `user` message and never in `system`, and never stored in history: see [`Session::prompt`].
pub trait ContextSource: Send + Sync {
    /// A block relevant to `prompt`, or `None` when nothing is. Called once per turn.
    fn turn_block<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Option<String>>;

    /// A standing summary, or `None` while there is not yet one to give. Once a session has
    /// adopted one it keeps it.
    fn preamble(&self) -> BoxFuture<'_, Option<String>>;
}

pub struct Session {
    messages: Vec<Message>,
    config: RunConfig,
    usage: Usage,
    context: Option<Arc<dyn ContextSource>>,
    /// Adopted at most once; the same bytes on every later request so the cached prefix holds.
    preamble: Option<String>,
}

/// What one turn adds to its requests without adding it to history.
#[derive(Default)]
struct TurnContext {
    /// Index in `Session::messages` of this turn's user message.
    user_index: usize,
    block: Option<String>,
}

impl Session {
    #[must_use]
    pub fn new(config: RunConfig) -> Self {
        Self {
            messages: Vec::new(),
            config,
            usage: Usage::default(),
            context: None,
            preamble: None,
        }
    }

    /// Attaches a source of per-turn context.
    #[must_use]
    pub fn with_context(mut self, context: Arc<dyn ContextSource>) -> Self {
        self.context = Some(context);
        self
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Appends one user turn and retains its paired history for subsequent prompts.
    ///
    /// # Errors
    /// Returns provider/protocol failures or exhaustion of the round limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn prompt<F>(
        &mut self,
        provider: &dyn Provider,
        tools: &ToolExecutor,
        gate: &ToolGate,
        prompt: impl Into<String>,
        cancel: CancellationToken,
        mut emit: F,
    ) -> Result<TurnOutcome, LoopError>
    where
        F: FnMut(OutputEvent),
    {
        let prompt = prompt.into();
        self.messages
            .push(Message::text(Role::User, prompt.clone()));
        let turn = TurnContext {
            user_index: self.messages.len() - 1,
            block: self.gather_context(&prompt).await,
        };
        emit(OutputEvent::TurnStarted);
        let mut usage = Usage::default();
        let result = self
            .run_turn(provider, tools, gate, &cancel, &turn, &mut usage, &mut emit)
            .await;
        self.usage.accumulate(usage);
        let stop_reason = match &result {
            Ok(outcome) => outcome.stop_reason.clone(),
            Err(error) => {
                emit(OutputEvent::Error {
                    message: error.to_string(),
                });
                StopReason::Other("error".to_owned())
            }
        };
        emit(OutputEvent::TurnFinished { stop_reason, usage });
        result
    }

    /// Fetches this turn's context and, once, the standing preamble.
    ///
    /// Done once per turn rather than per round: every round re-sends the same block at the same
    /// position, so the cache holds across tool rounds.
    async fn gather_context(&mut self, prompt: &str) -> Option<String> {
        let context = self.context.clone()?;
        if self.preamble.is_none() {
            self.preamble = context.preamble().await;
        }
        context.turn_block(prompt).await
    }

    /// History as the provider sees it: the preamble first, the turn's block immediately after
    /// the user message that started it. Neither is in `self.messages`.
    ///
    /// The block goes *after* the user message, not before, so that across consecutive turns the
    /// request prefix is byte-identical through that user message and only what follows differs.
    fn request_messages(&self, turn: &TurnContext) -> Vec<Message> {
        let mut messages = Vec::with_capacity(self.messages.len() + 2);
        if let Some(preamble) = &self.preamble {
            messages.push(Message::text(Role::User, preamble.clone()));
        }
        let split = (turn.user_index + 1).min(self.messages.len());
        messages.extend_from_slice(&self.messages[..split]);
        if let Some(block) = &turn.block {
            messages.push(Message::text(Role::User, block.clone()));
        }
        messages.extend_from_slice(&self.messages[split..]);
        messages
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn<F>(
        &mut self,
        provider: &dyn Provider,
        tools: &ToolExecutor,
        gate: &ToolGate,
        cancel: &CancellationToken,
        turn: &TurnContext,
        usage: &mut Usage,
        emit: &mut F,
    ) -> Result<TurnOutcome, LoopError>
    where
        F: FnMut(OutputEvent),
    {
        let definitions = provider_tool_definitions();
        let mut full_text = String::new();
        for round_index in 0..self.config.max_rounds {
            let request = Request {
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens,
                system: vec![self.config.system_prompt.clone()],
                tools: definitions.clone(),
                messages: self.request_messages(turn),
                effort: self.config.effort,
            };
            let completed = receive_round(provider, request, cancel, usage, emit).await?;
            full_text.push_str(&completed.text);
            // Validate pairing before publishing an assistant message to persistent history.
            if completed.stop_reason == StopReason::ToolUse && completed.calls.is_empty() {
                return Err(LoopError::Protocol(
                    "provider stopped for tool use without emitting a tool call".to_owned(),
                ));
            }
            if !completed.calls.is_empty() && completed.stop_reason != StopReason::ToolUse {
                return Err(LoopError::Protocol(
                    "provider emitted tool calls without a tool-use stop".to_owned(),
                ));
            }
            if !completed.assistant_content.is_empty() {
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: completed.assistant_content,
                });
            }
            if completed.stop_reason != StopReason::ToolUse {
                return Ok(TurnOutcome {
                    stop_reason: completed.stop_reason,
                    rounds: round_index + 1,
                    text: full_text,
                    usage: *usage,
                });
            }
            let results = execute_calls(tools, gate, &completed.calls, cancel, emit).await;
            self.messages.push(Message {
                role: Role::User,
                content: completed
                    .calls
                    .iter()
                    .zip(results)
                    .map(|(call, result)| ContentBlock::ToolResult {
                        tool_use_id: call.id.clone(),
                        content: result.content,
                        is_error: result.is_error,
                    })
                    .collect(),
            });
            if cancel.is_cancelled() {
                return Ok(TurnOutcome {
                    stop_reason: StopReason::Interrupted,
                    rounds: round_index + 1,
                    text: full_text,
                    usage: *usage,
                });
            }
        }
        Err(LoopError::RoundLimit {
            max_rounds: self.config.max_rounds,
        })
    }
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
    emit: F,
) -> Result<RunOutcome, LoopError>
where
    F: FnMut(OutputEvent),
{
    Session::new(config)
        .prompt(
            provider,
            tools,
            &ToolGate::default(),
            prompt,
            CancellationToken::new(),
            emit,
        )
        .await
}

async fn receive_round<F>(
    provider: &dyn Provider,
    request: Request,
    cancel: &CancellationToken,
    usage: &mut Usage,
    emit: &mut F,
) -> Result<CompletedRound, LoopError>
where
    F: FnMut(OutputEvent),
{
    let mut stream = provider.stream(request);
    let mut accumulator = RoundAccumulator::new();
    let mut stop_reason = None;

    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                // Incomplete tool calls have never been dispatched and cannot be replayed.
                accumulator.blocks.retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));
                return Ok(CompletedRound { assistant_content: accumulator.blocks, calls: Vec::new(),
                    text: accumulator.text, stop_reason: StopReason::Interrupted });
            }
            event = stream.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event? {
            Event::ReasoningDelta(delta) => emit(OutputEvent::ReasoningDelta(delta)),
            // Replay state, not display: accumulated for the next request, never emitted.
            Event::ReasoningDetails(details) => accumulator.push_reasoning(details),
            Event::TextDelta(delta) => {
                emit(OutputEvent::TextDelta(delta.clone()));
                accumulator.push_text(delta);
            }
            Event::ToolUseStart { id, name } => {
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
            Event::Usage(report) => usage.accumulate(report),
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
        });
    }
    let (assistant_content, calls, text) = accumulator.finish()?;
    Ok(CompletedRound {
        assistant_content,
        calls,
        text,
        stop_reason,
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

/// Marks where a client preview was cut, whether by size or by line count.
const TRUNCATION_MARKER: &str = "\n…";
/// A `read` result is shown to clients as only its opening lines; the model still gets all of it.
const READ_PREVIEW_LINES: usize = 3;

/// Client previews are bounded independently of model-facing tool results.
fn preview(value: &str) -> String {
    const LIMIT: usize = 2048;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT - TRUNCATION_MARKER.len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &value[..end])
}

/// The client preview of a finished tool. A successful `read` shows its first lines only.
fn output_preview(tool: &str, result: &ToolResult) -> String {
    if tool == "read" && !result.is_error {
        let mut lines = result.content.lines();
        let head: Vec<&str> = lines.by_ref().take(READ_PREVIEW_LINES).collect();
        if lines.next().is_some() {
            return preview(&format!("{}{TRUNCATION_MARKER}", head.join("\n")));
        }
    }
    preview(&result.content)
}

fn tool_error(content: impl Into<String>) -> ToolResult {
    ToolResult {
        content: content.into(),
        is_error: true,
    }
}

struct ToolEvent {
    event: OutputEvent,
    published: Option<oneshot::Sender<()>>,
}

async fn execute_one(
    tools: ToolExecutor,
    call: ToolCall,
    decision: GateDecision,
    approvals: Arc<dyn ApprovalSink>,
    approval_slot: Arc<Mutex<()>>,
    cancel: CancellationToken,
    events: mpsc::UnboundedSender<ToolEvent>,
) -> ToolResult {
    match decision {
        GateDecision::Deny { rule } => tool_error(format!("bash denied by policy: {rule}")),
        GateDecision::Allow if cancel.is_cancelled() => tool_error("interrupted"),
        GateDecision::Allow => tools.execute(&call.name, call.input).await,
        GateDecision::Escalate { rule, command } => {
            static APPROVAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
            let slot = tokio::select! {
                biased;
                () = cancel.cancelled() => return tool_error("interrupted"),
                slot = approval_slot.lock() => slot,
            };
            let id = format!(
                "approval-{}",
                APPROVAL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let (published, publication) = oneshot::channel();
            let _ = events.send(ToolEvent {
                event: OutputEvent::ApprovalRequested {
                    approval_id: id.clone(),
                    command,
                    rule,
                },
                published: Some(published),
            });
            // The callback registers the daemon's approval before the sink may resolve it.
            tokio::select! {
                biased;
                () = cancel.cancelled() => return tool_error("interrupted"),
                result = publication => if result.is_err() { return tool_error("interrupted"); },
            }
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => ApprovalOutcome::Cancelled,
                outcome = approvals.request(&id, &cancel) => outcome,
            };
            let approved = outcome == ApprovalOutcome::Approved && !cancel.is_cancelled();
            let _ = events.send(ToolEvent {
                event: OutputEvent::ApprovalResolved {
                    approval_id: id,
                    approved,
                },
                published: None,
            });
            drop(slot);
            if approved {
                tools.execute(&call.name, call.input).await
            } else if outcome == ApprovalOutcome::Cancelled || cancel.is_cancelled() {
                tool_error("interrupted")
            } else {
                tool_error("bash approval denied")
            }
        }
    }
}

async fn execute_calls<F>(
    tools: &ToolExecutor,
    gate: &ToolGate,
    calls: &[ToolCall],
    cancel: &CancellationToken,
    emit: &mut F,
) -> Vec<ToolResult>
where
    F: FnMut(OutputEvent),
{
    let mut set = JoinSet::new();
    let mut results = HashMap::new();
    let mut finished = HashSet::new();
    let mut pending = HashSet::new();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let approval_slot = Arc::new(Mutex::new(()));
    let started = Instant::now();
    for (index, call) in calls.iter().enumerate() {
        emit(OutputEvent::ToolStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            input_preview: preview(&call.input.to_string()),
        });
        let decision = gate.check_in(&call.name, &call.input, tools.root());
        let tools = tools.clone();
        let call = call.clone();
        let approvals = Arc::clone(&gate.approvals);
        let approval_slot = Arc::clone(&approval_slot);
        let cancel = cancel.clone();
        let events_tx = events_tx.clone();
        set.spawn(async move {
            let start = Instant::now();
            let result = execute_one(
                tools,
                call,
                decision,
                approvals,
                approval_slot,
                cancel,
                events_tx,
            )
            .await;
            (
                index,
                result,
                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            )
        });
    }
    drop(events_tx);
    while !set.is_empty() {
        tokio::select! {
            biased;
            Some(event) = events_rx.recv() => {
                publish_tool_event(event, &mut pending, emit);
            }
            () = cancel.cancelled() => {
                set.abort_all();
                // Joining after abort retains tasks that completed before cancellation.
                while let Some(joined) = set.join_next().await {
                    if let Ok((index, result, duration)) = joined { results.insert(index, (result, duration)); }
                }
                break;
            }
            Some(joined) = set.join_next() => {
                if let Ok((index, result, duration_ms)) = joined {
                    let call = &calls[index];
                    emit(OutputEvent::ToolFinished { id: call.id.clone(), name: call.name.clone(),
                        is_error: result.is_error, output_preview: output_preview(&call.name, &result), duration_ms });
                    finished.insert(index);
                    results.insert(index, (result, duration_ms));
                }
            }
            else => break,
        }
    }
    while let Ok(event) = events_rx.try_recv() {
        publish_tool_event(event, &mut pending, emit);
    }
    for approval_id in pending {
        emit(OutputEvent::ApprovalResolved {
            approval_id,
            approved: false,
        });
    }
    finish_calls(calls, results, &finished, cancel, started, emit)
}

fn publish_tool_event<F: FnMut(OutputEvent)>(
    event: ToolEvent,
    pending: &mut HashSet<String>,
    emit: &mut F,
) {
    match &event.event {
        OutputEvent::ApprovalRequested { approval_id, .. } => {
            pending.insert(approval_id.clone());
        }
        OutputEvent::ApprovalResolved { approval_id, .. } => {
            pending.remove(approval_id);
        }
        _ => {}
    }
    emit(event.event);
    if let Some(published) = event.published {
        let _ = published.send(());
    }
}

fn finish_calls<F>(
    calls: &[ToolCall],
    mut results: HashMap<usize, (ToolResult, u64)>,
    finished: &HashSet<usize>,
    cancel: &CancellationToken,
    started: Instant,
    emit: &mut F,
) -> Vec<ToolResult>
where
    F: FnMut(OutputEvent),
{
    calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let (result, duration_ms) = results.remove(&index).unwrap_or_else(|| {
                (
                    tool_error(if cancel.is_cancelled() {
                        "interrupted"
                    } else {
                        "tool task failed"
                    }),
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                )
            });
            if !finished.contains(&index) {
                emit(OutputEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: result.is_error,
                    output_preview: output_preview(&call.name, &result),
                    duration_ms,
                });
            }
            result
        })
        .collect()
}
