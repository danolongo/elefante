use std::{
    collections::HashSet,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use elefante_provider::{
    Capabilities, ContentBlock, Event, ModelId, Provider, ProviderError, ReasoningDetail, Request,
    StopReason, Usage,
};
use futures_util::stream::BoxStream;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Deserialize)]
enum ScriptEvent {
    ReasoningDelta(String),
    ReasoningDetails(Vec<ReasoningDetail>),
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta(String),
    ToolUseComplete { id: String, input: Value },
    Usage(Usage),
    Stop(StopReason),
}

impl From<ScriptEvent> for Event {
    fn from(event: ScriptEvent) -> Self {
        match event {
            ScriptEvent::ReasoningDelta(text) => Self::ReasoningDelta(text),
            ScriptEvent::ReasoningDetails(details) => Self::ReasoningDetails(details),
            ScriptEvent::TextDelta(text) => Self::TextDelta(text),
            ScriptEvent::ToolUseStart { id, name } => Self::ToolUseStart { id, name },
            ScriptEvent::ToolUseInputDelta(text) => Self::ToolUseInputDelta(text),
            ScriptEvent::ToolUseComplete { id, input } => Self::ToolUseComplete { id, input },
            ScriptEvent::Usage(usage) => Self::Usage(usage),
            ScriptEvent::Stop(stop) => Self::Stop(stop),
        }
    }
}

#[derive(Clone)]
enum Step {
    Event(Event),
    Delay(u64),
}

pub(crate) struct StubProvider {
    rounds: Vec<Vec<Step>>,
    next_round: AtomicUsize,
    delay: Duration,
    /// When set, every request is appended to this file as one JSON line, so a test can see
    /// exactly what the model would have been sent.
    record: Option<PathBuf>,
    /// Replies for extraction requests (those carrying a response schema), in order, the last one
    /// repeating. They never consume `rounds`, so memory learning cannot disturb a scripted turn.
    extract: Vec<String>,
    next_extract: AtomicUsize,
    extract_usage: Usage,
    /// Held before each extraction reply, so a test can withdraw consent while a call is out.
    extract_delay: Duration,
    /// The first this-many extraction requests fail instead of replying.
    extract_errors: usize,
}

impl StubProvider {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let value: Value = serde_json::from_slice(&fs::read(path)?)?;
        let delay = Duration::from_millis(value["delay_ms"].as_u64().unwrap_or(0));
        let record = value["record_requests"].as_str().map(PathBuf::from);
        let extract = value
            .get("extract")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map_or_else(|| item.to_string(), str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let extract_usage = value
            .get("extract_usage")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        let extract_delay = Duration::from_millis(value["extract_delay_ms"].as_u64().unwrap_or(0));
        let extract_errors = value["extract_errors"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        let rounds = if let Some(rounds) = value.get("rounds").and_then(Value::as_array) {
            rounds.clone()
        } else {
            vec![value.get("events").cloned().unwrap_or(value)]
        };
        let rounds = rounds
            .into_iter()
            .map(|round| {
                round
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("stub events must be an array"))?
                    .iter()
                    .map(|event| {
                        if let Some(delay) = event.get("delay_ms").and_then(Value::as_u64) {
                            return Ok(Step::Delay(delay));
                        }
                        Ok(Step::Event(
                            serde_json::from_value::<ScriptEvent>(event.clone())?.into(),
                        ))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(!rounds.is_empty(), "stub rounds must not be empty");
        Ok(Self {
            rounds,
            next_round: AtomicUsize::new(0),
            delay,
            record,
            extract,
            next_extract: AtomicUsize::new(0),
            extract_usage,
            extract_delay,
            extract_errors,
        })
    }
}

pub(crate) fn validate_pairing(request: &Request) -> Result<(), ProviderError> {
    let mut pending = HashSet::new();
    for message in &request.messages {
        for block in &message.content {
            // Each arm does its own bookkeeping and reports why it failed, if it did.
            // Clippy offers to collapse the nested `if` into a match guard, but a guard runs
            // during pattern selection, so `remove` would mutate `pending` even on arms that
            // go on not to match.
            let failure = match block {
                ContentBlock::ToolUse { id, .. } => {
                    (!pending.insert(id.clone())).then_some("duplicate tool_use id")
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    (!pending.remove(tool_use_id)).then_some("unpaired or duplicate tool_result")
                }
                _ => None,
            };
            if let Some(reason) = failure {
                return Err(ProviderError::Protocol(reason.into()));
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(ProviderError::Protocol(
            "tool_use lacks matching tool_result".into(),
        ))
    }
}

impl Provider for StubProvider {
    fn capabilities(&self, _model: &ModelId) -> Capabilities {
        Capabilities::default()
    }

    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event, ProviderError>> {
        if let Some(path) = &self.record
            && let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path)
            && let Ok(line) = serde_json::to_string(&request)
        {
            let _ = writeln!(file, "{line}");
        }
        if request.response_schema.is_some() {
            let index = self.next_extract.fetch_add(1, Ordering::Relaxed);
            let failing = index < self.extract_errors;
            let text = self
                .extract
                .get(index.min(self.extract.len().saturating_sub(1)))
                .cloned()
                .unwrap_or_else(|| r#"{"memories":[]}"#.to_owned());
            let mut events = vec![Ok(Event::TextDelta(text))];
            if !self.extract_usage.is_empty() {
                events.push(Ok(Event::Usage(self.extract_usage)));
            }
            events.push(Ok(Event::Stop(StopReason::EndTurn)));
            if failing {
                events = vec![Err(ProviderError::Protocol("scripted failure".into()))];
            }
            let pause = self.extract_delay;
            return Box::pin(async_stream::stream! {
                if !pause.is_zero() {
                    tokio::time::sleep(pause).await;
                }
                for event in events {
                    yield event;
                }
            });
        }
        let validation = validate_pairing(&request);
        let index = self.next_round.fetch_add(1, Ordering::Relaxed);
        let steps = self.rounds[index.min(self.rounds.len() - 1)].clone();
        let delay = self.delay;
        Box::pin(async_stream::try_stream! {
            validation?;
            for step in steps {
                match step {
                    Step::Delay(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                    Step::Event(event) => {
                        if !delay.is_zero() { tokio::time::sleep(delay).await; }
                        yield event;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elefante_provider::{Message, Role};

    #[test]
    fn stub_rejects_missing_extra_and_duplicate_tool_results() {
        let mut request = Request {
            model: "test".into(),
            max_tokens: 1,
            system: vec![],
            tools: vec![],
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }],
            }],
            effort: None,
            response_schema: None,
            web_search: None,
        };
        assert!(validate_pairing(&request).is_err());
        let result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "interrupted".into(),
                is_error: true,
            }],
        };
        request.messages.push(result.clone());
        assert!(validate_pairing(&request).is_ok());
        request.messages.push(result);
        assert!(validate_pairing(&request).is_err());
        request.messages.remove(0);
        assert!(validate_pairing(&request).is_err());
    }
}
