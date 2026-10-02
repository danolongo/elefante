//! Repository identity: which directory owns the index for a session started at `cwd`.

use std::path::{Path, PathBuf};

use crate::error::MemError;

/// The directory that owns one repository's index.
///
/// This is the nearest ancestor of the session's working directory that contains a `.git`
/// entry, or the working directory itself when there is none. It decides where `.elefante/`
/// lives and what the walker covers. It is **not** the tool policy root: a session started in
/// `repo/subdir` is still confined to `subdir`, and retrieval is scoped to match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoId {
    root: PathBuf,
}

impl RepoId {
    /// Resolves the repository containing `cwd`, refusing `/` and the user's home directory.
    ///
    /// # Errors
    /// Fails when `cwd` cannot be canonicalised, or when the resolved root is `/` or `$HOME`.
    pub fn resolve(cwd: &Path) -> Result<Self, MemError> {
        Self::resolve_with_home(cwd, std::env::var_os("HOME").map(PathBuf::from).as_deref())
    }

    /// As [`RepoId::resolve`], with the home directory supplied rather than read from the
    /// environment, so tests need not mutate process state.
    ///
    /// # Errors
    /// See [`RepoId::resolve`].
    pub fn resolve_with_home(cwd: &Path, home: Option<&Path>) -> Result<Self, MemError> {
        let canonical = cwd.canonicalize().map_err(|e| MemError::io(cwd, e))?;
        let root = canonical
            .ancestors()
            .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
            .unwrap_or(&canonical)
            .to_path_buf();

        if root.parent().is_none() {
            return Err(MemError::Refused {
                path: root,
                reason: "the filesystem root is never indexed",
            });
        }
        // A home directory is walked by nothing that asked to be: a session started there, or a
        // dotfiles repository rooted there, would otherwise index every project beneath it.
        if home
            .and_then(|h| h.canonicalize().ok())
            .is_some_and(|h| h == root)
        {
            return Err(MemError::Refused {
                path: root,
                reason: "the home directory is never indexed",
            });
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `cwd` relative to the root, with `/` separators; `None` when `cwd` is the root itself.
    ///
    /// This is the retrieval scope for a session started below the root.
    #[must_use]
    pub fn scope_of(&self, cwd: &Path) -> Option<String> {
        let relative = cwd.canonicalize().ok()?;
        let relative = relative.strip_prefix(&self.root).ok()?;
        let text = relative
            .components()
            .map(|c| c.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()?
            .join("/");
        (!text.is_empty()).then_some(text)
    }
}
