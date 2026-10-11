//! Per-candidate checks, all run before anything is written.
//!
//! The model's output is untrusted input to a persistence path. Nothing here asks whether a note is
//! *true*; it checks the things that can be checked mechanically: the quote exists, the wording
//! carries no payload the user did not write, the text survives the scrubber unchanged, and the
//! evidence came from a speaker that is allowed to vouch for it.

use std::sync::OnceLock;

use elefante_guard::Scrubber;

use super::{
    Candidate, MAX_LABEL_CHARS, MAX_QUOTE_CHARS, MAX_STATEMENT_CHARS, Origin, Speaker, Turn,
    display_safe,
};

/// Phrases that address an agent rather than describe a repository or a person's preference.
const INSTRUCTION_SHAPES: &[&str] = &[
    "ignore previous",
    "ignore all previous",
    "ignore prior",
    "ignore the above",
    "ignore earlier",
    "ignore your instructions",
    "disregard",
    "system prompt",
    "you must",
    "you are now",
    "new instructions",
];

/// Programs that turn a sentence into an action. If one appears in a note it must be in the words
/// the user actually wrote.
const SHELL_VERBS: &[&str] = &[
    "curl",
    "wget",
    "sh",
    "bash",
    "zsh",
    "sudo",
    "rm",
    "eval",
    "exec",
    "chmod",
    "chown",
    "ssh",
    "scp",
    "nc",
    "ncat",
    "netcat",
    "powershell",
];

/// Characters that make a token a path, URL, command fragment or identifier rather than a word.
const PAYLOAD_CHARS: &[char] = &[
    '/', '\\', '$', '`', '|', ';', '&', '<', '>', '(', ')', '{', '}', '=', '@', '#', ':', '.',
];

/// Cues that a user is asking for something to be remembered or changed going forward.
const CUES: &[&str] = &[
    "remember",
    "from now on",
    "going forward",
    "always",
    "never",
    "don't",
    "dont",
    "do not",
    "instead",
    "actually, use",
    "keep in mind",
    "make sure you",
    "i prefer",
    "we always",
    "we never",
    "stop doing",
];

/// Why a candidate was turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Empty,
    TooLong,
    QuoteNotFound,
    DangerousToken,
    InstructionShape,
    Secret,
}

pub struct Verified {
    /// The cleaned candidate (whitespace collapsed, quote capped).
    pub candidate: Candidate,
    pub origin: Origin,
}

fn scrubber() -> &'static Scrubber {
    static SCRUBBER: OnceLock<Scrubber> = OnceLock::new();
    SCRUBBER.get_or_init(Scrubber::new)
}

/// Case- and whitespace-insensitive form used to compare a quote with the conversation.
fn fold(text: &str) -> String {
    display_safe(&text.replace(['\u{2019}', '\u{2018}'], "'").to_lowercase())
}

/// Words only: lower-cased, every non-alphanumeric character a space. Two texts that differ only in
/// punctuation, quoting, backticks or an ellipsis compare equal, which is what a model copying a
/// quote will get subtly wrong, while the words and their order must still match.
fn loose(text: &str) -> String {
    display_safe(text)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `phrase` occurs in `text` as whole words (both already [`loose`]). An empty phrase is
/// trivially present.
fn has_phrase(text: &str, phrase: &str) -> bool {
    phrase.is_empty() || format!(" {text} ").contains(&format!(" {phrase} "))
}

/// Whether `text` asks for something to be remembered or changed. The list is English-only and
/// only affects *when* extraction runs and how confident a note starts, never whether one is kept.
#[must_use]
pub fn has_explicit_cue(text: &str) -> bool {
    let folded = fold(text);
    CUES.iter().any(|cue| folded.contains(cue))
}

fn trim_token(token: &str) -> &str {
    let token = token.trim_start_matches(['(', '"', '\'', '[', '{', '`']);
    token.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'', ']', '}', '`'])
}

/// Whether `token` looks like a payload: a path, URL, command fragment, identifier or shell verb.
#[must_use]
pub fn is_dangerous_token(token: &str) -> bool {
    // A code span is a command or identifier whatever is inside it.
    if token.contains('`') {
        return true;
    }
    let token = trim_token(token);
    if token.is_empty() || matches!(token.to_lowercase().as_str(), "e.g" | "i.e" | "etc") {
        return false;
    }
    token.chars().any(|c| PAYLOAD_CHARS.contains(&c))
        || SHELL_VERBS.contains(&token.to_lowercase().as_str())
}

fn dangerous_tokens_outside(text: &str, evidence: &str) -> bool {
    text.split_whitespace()
        .filter(|token| is_dangerous_token(token))
        .any(|token| !has_phrase(evidence, &loose(trim_token(token))))
}

fn instruction_shaped(text: &str) -> bool {
    let folded = fold(text);
    INSTRUCTION_SHAPES
        .iter()
        .any(|phrase| folded.contains(phrase))
}

fn cap_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// What the best evidence for a quote is.
struct Evidence {
    speaker: Speaker,
    /// Some turn of that speaker containing the quote carries an explicit cue.
    cue: bool,
    /// The words of every turn of that speaker that contains the quote ([`loose`]). Wording in a
    /// note may use anything the speaker wrote in those turns, not only the quoted part: a model
    /// that quotes a sentence and restates the example that follows it has not invented the example.
    words: String,
}

/// Finds the best evidence for `quote`: a user turn if any contains it, else an assistant turn.
fn evidence(quote: &str, excerpt: &[Turn]) -> Option<Evidence> {
    let wanted = loose(quote);
    if wanted.is_empty() {
        return None;
    }
    let mut found: [(bool, bool, Vec<String>); 2] = Default::default();
    for turn in excerpt {
        let words = loose(&turn.text);
        if !has_phrase(&words, &wanted) {
            continue;
        }
        let slot = match turn.speaker {
            Speaker::User => 0,
            Speaker::Assistant => 1,
        };
        found[slot].0 = true;
        found[slot].1 |= has_explicit_cue(&turn.text);
        found[slot].2.push(words);
    }
    let [user, assistant] = found;
    if user.0 {
        Some(Evidence {
            speaker: Speaker::User,
            cue: user.1,
            words: user.2.join(" "),
        })
    } else if assistant.0 {
        Some(Evidence {
            speaker: Speaker::Assistant,
            cue: false,
            words: assistant.2.join(" "),
        })
    } else {
        None
    }
}

/// Runs every per-candidate check.
///
/// # Errors
/// The first [`Reject`] that applies.
pub fn verify(candidate: &Candidate, excerpt: &[Turn]) -> Result<Verified, Reject> {
    // A label is for search and for a person scanning a list; it is never shown to a model, so one
    // that runs long is cut rather than costing the whole note.
    let label = cap_chars(&display_safe(&candidate.label), MAX_LABEL_CHARS);
    let statement = display_safe(&candidate.statement);
    let quote = cap_chars(&display_safe(&candidate.quote), MAX_QUOTE_CHARS);
    if label.is_empty() || statement.is_empty() || quote.is_empty() {
        return Err(Reject::Empty);
    }
    if statement.chars().count() > MAX_STATEMENT_CHARS {
        return Err(Reject::TooLong);
    }

    let Some(evidence) = evidence(&quote, excerpt) else {
        return Err(Reject::QuoteNotFound);
    };

    // Anything the scrubber would change is a secret shape: drop the note, never store a redaction.
    for text in [&label, &statement, &quote] {
        if scrubber().scrub(text) != *text {
            return Err(Reject::Secret);
        }
    }
    if instruction_shaped(&label) || instruction_shaped(&statement) {
        return Err(Reject::InstructionShape);
    }
    if dangerous_tokens_outside(&label, &evidence.words)
        || dangerous_tokens_outside(&statement, &evidence.words)
    {
        return Err(Reject::DangerousToken);
    }

    let origin = match evidence.speaker {
        Speaker::User if evidence.cue => Origin::Explicit,
        Speaker::User => Origin::Stated,
        Speaker::Assistant => Origin::Assistant,
    };
    Ok(Verified {
        candidate: Candidate {
            label,
            statement,
            quote,
            ..candidate.clone()
        },
        origin,
    })
}
