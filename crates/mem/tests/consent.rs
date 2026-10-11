use std::{fs, os::unix::fs::PermissionsExt as _};

use elefante_mem::{
    MemKey,
    consent::{self, CONSENT_FILE, ConsentAnswer, ConsentRecord},
};

fn record(answer: ConsentAnswer, extractor: &str) -> ConsentRecord {
    ConsentRecord {
        answer,
        extractor: extractor.to_owned(),
    }
}

#[test]
fn consent_round_trips_privately_and_replaces_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join(CONSENT_FILE);
    let key = MemKey::from_bytes([7; 32]);
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/one")).unwrap();
    assert_eq!(
        consent::read(&path, &key),
        Some(record(ConsentAnswer::Learn, "provider/one"))
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    consent::write(&path, &key, &record(ConsentAnswer::Off, "provider/two")).unwrap();
    assert_eq!(
        consent::read(&path, &key),
        Some(record(ConsentAnswer::Off, "provider/two"))
    );
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "no temporary file remains"
    );
}

#[test]
fn missing_malformed_tampered_and_foreign_consent_are_unset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONSENT_FILE);
    let key = MemKey::from_bytes([7; 32]);
    assert_eq!(consent::read(&path, &key), None);

    fs::write(&path, b"not json").unwrap();
    assert_eq!(consent::read(&path, &key), None);

    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/model")).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["extractor"] = "other/model".into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(consent::read(&path, &key), None);

    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/model")).unwrap();
    assert_eq!(consent::read(&path, &MemKey::from_bytes([8; 32])), None);
}
