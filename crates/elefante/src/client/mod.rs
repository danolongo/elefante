//! Rendering for the daemon client. Each output stream owns its renderer.
pub mod memory;

use crate::wire::{Notification, NotificationEvent, SandboxState, SandboxStatus};
use std::io::{self, Write};

/// The one warning line for a `session.create` result whose shell is not sandboxed.
///
/// Returns `None` while the sandbox is on, and for an older daemon that reports no state.
#[must_use]
pub fn sandbox_warning(create_result: &serde_json::Value) -> Option<String> {
    let status: SandboxStatus =
        serde_json::from_value(create_result.get("sandbox")?.clone()).ok()?;
    let detail = status
        .detail
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match status.state {
        SandboxState::On => None,
        SandboxState::Off => Some(format!(
            "sandbox: off ({detail}); every shell command needs your approval"
        )),
        SandboxState::Unavailable => Some(format!(
            "sandbox: unavailable; shell commands will fail: {detail}"
        )),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Reasoning,
    Text,
    Tool,
    Approval,
    Status,
    Error,
}

pub struct StreamRenderer<W: Write> {
    out: W,
    styled: bool,
    last: Option<Channel>,
    at_line_start: bool,
    memory_notices: bool,
    saved: usize,
    problem: Option<String>,
}

impl<W: Write> StreamRenderer<W> {
    pub fn new(out: W, styled: bool) -> Self {
        Self {
            out,
            styled,
            last: None,
            at_line_start: true,
            memory_notices: false,
            saved: 0,
            problem: None,
        }
    }

    /// Memory lines are for a person at a terminal; scripts never see them.
    #[must_use]
    pub fn with_memory_notices(mut self, enabled: bool) -> Self {
        self.memory_notices = enabled;
        self
    }

    /// The caller routes answer notifications to stdout and others to stderr.
    ///
    /// # Errors
    /// Returns errors from the output writer.
    pub fn render(&mut self, note: &Notification) -> io::Result<()> {
        use NotificationEvent as E;
        match &note.event {
            E::ReasoningDelta { text } => self.write(Channel::Reasoning, text, true),
            E::TextDelta { text } => self.write(Channel::Text, text, false),
            E::ToolStarted {
                name,
                input_preview,
                ..
            } => self.write(Channel::Tool, &format!("[{name}] {input_preview}\n"), false),
            E::ToolFinished {
                tool_call_id,
                is_error,
                output_preview,
                duration_ms,
            } => self.write(
                Channel::Tool,
                &format!(
                    "[{tool_call_id}] {} ({duration_ms} ms){}\n",
                    if *is_error { "error" } else { "done" },
                    if output_preview.is_empty() {
                        String::new()
                    } else {
                        format!(": {output_preview}")
                    }
                ),
                false,
            ),
            E::ApprovalRequested { command, rule, .. } => self.write(
                Channel::Approval,
                &format!("Approval required ({rule}): {command}\n"),
                false,
            ),
            E::ApprovalResolved { approved, .. } => self.write(
                Channel::Approval,
                if *approved {
                    "Approval allowed\n"
                } else {
                    "Approval denied: a run with a prompt cannot ask you. Run `elefante` with no prompt to approve commands.\n"
                },
                false,
            ),
            E::SessionError { message } => {
                self.write(Channel::Error, &format!("{message}\n"), false)
            }
            E::TurnFinished { stop_reason, .. } => {
                match stop_reason {
                    crate::wire::StopReason::Refusal => {
                        self.write(Channel::Status, "provider refused the request\n", false)
                    }
                    crate::wire::StopReason::Interrupted => {
                        self.write(Channel::Status, "interrupted\n", false)
                    }
                    _ => Ok(()),
                }?;
                self.write_saved()
            }
            E::MemoryStatus(status) => {
                if self.memory_notices
                    && let Some(problem) = &status.problem
                    && self.problem.as_ref() != Some(problem)
                {
                    self.problem = Some(problem.clone());
                    return self.write(Channel::Status, &format!("memory: {problem}\n"), false);
                }
                Ok(())
            }
            E::MemoryLearned { note, .. } => {
                if note.status != "pending" {
                    self.saved += 1;
                }
                Ok(())
            }
            E::TurnStarted {} | E::MemoryContext { .. } | E::Unknown { .. } => Ok(()),
        }
    }

    fn write_saved(&mut self) -> io::Result<()> {
        let saved = std::mem::take(&mut self.saved);
        if !self.memory_notices || saved == 0 {
            return Ok(());
        }
        let plural = if saved == 1 { "" } else { "s" };
        self.write(
            Channel::Status,
            &format!("memory: saved {saved} note{plural}\n"),
            false,
        )
    }

    fn write(&mut self, channel: Channel, text: &str, faint: bool) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.last.is_some_and(|last| last != channel) && !self.at_line_start {
            self.out.write_all(b"\n")?;
        }
        if faint && self.styled {
            self.out.write_all(b"\x1b[2m")?;
            let result = self.out.write_all(text.as_bytes());
            let reset = self.out.write_all(b"\x1b[0m");
            result?;
            reset?;
        } else {
            self.out.write_all(text.as_bytes())?;
        }
        self.last = Some(channel);
        self.at_line_start = text.ends_with('\n');
        self.out.flush()
    }

    /// Terminate a partial line without adding an empty line to an idle stream.
    ///
    /// # Errors
    /// Returns errors from the output writer.
    pub fn finish(&mut self) -> io::Result<()> {
        if !self.at_line_start {
            self.out.write_all(b"\n")?;
            self.at_line_start = true;
        }
        self.out.flush()
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::sandbox_warning;
    use serde_json::json;

    #[test]
    fn only_an_unsandboxed_session_gets_one_warning_line() {
        assert_eq!(
            sandbox_warning(&json!({"sandbox":{"state":"on","detail":"seatbelt"}})),
            None
        );
        assert_eq!(sandbox_warning(&json!({"session_id":"s1"})), None);
        assert_eq!(
            sandbox_warning(&json!({"sandbox":{"state":"off","detail":"ELEFANTE_SANDBOX=0"}}))
                .as_deref(),
            Some("sandbox: off (ELEFANTE_SANDBOX=0); every shell command needs your approval")
        );
        let unavailable = sandbox_warning(&json!({"sandbox":{
            "state":"unavailable","detail":"sandbox probe failed:\nexit status 1\x1b[31m"
        }}))
        .unwrap();
        assert!(unavailable.starts_with("sandbox: unavailable;"));
        assert!(!unavailable.contains(['\n', '\x1b']), "{unavailable:?}");
        assert!(unavailable.contains("probe failed: exit status 1"));
    }
}
