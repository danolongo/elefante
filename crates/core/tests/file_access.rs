use elefante_core::file_access::FileAccess;
use std::{fs, os::unix::fs::symlink};

#[test]
fn reads_and_atomic_writes_refuse_links_and_traversal() {
    let root = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(root.path()).unwrap();
    let access = FileAccess::new(&root).unwrap();
    access.write("nested/file", b"safe").unwrap();
    assert_eq!(access.read("nested/file").unwrap(), b"safe");
    assert!(access.read("../outside").is_err());
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), "outside").unwrap();
    symlink(outside.path(), root.join("link")).unwrap();
    assert!(access.read("link").is_err());
    assert!(access.write("link", b"bad").is_err());
    fs::remove_file(root.join("link")).unwrap();
    fs::hard_link(outside.path(), root.join("link")).unwrap();
    assert!(access.read("link").is_err());
    assert!(access.write("link", b"bad").is_err());
    assert_eq!(fs::read(outside.path()).unwrap(), b"outside");
    symlink(outside.path().parent().unwrap(), root.join("dirlink")).unwrap();
    assert!(access.write("dirlink/new/file", b"bad").is_err());
}

#[test]
fn rename_replaces_the_inode_and_preserves_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = tempfile::tempdir().unwrap();
    let access = FileAccess::new(&fs::canonicalize(root.path()).unwrap()).unwrap();
    access.write("file", b"old").unwrap();
    fs::set_permissions(root.path().join("file"), fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::metadata(root.path().join("file")).unwrap().ino();
    access.write("file", b"new").unwrap();
    let after = fs::metadata(root.path().join("file")).unwrap();
    assert_ne!(before, after.ino());
    assert_eq!(after.mode() & 0o777, 0o755);
    assert_eq!(access.read_to_string("file").unwrap(), "new");
}

#[test]
fn concurrent_link_swaps_never_touch_the_outside_file() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let root = tempfile::tempdir().unwrap();
    let canonical = fs::canonicalize(root.path()).unwrap();
    let access = FileAccess::new(&canonical).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("file"), "outside-secret").unwrap();
    access.write("swapped/file", b"inside").unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stopped);
    let outside_path = outside.path().to_owned();
    let plain = canonical.join("swapped/file");
    let swapper = std::thread::spawn(move || {
        while !signal.load(Ordering::Relaxed) {
            let (swapped, parked) = (canonical.join("swapped"), canonical.join("parked"));
            let _ = fs::rename(&swapped, &parked);
            let _ = symlink(&outside_path, &swapped);
            std::thread::sleep(std::time::Duration::from_micros(100));
            let _ = fs::remove_file(&swapped);
            // A write in the gap can create a fresh directory here, which blocks the return.
            if fs::rename(&parked, &swapped).is_err() {
                let _ = fs::remove_dir_all(&swapped);
                let _ = fs::rename(&parked, &swapped);
            }
        }
    });
    // A plain read follows the link, so it proves the swap happened while FileAccess ran.
    let mut raced = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut rounds = 0;
    while rounds < 300 || (!raced && std::time::Instant::now() < deadline) {
        rounds += 1;
        raced |= fs::read(&plain).is_ok_and(|bytes| bytes == b"outside-secret");
        if let Ok(bytes) = access.read("swapped/file") {
            assert_eq!(bytes, b"inside");
        }
        let _ = access.write("swapped/file", b"inside");
    }
    stopped.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
    assert!(raced, "a plain read never followed the swapped link");
    assert_eq!(
        fs::read(outside.path().join("file")).unwrap(),
        b"outside-secret"
    );
}

#[test]
fn target_hard_links_are_replaced_without_modifying_the_other_name() {
    let root = tempfile::tempdir().unwrap();
    let access = FileAccess::new(&fs::canonicalize(root.path()).unwrap()).unwrap();
    access.write("target/file", b"old").unwrap();
    fs::hard_link(root.path().join("target/file"), root.path().join("other")).unwrap();
    assert_eq!(access.read("target/file").unwrap(), b"old");
    access.write("target/file", b"new").unwrap();
    assert_eq!(fs::read(root.path().join("other")).unwrap(), b"old");
}
