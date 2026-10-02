use std::time::Duration;

use super::{Extraction, Language, MAX_FILE_DOC_CHARS, collapse_whitespace, truncate_chars};
use crate::stats::Counters;

pub struct Markdown;

const MAX_HEADINGS: usize = 12;

impl Language for Markdown {
    fn id(&self) -> &'static str {
        "markdown@1"
    }

    /// Headings by line scan, not tree-sitter: a document's outline is all that is searchable,
    /// and a grammar would add a dependency for no more signal.
    fn extract_with_budget(&self, source: &str, _: &Counters, _: Duration) -> Extraction {
        let mut headings = Vec::new();
        let mut fence: Option<&str> = None;
        for line in source.lines() {
            let trimmed = line.trim_start();
            let marker = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m));
            if let Some(marker) = marker {
                fence = match fence {
                    None => Some(marker),
                    Some(open) if open == marker => None,
                    other => other,
                };
                continue;
            }
            if fence.is_some() {
                continue;
            }
            let hashes = trimmed.chars().take_while(|&c| c == '#').count();
            if (1..=6).contains(&hashes) {
                let rest = &trimmed[hashes..];
                if rest.starts_with(char::is_whitespace) {
                    let title = collapse_whitespace(rest.trim().trim_end_matches('#'));
                    if !title.is_empty() {
                        headings.push(title);
                    }
                }
            }
        }
        let mut out = Extraction::empty();
        if headings.len() > MAX_HEADINGS {
            out.truncated = true;
        }
        headings.truncate(MAX_HEADINGS);
        out.file_doc = truncate_chars(&headings.join(" · "), MAX_FILE_DOC_CHARS);
        out.scrubbed()
    }
}
