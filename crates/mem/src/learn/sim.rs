//! Lexical similarity between two notes.
//!
//! Deliberately simple and fully deterministic: lower-cased content words, lightly stemmed, with
//! **negation words kept**. Dropping them as stopwords would make "use tabs" and "don't use tabs"
//! identical, which is the one mistake a duplicate detector must not make.

use std::collections::BTreeSet;

use crate::terms::split_words;

/// Words that flip or redirect a statement. They stay in the token set and are compared on their
/// own when deciding whether two notes are duplicates.
const NEGATIONS: &[&str] = &[
    "not", "no", "never", "without", "avoid", "instead", "stop", "dont", "neither", "nor",
];

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "is", "are", "be", "been", "was",
    "were", "with", "that", "this", "it", "its", "as", "at", "by", "from", "we", "you", "i", "our",
    "your", "my", "should", "must", "will", "would", "can", "could", "do", "does", "did", "have",
    "has", "had", "when", "then", "than", "so", "if", "also", "please", "all", "any", "user",
    "prefers", "uses",
];

fn normalise_contractions(text: &str) -> String {
    let text = text.replace(['\u{2019}', '\u{2018}'], "'").to_lowercase();
    text.replace("can't", "can not")
        .replace("won't", "will not")
        .replace("n't", " not")
}

/// A crude, consistent stemmer: plural and common derivational suffixes. Not linguistically
/// correct; it only needs to map the usual inflections of one word to one token.
fn stem(word: &str) -> String {
    let mut w = word.to_owned();
    if w.len() > 4 && w.ends_with("ies") {
        w.truncate(w.len() - 3);
        w.push('y');
    } else if w.len() > 4
        && (w.ends_with("sses") || w.ends_with("xes") || w.ends_with("ches") || w.ends_with("shes"))
    {
        w.truncate(w.len() - 2);
    } else if w.len() > 3
        && w.ends_with('s')
        && !w.ends_with("ss")
        && !w.ends_with("us")
        && !w.ends_with("is")
    {
        w.truncate(w.len() - 1);
    }
    for suffix in [
        "ations", "ation", "ments", "ment", "ness", "ings", "ing", "ed", "ly",
    ] {
        if w.len() > suffix.len() + 3 && w.ends_with(suffix) {
            w.truncate(w.len() - suffix.len());
            break;
        }
    }
    w
}

/// The comparable content words of `text`.
#[must_use]
pub fn tokens(text: &str) -> BTreeSet<String> {
    let normalised = normalise_contractions(text);
    split_words(&normalised)
        .into_iter()
        .filter(|w| w.chars().count() >= 2 || w == "a")
        .filter(|w| NEGATIONS.contains(&w.as_str()) || !STOPWORDS.contains(&w.as_str()))
        .filter(|w| !w.chars().all(char::is_numeric))
        .map(|w| {
            if NEGATIONS.contains(&w.as_str()) {
                w
            } else {
                stem(&w)
            }
        })
        .collect()
}

fn negations(set: &BTreeSet<String>) -> BTreeSet<&str> {
    set.iter()
        .map(String::as_str)
        .filter(|w| NEGATIONS.contains(w))
        .collect()
}

/// `|a ∩ b| / |a ∪ b|`. Zero when both are empty.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// `|a ∩ b| / min(|a|, |b|)`. Zero when either is empty.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let smaller = a.len().min(b.len());
    if smaller == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / smaller as f64
}

/// Whether two token sets say the same thing in different arrangement: near-identical content and
/// the same negations. A single flipped `not` in a long sentence leaves Jaccard high, so the
/// negation sets are compared separately and any difference vetoes a duplicate.
#[must_use]
pub fn is_duplicate(a: &BTreeSet<String>, b: &BTreeSet<String>, threshold: f64) -> bool {
    negations(a) == negations(b) && jaccard(a, b) >= threshold
}
