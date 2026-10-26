//! The host Git exception: preflight every approved literal write before launching it.
use elefante_guard::git::{self, GitWrite, Identity};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct HarnessGit {
    root: PathBuf,
    git: PathBuf,
    identity: Identity,
    repository: Option<(PathBuf, PathBuf)>,
}

impl HarnessGit {
    /// Capture repository identity before any agent command can create a repository.
    ///
    /// # Errors
    /// Returns an error for invalid roots, Git executables, or repository metadata.
    pub fn new(root: &Path, git: &Path, identity: Identity) -> Result<Self, String> {
        if !git.is_absolute() {
            return Err("Git executable must be absolute".into());
        }
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let repository = repository(&root)?;
        Ok(Self {
            root,
            git: git.into(),
            identity,
            repository,
        })
    }

    /// Caller holds the session's exclusive root lock throughout preflight and execution.
    ///
    /// # Errors
    /// Refuses unsafe repositories and returns process failures, timeouts or output errors.
    pub async fn execute(&self, write: &GitWrite) -> Result<String, String> {
        self.preflight().await?;
        let invocation = git::invocation(&self.git, &self.root, write, &self.identity);
        let mut command = Command::new(invocation.program);
        command
            .args(invocation.args)
            .current_dir(invocation.cwd)
            .env_clear()
            .envs(invocation.environment);
        let output = Box::pin(crate::sandbox::run_process(
            command,
            crate::sandbox::command_timeout(),
        ))
        .await?;
        if output.success {
            Ok(output.content)
        } else {
            Err(output.content)
        }
    }

    async fn preflight(&self) -> Result<(), String> {
        let Some((worktree, gitdir)) = &self.repository else {
            return Err("harness git requires a repository present at session start".into());
        };
        if repository(&self.root)?.as_ref() != self.repository.as_ref() {
            return Err("repository identity changed since session start".into());
        }
        let common = if gitdir.join("commondir").exists() {
            resolve(
                gitdir,
                fs::read_to_string(gitdir.join("commondir"))
                    .map_err(|e| e.to_string())?
                    .trim(),
            )
        } else {
            gitdir.clone()
        };
        let mut pending = vec![common.join("config"), gitdir.join("config.worktree")];
        let mut visited = BTreeSet::new();
        while let Some(config) = pending.pop() {
            if !config.exists() {
                continue;
            }
            let config = config.canonicalize().map_err(|e| e.to_string())?;
            if !visited.insert(config.clone()) {
                continue;
            }
            if visited.len() > 32 {
                return Err("too many Git config includes".into());
            }
            let path = config.to_str().ok_or("non-UTF-8 Git config path")?;
            let content = self
                .read(&[
                    "config",
                    "--no-includes",
                    "--null",
                    "--file",
                    path,
                    "--list",
                ])
                .await?;
            for entry in content.split('\0').filter(|s| !s.is_empty()) {
                let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
                let key = key.to_ascii_lowercase();
                if key.starts_with("filter.")
                    || (key.starts_with("merge.") && key.ends_with(".driver"))
                {
                    return Err("harness git refuses configured custom Git drivers".into());
                }
                let include = key == "include.path"
                    || (key.starts_with("includeif.") && key.rsplit('.').next() == Some("path"));
                if include || key == "core.hookspath" {
                    // Unknown home expansion cannot be proven outside the writable root.
                    if value.starts_with('~') {
                        return Err("harness git refuses home-expanded config targets".into());
                    }
                    let target = resolve(
                        if include {
                            config.parent().unwrap_or(gitdir)
                        } else {
                            worktree
                        },
                        value,
                    );
                    let target = target.canonicalize().unwrap_or(target);
                    if target.starts_with(&self.root) {
                        return Err(
                            "harness git refuses in-root include or hooksPath targets".into()
                        );
                    }
                    if include {
                        pending.push(target);
                    }
                }
            }
        }
        let mut attributes = BTreeSet::new();
        collect_attributes(worktree, &mut attributes)?;
        for path in [
            gitdir.join("info/attributes"),
            common.join("info/attributes"),
        ] {
            if path.exists() {
                check_file(&path)?;
            }
        }
        for path in &attributes {
            check_file(path)?;
        }
        let indexed = self.read(&["ls-files", "--cached", "-z"]).await?;
        for path in indexed.split('\0').filter(|s| !s.is_empty()) {
            if Path::new(path)
                .file_name()
                .is_some_and(|name| name == ".gitattributes")
            {
                let contents = self.read(&["show", &format!(":{path}")]).await?;
                check_attributes(&contents)?;
            }
        }
        Ok(())
    }

    async fn read(&self, args: &[&str]) -> Result<String, String> {
        let invocation = git::hardened(&self.git, &self.root);
        let mut command = Command::new(invocation.program);
        command
            .args(invocation.args)
            .args(args)
            .current_dir(
                self.repository
                    .as_ref()
                    .map_or(invocation.cwd.as_path(), |(root, _)| root.as_path()),
            )
            .env_clear()
            .envs(invocation.environment)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().ok_or("missing Git preflight stdout")?;
        let operation = async {
            let mut bytes = Vec::new();
            stdout
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| e.to_string())?;
            if bytes.len() > 1024 * 1024 {
                return Err("Git preflight output exceeded its limit".into());
            }
            if !child.wait().await.map_err(|e| e.to_string())?.success() {
                return Err("Git repository preflight failed".into());
            }
            String::from_utf8(bytes).map_err(|_| "non-UTF-8 Git preflight output".into())
        };
        tokio::time::timeout(Duration::from_secs(30), operation)
            .await
            .map_err(|_| "Git preflight timed out".to_owned())?
    }
}

fn repository(root: &Path) -> Result<Option<(PathBuf, PathBuf)>, String> {
    for ancestor in root.ancestors() {
        let entry = ancestor.join(".git");
        if !entry.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(&entry).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("symlinked .git is refused".into());
        }
        let directory = if metadata.is_dir() {
            entry
        } else {
            let content = fs::read_to_string(&entry).map_err(|e| e.to_string())?;
            resolve(
                ancestor,
                content
                    .trim()
                    .strip_prefix("gitdir: ")
                    .ok_or("invalid gitdir file")?,
            )
        };
        return Ok(Some((
            ancestor.into(),
            directory.canonicalize().map_err(|e| e.to_string())?,
        )));
    }
    Ok(None)
}

fn resolve(base: &Path, target: &str) -> PathBuf {
    let mut result = PathBuf::new();
    for part in base.join(target).components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn collect_attributes(directory: &Path, found: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if entry.file_name() == ".gitattributes" {
            found.insert(entry.path());
        }
        if kind.is_dir()
            && !entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git")
        {
            collect_attributes(&entry.path(), found)?;
        }
    }
    Ok(())
}

fn check_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        return Err("unsafe or oversized Git attributes file".into());
    }
    check_attributes(&fs::read_to_string(path).map_err(|e| e.to_string())?)
}

fn check_attributes(contents: &str) -> Result<(), String> {
    for line in contents
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
    {
        // Also inspect macro definitions; Git may activate them from another file.
        for token in line.split_whitespace() {
            if token.starts_with("filter=")
                || token
                    .strip_prefix("merge=")
                    .is_some_and(|driver| !matches!(driver, "text" | "binary" | "union"))
            {
                return Err("harness git refuses custom filter or merge attributes".into());
            }
        }
    }
    Ok(())
}
