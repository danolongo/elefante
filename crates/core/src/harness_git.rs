//! The host Git exception: preflight every approved literal write before launching it.
use elefante_guard::git::{self, GitWrite, Identity};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
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
    /// The git directory `root/.git` named at session start.
    repository: Option<PathBuf>,
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
        let repository = crate::sandbox::git_directory(&root)?;
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
        let harness = self.clone();
        tokio::task::spawn_blocking(move || harness.check_disk())
            .await
            .map_err(|error| format!("harness git preflight failed: {error}"))??;
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

    /// Checks repository identity, config targets and attribute files on disk. It spawns
    /// `git config` and walks the root, so preflight runs it on a blocking thread.
    fn check_disk(&self) -> Result<(), String> {
        let Some(gitdir) = &self.repository else {
            return Err("harness git requires a repository present at session start".into());
        };
        if crate::sandbox::git_directory(&self.root)?.as_ref() != Some(gitdir) {
            return Err("repository identity changed since session start".into());
        }
        let common = crate::sandbox::common_directory(gitdir)?;
        let config = repo_config(&self.git, &self.root, gitdir, None)?;
        if config.drivers {
            return Err("harness git refuses configured custom Git drivers".into());
        }
        for target in &config.targets {
            // Unknown home expansion cannot be proven outside the writable root.
            if target.home {
                return Err("harness git refuses home-expanded config targets".into());
            }
            if target.path.starts_with(&self.root)
                || target
                    .canonical
                    .as_ref()
                    .is_some_and(|canonical| canonical.starts_with(&self.root))
            {
                return Err("harness git refuses in-root include or hooksPath targets".into());
            }
        }
        let worktree = &self.root;
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
        Ok(())
    }

    async fn read(&self, args: &[&str]) -> Result<String, String> {
        let invocation = git::hardened(&self.git, &self.root);
        let mut command = Command::new(invocation.program);
        command
            .args(invocation.args)
            .args(args)
            .current_dir(invocation.cwd)
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

/// One `include.path`, `includeIf.*.path` or `core.hooksPath` value and where it points.
pub(crate) struct ConfigTarget {
    pub key: String,
    /// The target with `..` resolved lexically and symlinks kept.
    pub path: PathBuf,
    /// The canonical target, when it exists.
    pub canonical: Option<PathBuf>,
    /// The value starts with `~`.
    pub home: bool,
}

/// What harness git and the shell scan both read from a repository's config.
#[derive(Default)]
pub(crate) struct RepoConfig {
    /// A `filter.*` or `merge.*.driver` key is set.
    pub drivers: bool,
    pub targets: Vec<ConfigTarget>,
}

/// Reads `<common>/config` and `<gitdir>/config.worktree` and follows their includes, up to 32
/// files. `worktree` anchors a relative `core.hooksPath`. Without `home`, a `~` target is recorded
/// but not followed.
///
/// # Errors
/// Fails for unreadable or oversized config files, unparsable config, `~user` targets and include
/// loops past the limit.
pub(crate) fn repo_config(
    git: &Path,
    worktree: &Path,
    gitdir: &Path,
    home: Option<&Path>,
) -> Result<RepoConfig, String> {
    let common = crate::sandbox::common_directory(gitdir)?;
    let mut pending = vec![common.join("config"), gitdir.join("config.worktree")];
    let mut visited = BTreeSet::new();
    let mut config = RepoConfig::default();
    while let Some(path) = pending.pop() {
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if !visited.insert(path.clone()) {
            continue;
        }
        if visited.len() > 32 {
            return Err("too many Git config includes".into());
        }
        let listing = list_config(git, worktree, &read_config(&path)?)?;
        for entry in listing.split('\0').filter(|s| !s.is_empty()) {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            let key = key.to_ascii_lowercase();
            if key.starts_with("filter.") || (key.starts_with("merge.") && key.ends_with(".driver"))
            {
                config.drivers = true;
            }
            let include = key == "include.path"
                || (key.starts_with("includeif.") && key.rsplit('.').next() == Some("path"));
            if !include && key != "core.hookspath" {
                continue;
            }
            let base = if include {
                path.parent().unwrap_or(gitdir)
            } else {
                worktree
            };
            let target = match value.strip_prefix('~') {
                None => resolve(base, value),
                Some(rest) => {
                    let rest = match rest.strip_prefix('/') {
                        Some(rest) => rest,
                        None if rest.is_empty() => rest,
                        None => {
                            return Err(format!(
                                "unsupported home-expanded Git config target {value}"
                            ));
                        }
                    };
                    let Some(home) = home else {
                        config.targets.push(ConfigTarget {
                            key,
                            path: value.into(),
                            canonical: None,
                            home: true,
                        });
                        continue;
                    };
                    resolve(home, rest)
                }
            };
            let canonical = target.canonicalize().ok();
            if include {
                pending.push(canonical.clone().unwrap_or_else(|| target.clone()));
            }
            config.targets.push(ConfigTarget {
                key,
                path: target,
                canonical,
                home: value.starts_with('~'),
            });
        }
    }
    Ok(config)
}

/// Reads a config file without blocking on a FIFO or following a swapped-in device.
fn read_config(path: &Path) -> Result<Vec<u8>, String> {
    use rustix::fs::{Mode, OFlags};
    let refused = || format!("Git config {} is not a small regular file", path.display());
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| format!("Git config {}: {error}", path.display()))?;
    let file = fs::File::from(descriptor);
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err(refused());
    }
    Ok(bytes)
}

/// Lists one config file's entries with includes off, through the hardened git invocation.
fn list_config(git: &Path, root: &Path, contents: &[u8]) -> Result<String, String> {
    let invocation = git::hardened(git, root);
    let mut child = std::process::Command::new(&invocation.program)
        .args(&invocation.args)
        .args(["config", "--no-includes", "--null", "--file", "-", "--list"])
        .current_dir(&invocation.cwd)
        .env_clear()
        .envs(&invocation.environment)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().ok_or("missing Git config stdin")?;
    // A separate writer keeps a large config from filling both pipes at once.
    let output = std::thread::scope(|scope| {
        scope.spawn(move || stdin.write_all(contents));
        child.wait_with_output()
    })
    .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("Git config could not be parsed".into());
    }
    String::from_utf8(output.stdout).map_err(|_| "non-UTF-8 Git config".into())
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
