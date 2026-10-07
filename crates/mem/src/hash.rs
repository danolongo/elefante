//! Reading and hashing file content.

use std::{fs, io, path::Path};

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

/// Reads `path` in full and hashes it.
///
/// # Errors
/// Any I/O error from reading the file.
pub fn read_and_hash(path: &Path) -> io::Result<Hashed> {
    let mut bytes = fs::read(path)?;
    let blake3 = *blake3::hash(&bytes).as_bytes();
    let binary = bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0);
    if binary {
        bytes = Vec::new();
    }
    Ok(Hashed {
        blake3,
        binary,
        bytes,
    })
}

/// Hashes `files` (paths relative to `root`) on a bounded number of threads. Results are in
/// input order, and a file that cannot be read yields its error rather than aborting the batch.
pub fn hash_files(
    root: &Path,
    files: &[WalkedFile],
    counters: &Counters,
) -> Vec<io::Result<Hashed>> {
    par_map(files, |file| {
        let result = read_and_hash(&root.join(&file.path));
        if result.is_ok() {
            counters.add_hashed(1);
        }
        result
    })
}
