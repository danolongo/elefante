//! Descriptor-relative file access after path-policy resolution.

use elefante_mem::hash::{check_regular, open_beneath, open_root};
use rustix::fs::{AtFlags, Mode, OFlags};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Pins the repository directory so a path replacement cannot redirect an operation.
#[derive(Debug)]
pub struct FileAccess {
    root: PathBuf,
    descriptor: File,
}

impl FileAccess {
    /// `root` must already be canonical (as supplied by `PathPolicy`).
    ///
    /// # Errors
    /// Returns an error if the root cannot be opened without following symlinks.
    pub fn new(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root: root.to_owned(),
            descriptor: open_root(root)?,
        })
    }

    fn relative<'a>(&self, path: &'a Path) -> io::Result<&'a Path> {
        let path = if path.is_absolute() {
            path.strip_prefix(&self.root).map_err(io::Error::other)?
        } else {
            path
        };
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path is not beneath root",
            ));
        }
        Ok(path)
    }

    fn open_file(&self, path: &Path) -> io::Result<File> {
        let path = self.relative(path)?;
        let file = open_beneath(
            &self.descriptor,
            path,
            OFlags::RDONLY | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        check_regular(&file, path.starts_with("target"))?;
        Ok(file)
    }

    /// Checks that a path opens as an allowed regular file.
    ///
    /// # Errors
    /// Returns an error for traversal, links, or an inaccessible file.
    pub fn check_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        self.open_file(path.as_ref()).map(|_| ())
    }

    /// Reads an allowed regular file from a pinned descriptor.
    ///
    /// # Errors
    /// Returns an error for a refused open or a failed read.
    pub fn read(&self, path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let mut file = self.open_file(path.as_ref())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Reads an allowed UTF-8 file.
    ///
    /// # Errors
    /// Returns an error for a refused read or invalid UTF-8.
    pub fn read_to_string(&self, path: impl AsRef<Path>) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Creates parents through pinned descriptors and replaces the destination by rename.
    ///
    /// # Errors
    /// Returns an error for links, traversal, or failed directory, write, or rename operations.
    pub fn write(&self, path: impl AsRef<Path>, bytes: &[u8]) -> io::Result<()> {
        let relative = self.relative(path.as_ref())?;
        let name = relative
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
        let mut parent = self.descriptor.try_clone()?;
        for part in relative.parent().unwrap_or(Path::new("")).components() {
            if let Component::Normal(name) = part {
                match rustix::fs::mkdirat(&parent, name, Mode::from_bits_truncate(0o755)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                parent = open_beneath(
                    &parent,
                    Path::new(name),
                    OFlags::RDONLY | OFlags::DIRECTORY,
                    Mode::empty(),
                )?;
            }
        }
        let mode = match open_beneath(
            &parent,
            Path::new(name),
            OFlags::RDONLY | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(file) => {
                check_regular(&file, relative.starts_with("target"))?;
                Mode::from_bits_truncate((file.metadata()?.mode() & 0o777) as _)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Mode::from_bits_truncate(0o644)
            }
            Err(error) => return Err(error),
        };
        let (temp, mut file) = loop {
            let temp = format!(
                ".elefante-write-{}-{}",
                std::process::id(),
                WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            match open_beneath(
                &parent,
                Path::new(&temp),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_bits_truncate(0o600),
            ) {
                Ok(file) => break (temp, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        };
        let result = (|| {
            file.write_all(bytes)?;
            rustix::fs::fchmod(&file, mode)?;
            file.sync_all()?;
            rustix::fs::renameat(&parent, &temp, &parent, name)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&parent, &temp, AtFlags::empty());
        }
        result
    }
}
