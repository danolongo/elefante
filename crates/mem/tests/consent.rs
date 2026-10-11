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
        consent::read_for(&path, &key, "provider/one"),
        Some(record(ConsentAnswer::Learn, "provider/one"))
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    consent::write(&path, &key, &record(ConsentAnswer::Off, "provider/two")).unwrap();
    assert_eq!(
        consent::read_for(&path, &key, "provider/two"),
        Some(record(ConsentAnswer::Off, "provider/two"))
    );
    assert_eq!(
        consent::read_for(&path, &key, "provider/one"),
        Some(record(ConsentAnswer::Learn, "provider/one")),
        "an answer for another extractor is untouched"
    );
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "no temporary file remains"
    );
}

#[test]
fn one_answer_per_extractor_is_replaced_in_place_and_never_leaks_to_another() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONSENT_FILE);
    let key = MemKey::from_bytes([7; 32]);
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/one")).unwrap();
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/two")).unwrap();
    consent::write(&path, &key, &record(ConsentAnswer::Off, "provider/one")).unwrap();
    let mut all = consent::read_all(&path, &key);
    all.sort_by(|a, b| a.extractor.cmp(&b.extractor));
    assert_eq!(
        all,
        vec![
            record(ConsentAnswer::Off, "provider/one"),
            record(ConsentAnswer::Learn, "provider/two")
        ]
    );
    assert_eq!(consent::read_for(&path, &key, "provider/three"), None);
}

#[test]
fn missing_malformed_tampered_and_foreign_consent_are_unset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONSENT_FILE);
    let key = MemKey::from_bytes([7; 32]);
    assert_eq!(consent::read_for(&path, &key, "provider/model"), None);

    fs::write(&path, b"not json").unwrap();
    assert_eq!(consent::read_for(&path, &key, "provider/model"), None);

    // Changing one record's extractor breaks that record only.
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/model")).unwrap();
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/kept")).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let records = value["records"].as_array_mut().unwrap();
    let tampered = records
        .iter_mut()
        .find(|r| r["extractor"] == "provider/model")
        .unwrap();
    tampered["extractor"] = "other/model".into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(consent::read_for(&path, &key, "provider/model"), None);
    assert_eq!(consent::read_for(&path, &key, "other/model"), None);
    assert_eq!(
        consent::read_for(&path, &key, "provider/kept"),
        Some(record(ConsentAnswer::Learn, "provider/kept")),
        "the untouched record still verifies"
    );

    // Flipping an answer is caught the same way.
    consent::write(&path, &key, &record(ConsentAnswer::Off, "provider/model")).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for r in value["records"].as_array_mut().unwrap() {
        if r["extractor"] == "provider/model" {
            r["answer"] = "learn".into();
        }
    }
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(consent::read_for(&path, &key, "provider/model"), None);

    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/model")).unwrap();
    assert_eq!(
        consent::read_for(&path, &MemKey::from_bytes([8; 32]), "provider/model"),
        None,
        "another installation's key reads nothing"
    );
}

#[test]
fn the_first_file_shape_is_read_as_a_list_of_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONSENT_FILE);
    let key = MemKey::from_bytes([7; 32]);
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/old")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&value["records"][0]).unwrap()).unwrap();
    assert_eq!(
        consent::read_for(&path, &key, "provider/old"),
        Some(record(ConsentAnswer::Learn, "provider/old"))
    );
    consent::write(&path, &key, &record(ConsentAnswer::Learn, "provider/new")).unwrap();
    assert_eq!(consent::read_all(&path, &key).len(), 2);
}
