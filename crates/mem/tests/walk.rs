use std::{fs, path::Path};

use elefante_mem::{
    Counters,
    walk::{MAX_FILE_BYTES, Walk, walk, walk_limited},
};

fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let target = tmp.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, contents).unwrap();
    }
    tmp
}

fn paths(walk: &Walk) -> Vec<&str> {
    walk.files.iter().map(|f| f.path.as_str()).collect()
}

fn run(root: &Path) -> Walk {
    walk(root, &Counters::default()).unwrap()
}

#[test]
fn includes_ordinary_and_hidden_source_and_sorts_by_path() {
    let tmp = tree(&[
        ("src/lib.rs", "fn a() {}"),
        ("README.md", "# hi"),
        (".github/workflows/ci.yml", "on: push"),
        ("src/a/mod.rs", ""),
    ]);
    let w = run(tmp.path());
    assert_eq!(
        paths(&w),
        [
            ".github/workflows/ci.yml",
            "README.md",
            "src/a/mod.rs",
            "src/lib.rs"
        ]
    );
    assert!(w.complete && !w.truncated);
}

#[test]
fn prunes_git_and_our_own_directory_at_any_depth() {
    let tmp = tree(&[
        ("a.rs", ""),
        (".git/config", "x"),
        (".elefante/memory.db", "x"),
        ("vendor/sub/.git/HEAD", "x"),
        ("vendor/sub/.elefante/cache.db", "x"),
        ("vendor/sub/b.rs", ""),
    ]);
    assert_eq!(paths(&run(tmp.path())), ["a.rs", "vendor/sub/b.rs"]);
}

#[test]
fn honours_gitignore_without_needing_a_git_repository() {
    let tmp = tree(&[
        (".gitignore", "target/\n*.log\n"),
        ("src/a.rs", ""),
        ("target/debug/a.o", ""),
        ("build.log", ""),
        ("src/nested/.gitignore", "gen.rs\n"),
        ("src/nested/gen.rs", ""),
        ("src/nested/keep.rs", ""),
    ]);
    assert_eq!(
        paths(&run(tmp.path())),
        [
            ".gitignore",
            "src/a.rs",
            "src/nested/.gitignore",
            "src/nested/keep.rs"
        ]
    );
}

#[test]
fn secret_paths_are_pruned_without_being_listed_at_all() {
    let tmp = tree(&[
        ("ok.rs", ""),
        (".env", "SECRET=1"),
        (".env.local", "SECRET=1"),
        ("config/.env.production", "SECRET=1"),
        ("certs/server.pem", "-----BEGIN"),
        ("keys/signing.key", "x"),
        ("id_rsa", "x"),
        ("id_rsa.pub", "x"),
        (".ssh/config", "x"),
        (".aws/credentials", "x"),
        (".netrc", "x"),
        (".npmrc", "x"),
    ]);
    assert_eq!(paths(&run(tmp.path())), ["ok.rs"]);
}

#[cfg(unix)]
#[test]
fn symlinks_are_skipped_and_never_followed() {
    use std::os::unix::fs::symlink;
    let tmp = tree(&[("real/a.rs", ""), ("top.rs", "")]);
    symlink(tmp.path().join("real/a.rs"), tmp.path().join("link.rs")).unwrap();
    symlink(tmp.path().join("real"), tmp.path().join("linkdir")).unwrap();
    // A link out of the tree, and a link that loops.
    symlink("/etc/hosts", tmp.path().join("escape")).unwrap();
    symlink(tmp.path(), tmp.path().join("real/loop")).unwrap();
    assert_eq!(paths(&run(tmp.path())), ["real/a.rs", "top.rs"]);
}

#[test]
fn oversize_files_are_skipped_at_the_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let at_limit = usize::try_from(MAX_FILE_BYTES).unwrap();
    fs::write(tmp.path().join("exact.txt"), vec![b'a'; at_limit]).unwrap();
    fs::write(tmp.path().join("over.txt"), vec![b'a'; at_limit + 1]).unwrap();
    assert_eq!(paths(&run(tmp.path())), ["exact.txt"]);
}

// macOS filesystems reject non-UTF-8 names outright, so this can only be constructed on Linux.
#[cfg(target_os = "linux")]
#[test]
fn paths_that_are_not_utf8_are_skipped_not_lossily_converted() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let tmp = tree(&[("good.rs", "")]);
    fs::write(tmp.path().join(OsStr::from_bytes(b"bad\xff.rs")), "").unwrap();
    assert_eq!(paths(&run(tmp.path())), ["good.rs"]);
}

#[test]
fn the_file_limit_truncates_deterministically_and_marks_the_walk_incomplete() {
    let tmp = tree(&[
        ("a.rs", ""),
        ("b.rs", ""),
        ("c.rs", ""),
        ("d.rs", ""),
        ("e.rs", ""),
    ]);
    let w = walk_limited(tmp.path(), 3, &Counters::default()).unwrap();
    assert_eq!(paths(&w), ["a.rs", "b.rs", "c.rs"]);
    assert!(w.truncated);
    assert!(
        !w.complete,
        "a truncated walk must never be used to infer deletions"
    );
    // Exactly at the limit is not truncation.
    let exact = walk_limited(tmp.path(), 5, &Counters::default()).unwrap();
    assert_eq!(exact.files.len(), 5);
    assert!(!exact.truncated && exact.complete);
}

#[test]
fn counts_the_files_it_examines() {
    let tmp = tree(&[("a.rs", ""), ("b.rs", ""), (".env", "x")]);
    let counters = Counters::default();
    walk(tmp.path(), &counters).unwrap();
    // `.env` is pruned before it is examined, so it is not counted.
    assert_eq!(counters.snapshot().walked, 2);
}

#[test]
fn records_size_and_a_nonzero_mtime() {
    let tmp = tree(&[("a.rs", "12345")]);
    let w = run(tmp.path());
    assert_eq!(w.files[0].size, 5);
    assert!(w.files[0].mtime_ns > 0);
}

#[test]
fn a_missing_or_non_directory_root_is_an_error() {
    let tmp = tree(&[("f", "")]);
    assert!(walk(&tmp.path().join("nope"), &Counters::default()).is_err());
    assert!(walk(&tmp.path().join("f"), &Counters::default()).is_err());
}
