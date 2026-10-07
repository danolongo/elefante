//! Identifier and path splitting for the search index.
//!
//! SQLite's `unicode61` tokenizer treats `_` as a separator but does not split camelCase, so
//! `turn_lock` is searchable by `turn` and `lock` while `TurnLock` is one token. Splitting is
//! therefore done *before* insertion, by this module, and the same function is applied to the
//! query side so both agree.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Separator,
}

fn class(c: char) -> Class {
    if c.is_lowercase() {
        Class::Lower
    } else if c.is_uppercase() {
        Class::Upper
    } else if c.is_numeric() {
        Class::Digit
    } else if c.is_alphanumeric() {
        // Caseless letters (CJK and similar) are ordinary word characters.
        Class::Lower
    } else {
        Class::Separator
    }
}

/// Splits an identifier into lowercase words.
///
/// Boundaries: any non-alphanumeric character; a lower-case letter or digit followed by an
/// upper-case letter (`turnLock`, `v2Api`); and the end of an acronym (`HTTPServer` → `http`,
/// `server`). Digits stay attached to the word before them, so `sha256` and `utf8` survive whole.
#[must_use]
pub fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (index, &c) in chars.iter().enumerate() {
        let this = class(c);
        if this == Class::Separator {
            flush(&mut current, &mut words);
            continue;
        }
        if !current.is_empty() {
            // Classify the *original* characters: `current` holds lowercased text, which would
            // make every previous character look lower-case and defeat acronym detection.
            let prev = class(chars[index - 1]);
            let next = chars.get(index + 1).copied().map(class);
            let starts_word = this == Class::Upper
                && (matches!(prev, Class::Lower | Class::Digit)
                    || (prev == Class::Upper && next == Some(Class::Lower)));
            if starts_word {
                flush(&mut current, &mut words);
            }
        }
        current.extend(c.to_lowercase());
    }
    flush(&mut current, &mut words);
    words
}

fn flush(current: &mut String, words: &mut Vec<String>) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

/// The searchable form of one identifier: its words, then the unsplit lowercase form when the
/// identifier had more than one word, so `turnlock` finds `TurnLock` as well as `turn lock`.
#[must_use]
pub fn identifier_terms(name: &str) -> String {
    let words = split_words(name);
    let mut terms = words.join(" ");
    if words.len() > 1 {
        terms.push(' ');
        terms.push_str(&words.concat());
    }
    terms
}

/// The searchable form of a repository-relative path: the words of every component.
///
/// A file's extension contributes its own word (`rs`) but not a joined form, so `lib.rs` yields
/// `lib rs` rather than also indexing the meaningless `librs`; the stem keeps its joined form,
/// so `turn_lock.rs` is still found by `turnlock`.
#[must_use]
pub fn path_terms(path: &str) -> String {
    let mut seen = Vec::<String>::new();
    let mut add = |terms: &str| {
        for term in terms.split(' ') {
            if !term.is_empty() && !seen.iter().any(|t| t == term) {
                seen.push(term.to_owned());
            }
        }
    };
    for component in path.split('/') {
        match component.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => {
                add(&identifier_terms(stem));
                add(&split_words(extension).join(" "));
            }
            _ => add(&identifier_terms(component)),
        }
    }
    seen.join(" ")
}
