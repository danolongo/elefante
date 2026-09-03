//! Transport-generic Server-Sent Events decoding.
//!
//! The decoder handles the parts of the SSE wire format that byte-oriented HTTP
//! streams get wrong: frames split across chunk boundaries, mixed CRLF and LF
//! line endings, multi-line `data:` payloads, and comment lines. Nothing in here
//! knows about any particular provider.

use crate::ProviderError;

/// One decoded SSE frame.
#[derive(Debug)]
pub struct SseFrame {
    /// Value of the `event:` line, when the server sent one.
    ///
    /// `OpenRouter` never names its events; the field exists because the decoder
    /// is transport-generic and dropping it would make the decoder lossy.
    pub event: Option<String>,
    /// Concatenated `data:` lines, joined with newlines.
    pub data: String,
}

/// Incremental SSE frame decoder that buffers across chunk boundaries.
#[derive(Debug, Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    /// Feeds one chunk of the response body and returns every frame it completes.
    ///
    /// Comment-only frames (for example keep-alives) carry no `data:` line and
    /// are dropped rather than returned.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Protocol`] when a frame is not valid UTF-8.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>, ProviderError> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some((end, delimiter_len)) = find_frame_end(&self.pending) {
            let raw = self.pending.drain(..end).collect::<Vec<_>>();
            self.pending.drain(..delimiter_len);
            if let Some(frame) = parse_sse_frame(&raw)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    /// Decodes any trailing bytes left when the body ends without a final blank line.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Protocol`] when the remainder is not valid UTF-8.
    pub fn finish(&mut self) -> Result<Option<SseFrame>, ProviderError> {
        if self.pending.is_empty() {
            return Ok(None);
        }
        let raw = std::mem::take(&mut self.pending);
        parse_sse_frame(&raw)
    }
}

fn find_frame_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let line_feed = bytes
        .windows(2)
        .position(|part| part == b"\n\n")
        .map(|position| (position, 2));
    let carriage_return = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|position| (position, 4));
    match (line_feed, carriage_return) {
        (Some(left), Some(right)) => Some(if left.0 < right.0 { left } else { right }),
        (Some(frame), None) | (None, Some(frame)) => Some(frame),
        (None, None) => None,
    }
}

fn parse_sse_frame(bytes: &[u8]) -> Result<Option<SseFrame>, ProviderError> {
    let frame = std::str::from_utf8(bytes)
        .map_err(|error| ProviderError::Protocol(format!("SSE was not UTF-8: {error}")))?;
    let mut event = None;
    let mut data = Vec::new();
    for raw_line in frame.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseFrame {
        event,
        data: data.join("\n"),
    }))
}
