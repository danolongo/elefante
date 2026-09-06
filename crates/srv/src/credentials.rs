use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket/credential parent must be a real private directory",
        ));
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket/credential parent must have permissions 0700",
        ));
    }
    Ok(())
}

pub(crate) fn read(path: &Path) -> anyhow::Result<Option<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for line in contents.lines() {
        if let Some(value) = line
            .trim()
            .strip_prefix("api_key")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        {
            let key: String = serde_json::from_str(value.trim())?;
            return Ok((!key.trim().is_empty()).then_some(key));
        }
    }
    Ok(None)
}

pub(crate) fn write(path: &Path, key: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("credential path has no parent"))?;
    private_directory(parent)?;
    let temp = parent.join(format!(
        ".credentials-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        writeln!(file, "api_key = {}", serde_json::to_string(key)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_is_private_on_first_creation_and_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".elefante/credentials.toml");
        write(&path, "first").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write(&path, "second").unwrap();
        assert_eq!(read(&path).unwrap().as_deref(), Some("second"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
