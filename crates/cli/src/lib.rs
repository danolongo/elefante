//! Rendering for the daemon client. Each output stream owns its renderer.
pub mod memory;

use elefante_rpc::{Notification, NotificationEvent};
use std::io::{self, Write};

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
}

impl<W: Write> StreamRenderer<W> {
    pub fn new(out: W, styled: bool) -> Self {
        Self {
            out,
            styled,
            last: None,
            at_line_start: true,
        }
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
                    "Approval denied\n"
                },
                false,
            ),
            E::SessionError { message } => {
                self.write(Channel::Error, &format!("{message}\n"), false)
            }
            E::TurnFinished { stop_reason, .. } => match stop_reason {
                elefante_rpc::StopReason::Refusal => {
                    self.write(Channel::Status, "provider refused the request\n", false)
                }
                elefante_rpc::StopReason::Interrupted => {
                    self.write(Channel::Status, "interrupted\n", false)
                }
                _ => Ok(()),
            },
            E::TurnStarted {} | E::Unknown { .. } => Ok(()),
        }
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
