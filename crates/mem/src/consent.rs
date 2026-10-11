//! Tamper-evident, extractor-bound consent for learned memory.
//!
//! One answer is kept per extractor model, so agreeing to learn through one model neither grants
//! nor withdraws anything for another.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{MemError, MemKey, layout::create_private_dir};

/// File name beneath the directory that contains the user's credentials and memory key.
pub const CONSENT_FILE: &str = "memory-consent";

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

/// The durable answer a person gave for one extractor model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentAnswer {
    Learn,
    Off,
}

impl ConsentAnswer {
    fn as_str(self) -> &'static str {
        match self {
            Self::Learn => "learn",
            Self::Off => "off",
        }
    }
}

/// A verified consent answer and the extractor model it authorizes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentRecord {
    pub answer: ConsentAnswer,
    pub extractor: String,
}

#[derive(Serialize, Deserialize)]
struct StoredConsent {
    answer: ConsentAnswer,
    extractor: String,
    stamp: String,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    records: Vec<StoredConsent>,
}

/// The records in a consent file, whatever their stamps say. A file holding a single bare record
/// (the first shape) is read as a list of one.
fn stored(path: &Path) -> Vec<StoredConsent> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    if let Ok(file) = serde_json::from_slice::<StoredFile>(&bytes) {
        return file.records;
    }
    serde_json::from_slice::<StoredConsent>(&bytes)
        .map(|record| vec![record])
        .unwrap_or_default()
}

fn binding(answer: ConsentAnswer, extractor: &str) -> String {
    format!("{}\0{extractor}", answer.as_str())
}

/// Reads and verifies a consent file, returning the records whose stamps match. Missing,
/// unreadable, malformed, or unstamped files, and any single record that fails its stamp, are
/// deliberately indistinguishable from no answer for that extractor.
#[must_use]
pub fn read_all(path: &Path, key: &MemKey) -> Vec<ConsentRecord> {
    stored(path)
        .into_iter()
        .filter(|record| {
            key.stamp_matches(
                "consent",
                &binding(record.answer, &record.extractor),
                &record.stamp,
            )
        })
        .map(|record| ConsentRecord {
            answer: record.answer,
            extractor: record.extractor,
        })
        .collect()
}

/// The verified answer for one extractor, if any.
#[must_use]
pub fn read_for(path: &Path, key: &MemKey, extractor: &str) -> Option<ConsentRecord> {
    read_all(path, key)
        .into_iter()
        .rev()
        .find(|record| record.extractor == extractor)
}

/// Atomically persists a consent answer in a private file stamped by the memory key, replacing any
/// earlier answer for the same extractor and keeping the verified answers for the others. A record
/// that fails its stamp is dropped rather than carried forward.
///
/// # Errors
/// Returns an I/O or serialization error without replacing the previous answer.
pub fn write(path: &Path, key: &MemKey, record: &ConsentRecord) -> Result<(), MemError> {
    let parent = path.parent().ok_or_else(|| MemError::Refused {
        path: path.to_owned(),
        reason: "there is no directory to keep memory consent in",
    })?;
    create_private_dir(parent)?;
    let mut records: Vec<ConsentRecord> = read_all(path, key)
        .into_iter()
        .filter(|kept| kept.extractor != record.extractor)
        .collect();
    records.push(record.clone());
    let file = StoredFile {
        records: records
            .into_iter()
            .map(|record| StoredConsent {
                stamp: key.stamp("consent", &binding(record.answer, &record.extractor)),
                answer: record.answer,
                extractor: record.extractor,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&file).map_err(|error| {
        MemError::io(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        )
    })?;
    let temp = parent.join(format!(
        ".memory-consent-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).map_err(|e| MemError::io(&temp, e))?;
        file.write_all(&bytes).map_err(|e| MemError::io(&temp, e))?;
        file.write_all(b"\n").map_err(|e| MemError::io(&temp, e))?;
        file.sync_all().map_err(|e| MemError::io(&temp, e))?;
        fs::rename(&temp, path).map_err(|e| MemError::io(path, e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
