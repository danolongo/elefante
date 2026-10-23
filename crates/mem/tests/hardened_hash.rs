use elefante_mem::{hash::hash_files, hash::read_and_hash, stats::Counters, walk::WalkedFile};
use std::{fs, os::unix::fs::symlink};

#[test]
fn hashing_refuses_links_but_reads_regular_files() {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let ordinary = root.path().join("ordinary");
    fs::write(&ordinary, "safe").unwrap();
    assert_eq!(read_and_hash(&ordinary).unwrap().bytes, b"safe");
    let link = root.path().join("link");
    symlink(&ordinary, &link).unwrap();
    assert!(read_and_hash(&link).is_err());
    fs::remove_file(&link).unwrap();
    fs::hard_link(&ordinary, &link).unwrap();
    assert!(read_and_hash(&link).is_err());
}

#[test]
fn batch_hashing_refuses_swapped_directory_and_parent_paths() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("file"), "outside").unwrap();
    fs::create_dir(root.path().join("dir")).unwrap();
    fs::write(root.path().join("dir/file"), "inside").unwrap();
    let candidate = |path: &str| WalkedFile {
        path: path.into(),
        size: 6,
        mtime_ns: 0,
    };
    assert!(hash_files(root.path(), &[candidate("dir/file")], &Counters::default())[0].is_ok());
    fs::remove_file(root.path().join("dir/file")).unwrap();
    fs::remove_dir(root.path().join("dir")).unwrap();
    symlink(outside.path(), root.path().join("dir")).unwrap();
    assert!(
        hash_files(
            root.path(),
            &[candidate("dir/file"), candidate("../file")],
            &Counters::default()
        )
        .iter()
        .all(Result::is_err)
    );
}

#[test]
fn batch_target_hard_links_remain_readable() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("target")).unwrap();
    fs::write(root.path().join("target/file"), "build").unwrap();
    fs::hard_link(root.path().join("target/file"), root.path().join("alias")).unwrap();
    let files = ["target/file", "alias"].map(|path| WalkedFile {
        path: path.into(),
        size: 5,
        mtime_ns: 0,
    });
    let hashed = hash_files(root.path(), &files, &Counters::default());
    assert_eq!(hashed[0].as_ref().unwrap().bytes, b"build");
    assert!(hashed[1].is_err());
}
