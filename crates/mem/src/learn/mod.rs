//! Learned memory: durable notes distilled from conversations. See `docs/PLAN.md` § Memory
//! subsystem, and this crate's README for the contract.
//!
//! This module never talks to a model. A caller hands it *candidates* (what a model proposed) and
//! the *excerpt* they were drawn from, and every decision that matters is made here, by code that
//! can be tested: whether the quote is real, whether the wording is safe to show, whether the note
//! duplicates or contradicts another, what its confidence is, and whether it may be shown at all.
//!
//! Only rows with `status = 'active'` are searchable and rendered. `pending` rows (the assistant's
//! own claims, unconfirmed by the user) and `superseded` rows have no full-text entry.

mod admin;
mod compact;
mod ingest;
mod parse;
mod service;
mod sim;
mod store;
mod verify;

use std::fmt;

pub use admin::{LearnedRow, approve, clear, forget, list};
pub use compact::{
    ACTIVE_CAP, COMPACT_INTERVAL_NS, CompactReport, SOFT_CAP, SUPERSEDED_RETENTION_DAYS, compact,
    compact_if_due,
};
pub use ingest::{IngestInput, IngestReport, Rejection, ingest};
pub use parse::{ParseOutcome, parse_output};
pub use service::{ingest_async, nearest_async};
pub use sim::{is_duplicate, jaccard, overlap, tokens};
pub use store::{LearnedNode, load_active_and_pending, nearest};
pub use verify::{has_explicit_cue, is_dangerous_token};

// ----- limits: named, so tuning changes numbers and never structure -----

pub const MAX_LABEL_CHARS: usize = 60;
pub const MAX_STATEMENT_CHARS: usize = 180;
pub const MAX_QUOTE_CHARS: usize = 160;
pub const MAX_ANCHORS: usize = 5;
/// Candidates taken from one extraction; the rest are ignored.
pub const MAX_CANDIDATES: usize = 8;

/// Auto-duplicate threshold on Jaccard similarity of content words.
pub const DUP_JACCARD: f64 = 0.9;
/// A model verdict (`same_as`, `replaces`) is accepted only above this overlap, or with a shared anchor.
pub const RELATED_OVERLAP: f64 = 0.4;

pub const CONFIDENCE_EXPLICIT: f64 = 0.9;
pub const CONFIDENCE_STATED: f64 = 0.6;
pub const CONFIDENCE_ASSISTANT: f64 = 0.4;
/// Fraction of the remaining distance to 1.0 gained by one reinforcement.
pub const REINFORCE_GAIN: f64 = 0.3;
pub const CONFIDENCE_CAP: f64 = 0.99;
/// Below this effective confidence a note is no longer retrieved and may be compacted.
pub const CONFIDENCE_FLOOR: f64 = 0.1;

pub const HALF_LIFE_PREFERENCE_DAYS: f64 = 180.0;
pub const HALF_LIFE_CONVENTION_DAYS: f64 = 180.0;
pub const HALF_LIFE_DECISION_DAYS: f64 = 90.0;
pub const HALF_LIFE_FACT_DAYS: f64 = 30.0;

pub(crate) const NS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// SQL fragment: the learned kinds.
pub(crate) const KINDS_SQL: &str = "('fact','preference','decision','convention')";
/// `node.source` prefix of every learned row.
pub const SOURCE_PREFIX: &str = "learned:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Fact,
    Preference,
    Decision,
    Convention,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Convention => "convention",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fact" => Some(Self::Fact),
            "preference" => Some(Self::Preference),
            "decision" => Some(Self::Decision),
            "convention" => Some(Self::Convention),
            _ => None,
        }
    }

    /// Days for effective confidence to halve with no reinforcement.
    #[must_use]
    pub fn half_life_days(self) -> f64 {
        match self {
            Self::Preference => HALF_LIFE_PREFERENCE_DAYS,
            Self::Convention => HALF_LIFE_CONVENTION_DAYS,
            Self::Decision => HALF_LIFE_DECISION_DAYS,
            Self::Fact => HALF_LIFE_FACT_DAYS,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a note's evidence came from. The harness assigns it; the model never does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A user message containing an explicit cue ("remember…", "from now on…").
    Explicit,
    /// A user message, no cue.
    Stated,
    /// Assistant text only. Never shown until a user confirms it.
    Assistant,
}

impl Origin {
    #[must_use]
    pub fn source(self) -> &'static str {
        match self {
            Self::Explicit => "learned:explicit",
            Self::Stated => "learned:stated",
            Self::Assistant => "learned:assistant",
        }
    }

    #[must_use]
    pub fn from_source(source: &str) -> Option<Self> {
        match source {
            "learned:explicit" => Some(Self::Explicit),
            "learned:stated" => Some(Self::Stated),
            "learned:assistant" => Some(Self::Assistant),
            _ => None,
        }
    }

    #[must_use]
    pub fn initial_confidence(self) -> f64 {
        match self {
            Self::Explicit => CONFIDENCE_EXPLICIT,
            Self::Stated => CONFIDENCE_STATED,
            Self::Assistant => CONFIDENCE_ASSISTANT,
        }
    }

    #[must_use]
    pub fn is_user(self) -> bool {
        !matches!(self, Self::Assistant)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    Pending,
    Superseded,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Superseded => "superseded",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "active" => Some(Self::Active),
            "pending" => Some(Self::Pending),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// Who spoke a turn of the excerpt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    User,
    Assistant,
}

/// One turn of conversation text. Only text blocks ever become a `Turn`: never a tool result,
/// a tool call, or reasoning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub speaker: Speaker,
    pub text: String,
}

/// What the model said a candidate's relation to a note it was shown is. `usize` is the 1-based
/// handle (`m3`) of a note in the list the model was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    New,
    SameAs(usize),
    Replaces(usize),
}

/// A note a model proposed, after strict validation of its shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: Kind,
    pub label: String,
    pub statement: String,
    pub anchors: Vec<String>,
    pub quote: String,
    pub relation: Relation,
}

/// Effective confidence at `now_ns`: the stored confidence, halved every half-life since the note
/// was last reinforced. Computed on read, so nothing is written as time passes.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn effective_confidence(
    confidence: f64,
    kind: Kind,
    last_reinforced_ns: i64,
    now_ns: i64,
) -> f64 {
    let age_days = (now_ns - last_reinforced_ns).max(0) as f64 / NS_PER_DAY as f64;
    confidence * 0.5_f64.powf(age_days / kind.half_life_days())
}
