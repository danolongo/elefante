use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemError {
    #[error("memory store i/o failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("memory store query failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path} has schema version {found}, newer than the {supported} this build understands")]
    UnsupportedSchema {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    #[error("{path} is not a valid memory store: {reason}")]
    Corrupt { path: PathBuf, reason: String },
    #[error("memory is disabled for {path}: {reason}")]
    Refused { path: PathBuf, reason: &'static str },
    #[error("indexing aborted: {0}")]
    Aborted(String),
    #[error("memory store worker is gone")]
    ActorGone,
    #[error("memory store worker panicked while handling a request")]
    ActorPanicked,
}

impl MemError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
