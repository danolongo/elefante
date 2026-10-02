//! Turning one file's content into symbols and imports. Design: `docs/M2-DESIGN.md` § 7.
//!
//! Extraction is a pure function of `(content, extractor)`: no path, no database. That is what
//! lets its result be cached by content hash and reused across renames, branch switches, and a
//! rebuilt graph.

pub mod cache;
mod markdown;
mod rust;
mod typescript;

use std::{ops::ControlFlow, sync::OnceLock, time::Duration, time::Instant};

use elefante_guard::Scrubber;
use serde::{Deserialize, Serialize};
use tree_sitter::{ParseOptions, Parser, Tree};

use crate::stats::Counters;

pub const MAX_SYMBOLS: usize = 2_000;
pub const MAX_IMPORTS: usize = 1_000;
pub const MAX_SIGNATURE_CHARS: usize = 200;
pub const MAX_DOC_CHARS: usize = 160;
pub const MAX_FILE_DOC_CHARS: usize = 300;
/// A file that takes longer than this to parse is abandoned rather than stalling the indexer.
pub const PARSE_BUDGET: Duration = Duration::from_secs(1);

pub const RECORD_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extraction {
    #[serde(default)]
    pub v: u32,
    /// One sentence describing the file: a module doc, a file-level comment, or the headings.
    #[serde(default)]
    pub file_doc: String,
    #[serde(default)]
    pub symbols: Vec<Symbol>,
    #[serde(default)]
    pub imports: Vec<Import>,
    /// A cap was hit or the parse was abandoned; what is here is a prefix of the truth.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub kind: String,
    pub name: String,
    /// Path-independent, `::`-nested (`Session::prompt`).
    pub qname: String,
    /// The declaration up to its body or initializer, string literals elided.
    pub signature: String,
    /// The first sentence of the doc comment, or empty.
    pub doc: String,
    pub line: u32,
    pub end_line: u32,
    /// For `impl Trait for Type`, the trait; the method itself is named after the type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_trait: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// `mod`, `use`, `import`, or `reexport`.
    pub kind: String,
    pub spec: String,
}

/// A language the indexer can extract from.
pub trait Language: Sync {
    /// Stable id including a version, e.g. `rust@1`. Bumping it invalidates exactly this
    /// language's cached extractions.
    fn id(&self) -> &'static str;

    fn extract_with_budget(
        &self,
        source: &str,
        counters: &Counters,
        budget: Duration,
    ) -> Extraction;

    fn extract(&self, source: &str, counters: &Counters) -> Extraction {
        self.extract_with_budget(source, counters, PARSE_BUDGET)
    }
}

static RUST: rust::Rust = rust::Rust;
static TYPESCRIPT: typescript::TypeScript = typescript::TypeScript { tsx: false };
static TSX: typescript::TypeScript = typescript::TypeScript { tsx: true };
static MARKDOWN: markdown::Markdown = markdown::Markdown;

/// The extractor for a repository-relative path, if any. Everything else is a file node with no
/// body: findable by path, not by content.
#[must_use]
pub fn language_for(path: &str) -> Option<&'static dyn Language> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (_, extension) = name.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some(&RUST),
        "ts" | "mts" | "cts" => Some(&TYPESCRIPT),
        "tsx" => Some(&TSX),
        "md" | "markdown" => Some(&MARKDOWN),
        _ => None,
    }
}

/// The single place a tree-sitter parse is invoked, and so the single place `parsed` counts.
pub(crate) fn parse(
    language: &tree_sitter::Language,
    source: &str,
    counters: &Counters,
    budget: Duration,
) -> Option<Tree> {
    counters.add_parsed(1);
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    let started = Instant::now();
    let mut over_budget = |_: &tree_sitter::ParseState| {
        if started.elapsed() >= budget {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let bytes = source.as_bytes();
    parser.parse_with_options(
        &mut |offset, _| &bytes[offset.min(bytes.len())..],
        None,
        Some(ParseOptions::new().progress_callback(&mut over_budget)),
    )
}

fn scrubber() -> &'static Scrubber {
    static SCRUBBER: OnceLock<Scrubber> = OnceLock::new();
    SCRUBBER.get_or_init(Scrubber::new)
}

impl Extraction {
    pub(crate) fn empty() -> Self {
        Self {
            v: RECORD_VERSION,
            ..Self::default()
        }
    }

    /// Passes every string through the same `Scrubber` that guards tool output. This is the
    /// second layer; the first is that signatures never contain initializers or literals.
    pub(crate) fn scrubbed(mut self) -> Self {
        let scrub = |s: &mut String| *s = scrubber().scrub(s);
        scrub(&mut self.file_doc);
        for symbol in &mut self.symbols {
            scrub(&mut symbol.name);
            scrub(&mut symbol.qname);
            scrub(&mut symbol.signature);
            scrub(&mut symbol.doc);
        }
        for import in &mut self.imports {
            scrub(&mut import.spec);
        }
        self
    }

    pub(crate) fn push_symbol(&mut self, symbol: Symbol) -> bool {
        if self.symbols.len() >= MAX_SYMBOLS {
            self.truncated = true;
            return false;
        }
        self.symbols.push(symbol);
        true
    }

    pub(crate) fn push_import(&mut self, kind: &str, spec: String) {
        if spec.is_empty()
            || self
                .imports
                .iter()
                .any(|i| i.kind == kind && i.spec == spec)
        {
            return;
        }
        if self.imports.len() >= MAX_IMPORTS {
            self.truncated = true;
            return;
        }
        self.imports.push(Import {
            kind: kind.to_owned(),
            spec,
        });
    }
}

// ----- text helpers shared by the extractors -----

/// The last row a node occupies. A node that ends with a newline ends at column 0 of the next
/// row, and counting that row would make a comment look adjacent to code a blank line below it.
pub(crate) fn last_row(node: tree_sitter::Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[must_use]
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The first sentence of the first paragraph, whitespace-collapsed and capped.
pub(crate) fn first_sentence(raw: &str, max: usize) -> String {
    let paragraph = raw
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("");
    let text = collapse_whitespace(paragraph);
    let end = text
        .char_indices()
        .find(|&(index, c)| {
            matches!(c, '.' | '?' | '!')
                && text[index + c.len_utf8()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map_or(text.len(), |(index, c)| index + c.len_utf8());
    truncate_chars(&text[..end], max)
}

/// Replaces the contents of string literals with `…`, keeping the delimiters.
///
/// Applied to signatures so a default argument cannot carry a credential into the graph.
/// `single_quotes` is false for Rust, where `'` also starts lifetimes and char literals.
pub(crate) fn mask_strings(text: &str, single_quotes: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut delimiter: Option<char> = None;
    let mut escaped = false;
    for c in text.chars() {
        match delimiter {
            None => {
                out.push(c);
                if c == '"' || c == '`' || (single_quotes && c == '\'') {
                    delimiter = Some(c);
                    out.push('…');
                }
            }
            Some(open) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == open {
                    out.push(c);
                    delimiter = None;
                }
            }
        }
    }
    out
}

pub(crate) fn clean_signature(raw: &str, single_quotes: bool) -> String {
    let collapsed = collapse_whitespace(raw);
    let masked = mask_strings(&collapsed, single_quotes);
    let trimmed = masked
        .trim_end_matches(|c: char| matches!(c, '=' | ';' | '{' | ',') || c.is_whitespace())
        .trim_end_matches("=>")
        .trim_end();
    truncate_chars(trimmed, MAX_SIGNATURE_CHARS)
}
