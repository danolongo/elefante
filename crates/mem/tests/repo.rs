use std::{fs, path::Path};

use elefante_mem::{MemError, RepoId};

fn canonical(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap()
}

#[test]
fn git_directory_marks_the_root_from_any_depth() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("a/.git")).unwrap();
    fs::create_dir_all(tmp.path().join("a/src/deep")).unwrap();
    let repo = RepoId::resolve_with_home(&tmp.path().join("a/src/deep"), None).unwrap();
    assert_eq!(repo.root(), canonical(&tmp.path().join("a")));
    assert_eq!(
        repo.scope_of(&tmp.path().join("a/src/deep")).as_deref(),
        Some("src/deep")
    );
    assert_eq!(repo.scope_of(&tmp.path().join("a")), None);
}

#[test]
fn a_git_file_marks_the_root_for_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("wt/sub")).unwrap();
    fs::write(tmp.path().join("wt/.git"), "gitdir: /elsewhere\n").unwrap();
    let repo = RepoId::resolve_with_home(&tmp.path().join("wt/sub"), None).unwrap();
    assert_eq!(repo.root(), canonical(&tmp.path().join("wt")));
}

#[test]
fn the_nearest_git_entry_wins() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("outer/.git")).unwrap();
    fs::create_dir_all(tmp.path().join("outer/inner/.git")).unwrap();
    let repo = RepoId::resolve_with_home(&tmp.path().join("outer/inner"), None).unwrap();
    assert_eq!(repo.root(), canonical(&tmp.path().join("outer/inner")));
}

#[test]
fn without_git_the_working_directory_is_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("plain/x")).unwrap();
    let repo = RepoId::resolve_with_home(&tmp.path().join("plain/x"), None).unwrap();
    assert_eq!(repo.root(), canonical(&tmp.path().join("plain/x")));
}

#[test]
fn the_home_directory_is_refused_even_when_it_is_a_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".git")).unwrap();
    fs::create_dir_all(home.join("project/.git")).unwrap();
    fs::create_dir_all(home.join("scratch")).unwrap();

    assert!(matches!(
        RepoId::resolve_with_home(&home, Some(&home)),
        Err(MemError::Refused { .. })
    ));
    // A directory under home that is not itself a repository resolves to the enclosing home
    // repository's root, which is home, and so is refused too: the dotfiles-repo case.
    assert!(matches!(
        RepoId::resolve_with_home(&home.join("scratch"), Some(&home)),
        Err(MemError::Refused { .. })
    ));
    // A real project beneath home is fine.
    let project = RepoId::resolve_with_home(&home.join("project"), Some(&home)).unwrap();
    assert_eq!(project.root(), canonical(&home.join("project")));
}

#[test]
fn the_filesystem_root_is_refused() {
    assert!(matches!(
        RepoId::resolve_with_home(Path::new("/"), None),
        Err(MemError::Refused { .. })
    ));
}

#[test]
fn a_missing_directory_is_an_io_error_not_a_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        RepoId::resolve_with_home(&tmp.path().join("nope"), None),
        Err(MemError::Io { .. })
    ));
}
