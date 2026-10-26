//! Reading and hashing file content.

use rustix::fs::{Mode, OFlags};
use std::{
    fs::File,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

/// Opens a trusted repository root without following symlinks in its spelling.
///
/// # Errors
/// Returns the operating system error when the directory cannot be opened safely.
pub fn open_root(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    let fd = rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS,
    )?;
    #[cfg(target_os = "macos")]
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW_ANY,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// Opens a relative path beneath an already opened directory. Parent and absolute paths fail.
///
/// # Errors
/// Returns a permission error for traversal or an operating system error for a refused open.
pub fn open_beneath(root: &File, path: &Path, flags: OFlags, mode: Mode) -> io::Result<File> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path is not beneath root",
        ));
    }
    #[cfg(target_os = "linux")]
    let fd = rustix::fs::openat2(
        root,
        path,
        flags | OFlags::CLOEXEC,
        mode,
        rustix::fs::ResolveFlags::BENEATH | rustix::fs::ResolveFlags::NO_SYMLINKS,
    )?;
    #[cfg(target_os = "macos")]
    let fd = rustix::fs::openat(
        root,
        path,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW_ANY,
        mode,
    )?;
    Ok(File::from(fd))
}

/// Checks metadata on the opened descriptor, never on a separately resolved path.
///
/// # Errors
/// Returns an error for nonregular files, forbidden hard links, or unavailable metadata.
pub fn check_regular(file: &File, allow_hard_links: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || (!allow_hard_links && metadata.nlink() > 1) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file is not regular or has multiple hard links",
        ));
    }
    Ok(())
}

fn read_relative(root: &File, path: &Path) -> io::Result<Hashed> {
    let mut file = open_beneath(root, path, OFlags::RDONLY | OFlags::NONBLOCK, Mode::empty())?;
    check_regular(&file, path.starts_with("target"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(hash_bytes(bytes))
}

use crate::{stats::Counters, util::par_map, walk::WalkedFile};

/// How much of a file is inspected for a NUL byte to call it binary.
pub const BINARY_SNIFF_BYTES: usize = 8192;

#[derive(Debug)]
pub struct Hashed {
    pub blake3: [u8; 32],
    /// A NUL byte appears in the first [`BINARY_SNIFF_BYTES`]. Such a file is hashed once, so
    /// its verdict can be remembered, and never indexed.
    pub binary: bool,
    /// The content, retained so a parse does not read the file a second time. Empty for binaries.
    pub bytes: Vec<u8>,
}

fn hash_bytes(mut bytes: Vec<u8>) -> Hashed {
    let blake3 = *blake3::hash(&bytes).as_bytes();
    let binary = bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0);
    if binary {
        bytes = Vec::new();
    }
    Hashed {
        blake3,
        binary,
        bytes,
    }
}

/// Hashes `files` (paths relative to `root`) on a bounded number of threads. Results are in
/// input order, and a file that cannot be read yields its error rather than aborting the batch.
pub fn hash_files(
    root: &Path,
    files: &[WalkedFile],
    counters: &Counters,
) -> Vec<io::Result<Hashed>> {
    let descriptor = root.canonicalize().and_then(|root| open_root(&root));
    par_map(files, |file| {
        let result = match &descriptor {
            Ok(root) => read_relative(root, Path::new(&file.path)),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        };
        if result.is_ok() {
            counters.add_hashed(1);
        }
        result
    })
}
