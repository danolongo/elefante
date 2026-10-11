use crate::{MemoryChange, MemoryNote, MemoryStatus, StopReason, Usage, encode_frame};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Sequence {
    Single { seq: u64 },
    Coalesced { seq_from: u64, seq_to: u64 },
}

impl Sequence {
    #[must_use]
    pub const fn first(self) -> u64 {
        match self {
            Self::Single { seq } => seq,
            Self::Coalesced { seq_from, .. } => seq_from,
        }
    }
    #[must_use]
    pub const fn last(self) -> u64 {
        match self {
            Self::Single { seq } => seq,
            Self::Coalesced { seq_to, .. } => seq_to,
        }
    }
}

impl<'de> Deserialize<'de> for Sequence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            seq: Option<u64>,
            seq_from: Option<u64>,
            seq_to: Option<u64>,
        }
        let fields = Fields::deserialize(deserializer)?;
        match (fields.seq, fields.seq_from, fields.seq_to) {
            (Some(seq), None, None) if seq > 0 => Ok(Self::Single { seq }),
            (None, Some(seq_from), Some(seq_to)) if seq_from > 0 && seq_from <= seq_to => {
                Ok(Self::Coalesced { seq_from, seq_to })
            }
            _ => Err(serde::de::Error::custom(
                "expected positive seq or ordered seq_from/seq_to",
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Notification {
    pub session_id: String,
    #[serde(flatten)]
    pub sequence: Sequence,
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub event: NotificationEvent,
}

impl<'de> Deserialize<'de> for Notification {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Metadata {
            session_id: String,
            turn_id: Option<String>,
        }
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("expected notification object"))?;
        if object.contains_key("id") || !object.contains_key("turn_id") {
            return Err(serde::de::Error::custom(
                "notification requires turn_id and must not carry id",
            ));
        }
        let metadata: Metadata =
            serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
        let sequence: Sequence =
            serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
        let event: NotificationEvent =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        if matches!(sequence, Sequence::Coalesced { .. }) && !event.is_delta() {
            return Err(serde::de::Error::custom(
                "only text and reasoning deltas may carry a sequence range",
            ));
        }
        Ok(Self {
            session_id: metadata.session_id,
            turn_id: metadata.turn_id,
            sequence,
            event,
        })
    }
}

impl Notification {
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        seq: u64,
        turn_id: Option<String>,
        event: NotificationEvent,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            sequence: Sequence::Single { seq },
            turn_id,
            event,
        }
    }

    /// Merge adjacent deltas of the same channel, session, and turn.
    /// Returns false without mutation when merging would lose ordering or exceed framing limits.
    pub fn coalesce(&mut self, next: &Self) -> bool {
        if self.session_id != next.session_id
            || self.turn_id != next.turn_id
            || self.sequence.last().checked_add(1) != Some(next.sequence.first())
        {
            return false;
        }
        match (&mut self.event, &next.event) {
            (
                NotificationEvent::TextDelta { text },
                NotificationEvent::TextDelta { text: more },
            )
            | (
                NotificationEvent::ReasoningDelta { text },
                NotificationEvent::ReasoningDelta { text: more },
            ) => {
                let old_len = text.len();
                text.push_str(more);
                let old_sequence = self.sequence;
                self.sequence = Sequence::Coalesced {
                    seq_from: old_sequence.first(),
                    seq_to: next.sequence.last(),
                };
                if encode_frame(self).is_err() {
                    match &mut self.event {
                        NotificationEvent::TextDelta { text }
                        | NotificationEvent::ReasoningDelta { text } => text.truncate(old_len),
                        _ => unreachable!(),
                    }
                    self.sequence = old_sequence;
                    return false;
                }
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum NotificationEvent {
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta { text: String },
    #[serde(rename = "text.delta")]
    TextDelta { text: String },
    #[serde(rename = "tool.started")]
    ToolStarted {
        tool_call_id: String,
        name: String,
        input_preview: String,
    },
    #[serde(rename = "tool.finished")]
    ToolFinished {
        tool_call_id: String,
        is_error: bool,
        output_preview: String,
        duration_ms: u64,
    },
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        approval_id: String,
        command: String,
        rule: String,
    },
    #[serde(rename = "approval.resolved")]
    ApprovalResolved { approval_id: String, approved: bool },
    #[serde(rename = "turn.finished")]
    TurnFinished {
        stop_reason: StopReason,
        usage: Usage,
    },
    #[serde(rename = "session.error")]
    SessionError { message: String },
    #[serde(rename = "memory.status")]
    MemoryStatus(MemoryStatus),
    #[serde(rename = "memory.learned")]
    MemoryLearned {
        note: MemoryNote,
        change: MemoryChange,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replaced: Option<MemoryNote>,
    },
    #[serde(rename = "memory.context")]
    MemoryContext { notes: Vec<MemoryNote> },
    #[serde(skip_serializing)]
    Unknown { method: String },
}

#[derive(Deserialize)]
#[serde(tag = "method", content = "params")]
enum KnownNotificationEvent {
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta { text: String },
    #[serde(rename = "text.delta")]
    TextDelta { text: String },
    #[serde(rename = "tool.started")]
    ToolStarted {
        tool_call_id: String,
        name: String,
        input_preview: String,
    },
    #[serde(rename = "tool.finished")]
    ToolFinished {
        tool_call_id: String,
        is_error: bool,
        output_preview: String,
        duration_ms: u64,
    },
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        approval_id: String,
        command: String,
        rule: String,
    },
    #[serde(rename = "approval.resolved")]
    ApprovalResolved { approval_id: String, approved: bool },
    #[serde(rename = "turn.finished")]
    TurnFinished {
        stop_reason: StopReason,
        usage: Usage,
    },
    #[serde(rename = "session.error")]
    SessionError { message: String },
    #[serde(rename = "memory.status")]
    MemoryStatus(MemoryStatus),
    #[serde(rename = "memory.learned")]
    MemoryLearned {
        note: MemoryNote,
        change: MemoryChange,
        #[serde(default)]
        replaced: Option<MemoryNote>,
    },
    #[serde(rename = "memory.context")]
    MemoryContext { notes: Vec<MemoryNote> },
}

impl From<KnownNotificationEvent> for NotificationEvent {
    fn from(event: KnownNotificationEvent) -> Self {
        match event {
            KnownNotificationEvent::TurnStarted {} => Self::TurnStarted {},
            KnownNotificationEvent::ReasoningDelta { text } => Self::ReasoningDelta { text },
            KnownNotificationEvent::TextDelta { text } => Self::TextDelta { text },
            KnownNotificationEvent::ToolStarted {
                tool_call_id,
                name,
                input_preview,
            } => Self::ToolStarted {
                tool_call_id,
                name,
                input_preview,
            },
            KnownNotificationEvent::ToolFinished {
                tool_call_id,
                is_error,
                output_preview,
                duration_ms,
            } => Self::ToolFinished {
                tool_call_id,
                is_error,
                output_preview,
                duration_ms,
            },
            KnownNotificationEvent::ApprovalRequested {
                approval_id,
                command,
                rule,
            } => Self::ApprovalRequested {
                approval_id,
                command,
                rule,
            },
            KnownNotificationEvent::ApprovalResolved {
                approval_id,
                approved,
            } => Self::ApprovalResolved {
                approval_id,
                approved,
            },
            KnownNotificationEvent::TurnFinished { stop_reason, usage } => {
                Self::TurnFinished { stop_reason, usage }
            }
            KnownNotificationEvent::SessionError { message } => Self::SessionError { message },
            KnownNotificationEvent::MemoryStatus(status) => Self::MemoryStatus(status),
            KnownNotificationEvent::MemoryLearned {
                note,
                change,
                replaced,
            } => Self::MemoryLearned {
                note,
                change,
                replaced,
            },
            KnownNotificationEvent::MemoryContext { notes } => Self::MemoryContext { notes },
        }
    }
}

impl<'de> Deserialize<'de> for NotificationEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let method = value
            .as_object()
            .and_then(|object| object.get("method"))
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("notification event requires string method"))?
            .to_owned();
        match method.as_str() {
            "turn.started" | "reasoning.delta" | "text.delta" | "tool.started"
            | "tool.finished" | "approval.requested" | "approval.resolved" | "turn.finished"
            | "session.error" | "memory.status" | "memory.learned" | "memory.context" => {
                serde_json::from_value::<KnownNotificationEvent>(value)
                    .map(Into::into)
                    .map_err(serde::de::Error::custom)
            }
            _ if value
                .as_object()
                .and_then(|object| object.get("params"))
                .is_some_and(Value::is_object) =>
            {
                Ok(Self::Unknown { method })
            }
            _ => Err(serde::de::Error::custom(
                "notification event requires object params",
            )),
        }
    }
}

impl NotificationEvent {
    #[must_use]
    pub const fn is_delta(&self) -> bool {
        matches!(self, Self::ReasoningDelta { .. } | Self::TextDelta { .. })
    }
}
