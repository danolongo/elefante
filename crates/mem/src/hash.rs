//! Reading and hashing file content.

use std::{
    fs, io,
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{stats::Counters, walk::WalkedFile};

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
///
/// # Panics
/// Only if a worker thread panics, which would leave a result slot unfilled.
pub fn hash_files(
    root: &Path,
    files: &[WalkedFile],
    counters: &Counters,
) -> Vec<io::Result<Hashed>> {
    let slots: Vec<OnceLock<io::Result<Hashed>>> = files.iter().map(|_| OnceLock::new()).collect();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(8)
        .min(files.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(index) else { break };
                    let result = read_and_hash(&root.join(&file.path));
                    if result.is_ok() {
                        counters.add_hashed(1);
                    }
                    let _ = slots[index].set(result);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().expect("every slot is filled"))
        .collect()
}
