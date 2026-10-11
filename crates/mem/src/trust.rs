//! Who may vouch for what is stored. See `docs/PLAN.md` § Memory subsystem, and this crate's README for the contract.
//!
//! A repository can ship its own `.elefante/`, and a branch checkout can replace a database under
//! a running daemon. Text read back from either would reach the prompt at `user` authority, so
//! every store and every learned row carries a keyed `blake3` MAC made with a key that lives
//! outside any working tree. A row or store whose MAC does not verify is treated as absent.
//!
//! This is integrity and origin, not truth: it says *this harness wrote these bytes*, not that the
//! sentence is accurate. It does not stop a process that can read the key (an unrestricted shell
//! running as the same user); `docs/PLAN.md` states that the bash classifier is not a sandbox.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{error::MemError, layout::create_private_dir};

/// File name of the key beneath `~/.elefante/`.
pub const KEY_FILE: &str = "memory.key";
const KEY_BYTES: usize = 32;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

/// The fields a learned row's MAC covers. Everything that can change what the model reads is here;
/// `status`, `confidence` and `last_reinforced_ns` are deliberately not, so reinforcing a note does
/// not require re-signing it and flipping them cannot introduce new text.
#[derive(Clone, Copy, Debug)]
pub struct RowFields<'a> {
    pub kind: &'a str,
    pub key: &'a str,
    pub name: &'a str,
    pub body: &'a str,
    pub source: &'a str,
    pub created_at_ns: i64,
}

/// A 32-byte secret used only to authenticate what this harness wrote.
#[derive(Clone)]
pub struct MemKey([u8; KEY_BYTES]);

impl fmt::Debug for MemKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MemKey(..)")
    }
}

fn absorb(hasher: &mut blake3::Hasher, field: &[u8]) {
    hasher.update(&(field.len() as u64).to_le_bytes());
    hasher.update(field);
}

impl MemKey {
    /// A key from fixed bytes. For tests; a real key comes from [`MemKey::load_or_create`].
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    /// `~/.elefante/memory.key`, or `None` when `HOME` is unset.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".elefante").join(KEY_FILE))
    }

    /// Reads the key at `path`, creating it first if it does not exist.
    ///
    /// Creation is race-free: the key is written to a private temporary file and hard-linked into
    /// place, which fails if another process got there first, in which case that key is read. The
    /// file is `0600` and a key readable by anyone else is refused rather than trusted.
    ///
    /// # Errors
    /// Fails on I/O errors, a file that is not exactly 32 bytes, or a key with loose permissions.
    pub fn load_or_create(path: &Path) -> Result<Self, MemError> {
        if !path.exists() {
            create_key(path)?;
        }
        read_key(path)
    }

    /// The stamp that marks a database as initialised by a harness holding this key.
    /// `purpose` separates the graph from the cache, and `binding` ties a stamp to one database.
    #[must_use]
    pub fn stamp(&self, purpose: &str, binding: &str) -> String {
        self.stamp_hash(purpose, binding).to_hex().to_string()
    }

    fn stamp_hash(&self, purpose: &str, binding: &str) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        absorb(&mut hasher, b"elefante.store.v1");
        absorb(&mut hasher, purpose.as_bytes());
        absorb(&mut hasher, binding.as_bytes());
        hasher.finalize()
    }

    /// Whether `claimed` is the stamp for `(purpose, binding)`. Constant-time on the digest.
    #[must_use]
    pub fn stamp_matches(&self, purpose: &str, binding: &str, claimed: &str) -> bool {
        let Ok(claimed) = blake3::Hash::from_hex(claimed) else {
            return false;
        };
        claimed == self.stamp_hash(purpose, binding)
    }

    /// The MAC of a learned row.
    #[must_use]
    pub fn row_mac(&self, fields: &RowFields<'_>) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_keyed(&self.0);
        absorb(&mut hasher, b"elefante.memory.v1");
        for field in [
            fields.kind,
            fields.key,
            fields.name,
            fields.body,
            fields.source,
        ] {
            absorb(&mut hasher, field.as_bytes());
        }
        hasher.update(&fields.created_at_ns.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Whether `claimed` is the MAC of `fields`.
    #[must_use]
    pub fn row_mac_matches(&self, fields: &RowFields<'_>, claimed: &[u8]) -> bool {
        let Ok(claimed) = <[u8; 32]>::try_from(claimed) else {
            return false;
        };
        blake3::Hash::from_bytes(claimed) == blake3::Hash::from_bytes(self.row_mac(fields))
    }
}

fn create_key(path: &Path) -> Result<(), MemError> {
    let parent = path.parent().ok_or_else(|| {
        MemError::io(
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "key path has no parent"),
        )
    })?;
    create_private_dir(parent)?;
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| MemError::io(path, io::Error::other(e.to_string())))?;

    let temporary = parent.join(format!(
        ".{KEY_FILE}.{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = private_new(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temporary);
        return Err(MemError::io(&temporary, e));
    }
    // Linking fails if the key already exists, so exactly one writer wins and the rest read it.
    let linked = fs::hard_link(&temporary, path);
    let _ = fs::remove_file(&temporary);
    match linked {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(MemError::io(path, e)),
    }
}

#[cfg(unix)]
fn private_new(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_new(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn read_key(path: &Path) -> Result<MemKey, MemError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map_err(|e| MemError::io(path, e))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(MemError::Refused {
                path: path.to_owned(),
                reason: "the memory key must not be readable by anyone else (expected mode 0600)",
            });
        }
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| MemError::io(path, e))?;
    let key = <[u8; KEY_BYTES]>::try_from(bytes.as_slice()).map_err(|_| MemError::Corrupt {
        path: path.to_owned(),
        reason: format!("memory key is {} bytes, expected {KEY_BYTES}", bytes.len()),
    })?;
    Ok(MemKey(key))
}
