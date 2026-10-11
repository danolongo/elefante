//! Reading a model's reply. Lenient about packaging (code fences, a sentence before the JSON),
//! strict about shape: an entry that does not match the schema is dropped on its own and counted.

use serde_json::Value;

use super::{Candidate, Kind, Relation};

/// Entries read from one reply; more than this are ignored.
const MAX_ENTRIES: usize = 32;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseOutcome {
    pub candidates: Vec<Candidate>,
    /// Entries that were present but malformed.
    pub invalid: usize,
}

/// The first balanced `{ … }` in `text`, honouring strings and escapes.
fn first_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let (mut in_string, mut escaped) = (false, false);
    for (offset, c) in text[start..].char_indices() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

fn handle(value: &Value) -> Option<usize> {
    let raw = match value {
        Value::String(s) => s.trim().trim_start_matches(['m', 'M']).to_owned(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    raw.parse::<usize>().ok().filter(|h| *h >= 1)
}

fn relation(entry: &Value) -> Option<Relation> {
    let Some(relation) = entry.get("relation").filter(|v| !v.is_null()) else {
        return Some(Relation::New);
    };
    let kind = relation
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("new");
    match kind {
        "new" => Some(Relation::New),
        "same_as" => relation
            .get("target")
            .and_then(handle)
            .map(Relation::SameAs),
        "replaces" => relation
            .get("target")
            .and_then(handle)
            .map(Relation::Replaces),
        _ => None,
    }
}

fn entry(entry: &Value) -> Option<Candidate> {
    let text = |field: &str| entry.get(field).and_then(Value::as_str).map(str::to_owned);
    let anchors = match entry.get("anchors") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|a| a.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    Some(Candidate {
        kind: Kind::parse(entry.get("kind")?.as_str()?)?,
        label: text("label")?,
        statement: text("statement")?,
        anchors,
        quote: text("quote")?,
        relation: relation(entry)?,
    })
}

/// Parses a model reply into candidates. `None` when no JSON object with a `memories` array can
/// be found at all, which callers count as a failed extraction.
#[must_use]
pub fn parse_output(text: &str) -> Option<ParseOutcome> {
    let object = first_object(text)?;
    let value: Value = serde_json::from_str(object).ok()?;
    let entries = value.get("memories")?.as_array()?;
    let mut outcome = ParseOutcome::default();
    for item in entries.iter().take(MAX_ENTRIES) {
        match entry(item) {
            Some(candidate) => outcome.candidates.push(candidate),
            None => outcome.invalid += 1,
        }
    }
    Some(outcome)
}
