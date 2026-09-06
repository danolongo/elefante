use serde::{Serialize, de::DeserializeOwned};
use std::fmt;

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const PREVIEW_BYTES: usize = 2 * 1024;
pub const TRUNCATION_MARKER: &str = "\n… [truncated]";

/// Cap the complete preview, including its marker, to 2 KiB of UTF-8.
#[must_use]
pub fn truncate_preview(text: &str) -> String {
    if text.len() <= PREVIEW_BYTES {
        return text.to_owned();
    }
    let mut end = PREVIEW_BYTES - TRUNCATION_MARKER.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &text[..end])
}

#[derive(Debug)]
pub enum FrameError {
    TooLarge,
    Incomplete,
    Json(serde_json::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("NDJSON line exceeds 8 MiB"),
            Self::Incomplete => f.write_str("unterminated NDJSON line"),
            Self::Json(error) => write!(f, "invalid NDJSON: {error}"),
        }
    }
}
impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

/// Serialize one newline-terminated JSON value; the cap excludes the delimiter.
///
/// # Errors
/// Returns an error for invalid JSON values or a line exceeding 8 MiB.
pub fn encode_frame(value: &impl Serialize) -> Result<Vec<u8>, FrameError> {
    let mut bytes = serde_json::to_vec(value).map_err(FrameError::Json)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode one newline-terminated frame. Embedded raw newlines are invalid.
///
/// # Errors
/// Returns an error for an oversized, incomplete, or malformed frame.
pub fn decode_frame<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    let line = bytes.strip_suffix(b"\n").ok_or(FrameError::Incomplete)?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    if line.contains(&b'\n') {
        return Err(FrameError::Incomplete);
    }
    serde_json::from_slice(line).map_err(FrameError::Json)
}

/// Incremental framing for socket chunks; an error is terminal for the connection.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
    failed: bool,
}

impl FrameDecoder {
    /// Split arbitrary socket chunks into complete newline-terminated frames.
    ///
    /// # Errors
    /// Returns `TooLarge` immediately on overflow, before buffering beyond the cap.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        if self.failed {
            return Err(FrameError::TooLarge);
        }
        let mut frames = Vec::new();
        while !bytes.is_empty() {
            let newline = bytes.iter().position(|byte| *byte == b'\n');
            let end = newline.unwrap_or(bytes.len());
            if self.pending.len().saturating_add(end) > MAX_FRAME_BYTES {
                self.pending.clear();
                self.failed = true;
                return Err(FrameError::TooLarge);
            }
            self.pending.extend_from_slice(&bytes[..end]);
            if newline.is_some() {
                self.pending.push(b'\n');
                frames.push(std::mem::take(&mut self.pending));
                bytes = &bytes[end + 1..];
            } else {
                break;
            }
        }
        Ok(frames)
    }

    /// Confirm EOF occurred at a frame boundary.
    ///
    /// # Errors
    /// Returns an error if an oversized or unterminated frame was received.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.failed {
            Err(FrameError::TooLarge)
        } else if self.pending.is_empty() {
            Ok(())
        } else {
            Err(FrameError::Incomplete)
        }
    }
}
