//! The repository walk. Design: `docs/M2-DESIGN.md` § 6.

use std::{fs, path::Path, time::UNIX_EPOCH};

use elefante_guard::PathPolicy;
use ignore::WalkBuilder;

use crate::{error::MemError, layout::DIR_NAME, stats::Counters};

/// Files above this size are generated bundles and lockfiles; they would dominate the graph.
pub const MAX_FILE_BYTES: u64 = 1 << 20;
/// Hard bound on the number of files one repository contributes.
pub const MAX_FILES: usize = 200_000;

/// A candidate file, identified by what a stat call alone can tell us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkedFile {
    /// Repository-relative, `/`-separated, valid UTF-8.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug)]
pub struct Walk {
    /// Sorted by path.
    pub files: Vec<WalkedFile>,
    /// The file limit was reached; later files were not examined.
    pub truncated: bool,
    /// Every entry was examined without error and nothing was cut off. Only a complete walk
    /// may be used to conclude that an unseen file was deleted.
    pub complete: bool,
}

/// Walks `root` with the production file limit.
///
/// # Errors
/// Fails when `root` itself cannot be read.
pub fn walk(root: &Path, counters: &Counters) -> Result<Walk, MemError> {
    walk_limited(root, MAX_FILES, counters)
}

/// Walks `root`, stopping after `limit` files.
///
/// Honours `.gitignore`, `.ignore` and `.git/info/exclude` beneath `root`, deliberately **not**
/// the user's global excludes or any parent directory's ignore files: those are machine state,
/// and an index that differs between machines for reasons outside the repository is a test that
/// passes locally and fails in CI. Symlinks are skipped, never followed. Secret paths, `.git`
/// and `.elefante` are pruned without being read.
///
/// # Errors
/// Fails when `root` itself cannot be read.
pub fn walk_limited(root: &Path, limit: usize, counters: &Counters) -> Result<Walk, MemError> {
    let root_metadata = fs::metadata(root).map_err(|e| MemError::io(root, e))?;
    if !root_metadata.is_dir() {
        return Err(MemError::io(
            root,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a directory"),
        ));
    }
    let prune_root = root.to_path_buf();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .parents(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        .require_git(false)
        .sort_by_file_path(Ord::cmp)
        .filter_entry(move |entry| {
            let name = entry.file_name();
            if name == ".git" || name == DIR_NAME {
                return false;
            }
            let relative = entry
                .path()
                .strip_prefix(&prune_root)
                .unwrap_or(entry.path());
            !PathPolicy::is_secret_path(relative)
        });

    let mut files = Vec::new();
    let mut complete = true;
    let mut truncated = false;
    for entry in builder.build() {
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_file() {
            continue;
        }
        let Some(relative) = entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.to_str())
        else {
            continue;
        };
        counters.add_walked(1);
        let Ok(metadata) = entry.metadata() else {
            // Vanished between listing and stat: benign, and the next refresh sees the truth.
            continue;
        };
        if metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        if files.len() >= limit {
            truncated = true;
            break;
        }
        files.push(WalkedFile {
            path: relative.replace(std::path::MAIN_SEPARATOR, "/"),
            size: metadata.len(),
            mtime_ns: mtime_ns(&metadata),
        });
    }
    Ok(Walk {
        files,
        truncated,
        complete: complete && !truncated,
    })
}

fn mtime_ns(metadata: &fs::Metadata) -> i64 {
    match metadata.modified().map(|t| t.duration_since(UNIX_EPOCH)) {
        Ok(Ok(after)) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Ok(Err(before)) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}
