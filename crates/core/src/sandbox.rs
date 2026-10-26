//! Fail-closed shell launch and bounded process output.
use elefante_guard::sandbox::{self, HiddenKind, HiddenPath, Network, Policy};
use std::{collections::VecDeque, ffi::OsString, path::PathBuf, process::Stdio, time::Duration};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::Path,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

type RootLocks = Mutex<HashMap<PathBuf, Weak<tokio::sync::RwLock<()>>>>;
static ROOT_LOCKS: OnceLock<RootLocks> = OnceLock::new();
static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DETECTED: OnceLock<Sandbox> = OnceLock::new();
static PROBED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Session-owned policy and scratch directory. Clones of the executor share this value.
#[derive(Debug)]
pub struct SessionSandbox {
    state: Sandbox,
    policy: Policy,
    bash: PathBuf,
    pub git: PathBuf,
    pub lock: Arc<tokio::sync::RwLock<()>>,
    initial_git: BTreeSet<PathBuf>,
    tool_dirs: Vec<PathBuf>,
    rustup_home: Option<PathBuf>,
    main_git: Option<PathBuf>,
}

impl Drop for SessionSandbox {
    fn drop(&mut self) {
        // This exact directory was created exclusively by this session.
        let _ = fs::remove_dir_all(&self.policy.temp);
    }
}

/// Resolves a host program from the fixed system directories.
///
/// # Errors
/// Fails when the program is absent or its path cannot be canonicalized.
pub fn binary(name: &str) -> Result<PathBuf, String> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| format!("{name} is unavailable in fixed system directories"))
        .and_then(|path| path.canonicalize().map_err(|error| error.to_string()))
}

/// Setup advice for a host whose sandbox mechanism is missing or cannot launch.
const SETUP: &str = if cfg!(target_os = "linux") {
    "install bubblewrap and enable unprivileged user namespaces, or set ELEFANTE_SANDBOX=0 to opt out"
} else {
    "set ELEFANTE_SANDBOX=0 to opt out"
};

/// The refusal every bash or harness caller returns for an `Unavailable` session.
#[must_use]
pub fn unavailable(reason: &str) -> String {
    format!("sandbox unavailable: {reason}")
}

fn detect() -> Sandbox {
    if std::env::var_os("ELEFANTE_SANDBOX").is_some_and(|value| value == "0") {
        return Sandbox::Disabled;
    }
    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
        Sandbox::Seatbelt
    } else if cfg!(target_os = "linux") {
        binary("bwrap").map_or_else(
            |error| Sandbox::Unavailable(format!("{error}; {SETUP}")),
            |bwrap| Sandbox::Bubblewrap { bwrap },
        )
    } else if cfg!(target_os = "macos") {
        Sandbox::Unavailable(format!("/usr/bin/sandbox-exec is missing; {SETUP}"))
    } else {
        Sandbox::Unavailable(format!("this OS has no supported sandbox; {SETUP}"))
    }
}

const TEMP_PREFIX: &str = "elefante-sandbox-";

/// Creates `elefante-sandbox-<pid>-<n>` with mode 0700. When that name exists (a leftover from
/// a reused pid, or another user's directory on a shared `/tmp`), it retries with a random suffix.
fn private_temp_dir() -> Result<PathBuf, String> {
    private_temp_dir_in(&fs::canonicalize(std::env::temp_dir()).map_err(|error| error.to_string())?)
}

fn private_temp_dir_in(base: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let id = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = format!("{TEMP_PREFIX}{}-{id}", std::process::id());
    for _ in 0..8 {
        let temp = base.join(&name);
        match fs::DirBuilder::new().mode(0o700).create(&temp) {
            Ok(()) => return Ok(temp),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                name = format!(
                    "{TEMP_PREFIX}{}-{id}-{:016x}",
                    std::process::id(),
                    random()?
                );
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("no free private sandbox temp directory name".into())
}

fn random() -> Result<u64, String> {
    use std::io::Read as _;
    let mut bytes = [0_u8; 8];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| error.to_string())?;
    Ok(u64::from_ne_bytes(bytes))
}

/// Removes this user's `elefante-sandbox-<pid>-*` directories whose process no longer runs.
///
/// A crashed daemon leaves its session temp directories behind. The daemon calls this once at
/// start and gets the number it removed. Symlinks, other users' entries and entries it cannot
/// inspect stay in place.
#[must_use]
pub fn sweep_stale_temp_dirs() -> usize {
    sweep_stale_temp_dirs_in(&std::env::temp_dir())
}

fn sweep_stale_temp_dirs_in(base: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = fs::read_dir(base) else {
        return 0;
    };
    let uid = rustix::process::getuid().as_raw();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(TEMP_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || metadata.uid() != uid || process_alive(pid) {
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn process_alive(pid: i32) -> bool {
    if u32::try_from(pid).is_ok_and(|pid| pid == std::process::id()) {
        return true;
    }
    rustix::process::Pid::from_raw(pid).is_none_or(|pid| {
        // EPERM means the process exists under another user; only ESRCH proves it is gone.
        rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH)
    })
}

/// Detects the host mechanism and runs `true` under a full profile, once per process.
///
/// The daemon calls this at start; every later session takes the stored state. A probe that
/// cannot launch yields `Unavailable` with its reason.
pub async fn probe() -> Sandbox {
    if let Some(state) = DETECTED.get() {
        return state.clone();
    }
    let detected = detect();
    let state = if matches!(detected, Sandbox::Disabled | Sandbox::Unavailable(_)) {
        detected
    } else {
        match probe_detected(detected.clone()).await {
            Ok(()) => detected,
            Err(reason) => Sandbox::Unavailable(format!("sandbox probe failed: {reason}")),
        }
    };
    let state = DETECTED.get_or_init(|| state).clone();
    if !matches!(state, Sandbox::Unavailable(_)) {
        let _ = PROBED.set(());
    }
    state
}

async fn probe_detected(detected: Sandbox) -> Result<(), String> {
    let root = private_temp_dir()?;
    let result = async {
        let session = SessionSandbox::with_state(&root, detected)?;
        if let Sandbox::Unavailable(reason) = session.state() {
            return Err(reason.clone());
        }
        let output = session.run_shell_unprobed("true", Network::Off).await?;
        if output.success {
            Ok(())
        } else {
            Err(output.content)
        }
    }
    .await;
    let _ = fs::remove_dir_all(&root);
    result
}

fn base_policy(root: &Path, home: PathBuf, harness_dir: PathBuf) -> Result<Policy, String> {
    let temp = private_temp_dir()?;
    let mut protected = vec![
        harness_dir.join("run/elefante.sock"),
        harness_dir.join("run/token"),
    ];
    if let Some(path) = std::env::var_os("ELEFANTE_SOCKET") {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            protected.push(parent.join("token"));
        }
        protected.push(path);
    }
    if let Some(directory) = std::env::var_os("XDG_RUNTIME_DIR") {
        let directory = PathBuf::from(directory);
        protected.push(directory.join("elefante.sock"));
        protected.push(directory.join("token"));
    }
    Ok(Policy {
        root: root.into(),
        home,
        harness_dir,
        protected,
        temp,
        readable: Vec::new(),
        writable: Vec::new(),
        read_only: Vec::new(),
        hidden: Vec::new(),
        network: Network::Off,
    })
}

impl SessionSandbox {
    /// Creates a private session policy without opening any credential file.
    ///
    /// # Errors
    /// Fails closed for invalid roots, unavailable binaries, or incomplete root scans.
    pub fn new(root: &Path) -> Result<Self, String> {
        Self::with_state(root, DETECTED.get_or_init(detect).clone())
    }

    fn with_state(root: &Path, mut state: Sandbox) -> Result<Self, String> {
        let home = std::env::var_os("HOME").ok_or("HOME is unavailable")?;
        let home = fs::canonicalize(home).map_err(|error| error.to_string())?;
        let harness_dir = home.join(".elefante");
        let root_error = sandbox::validate_root(root, &home, &harness_dir).err();
        let bash = binary("bash")?;
        let git = binary("git")?;
        if let Some(error) = root_error {
            state = Sandbox::Unavailable(error.to_string());
        }
        let mut programs = vec![bash.clone(), git.clone()];
        if let Sandbox::Bubblewrap { bwrap } = &state {
            programs.push(bwrap.clone());
        }
        for program in programs {
            if program.starts_with(root) {
                state =
                    Sandbox::Unavailable("sandbox executable is inside the session root".into());
                break;
            }
        }
        let policy = base_policy(root, home, harness_dir)?;
        let mut locks = ROOT_LOCKS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| "root lock poisoned")?;
        let lock = locks.get(root).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(tokio::sync::RwLock::new(()));
            locks.insert(root.into(), Arc::downgrade(&lock));
            lock
        });
        drop(locks);
        let mut session = Self {
            state,
            policy,
            bash,
            git,
            lock,
            initial_git: BTreeSet::new(),
            tool_dirs: Vec::new(),
            rustup_home: None,
            main_git: None,
        };
        for name in ["home", "cargo", "rustup"] {
            fs::create_dir(session.policy.temp.join(name)).map_err(|error| error.to_string())?;
        }
        session.policy.readable = [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/etc/ld.so.cache",
            "/etc/alternatives",
            "/etc/ssl/certs",
            "/etc/resolv.conf",
            "/etc/nsswitch.conf",
            "/etc/hosts",
            "/System/Library",
            "/Library/Apple",
            "/Library/Developer/CommandLineTools",
            "/private/etc",
            "/dev/null",
            "/dev/random",
            "/dev/urandom",
            "/private/var/db/dyld",
        ]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect();
        for program in [&session.bash, &session.git] {
            let parent = program
                .parent()
                .ok_or("binary has no directory")?
                .to_path_buf();
            if !session
                .policy
                .readable
                .iter()
                .any(|path| parent.starts_with(path))
            {
                session.policy.readable.push(parent);
            }
        }
        if let Err(error) = session.configure_toolchains() {
            session.state = Sandbox::Unavailable(error);
        }
        if !matches!(session.state, Sandbox::Unavailable(_) | Sandbox::Disabled) {
            match initial_git_directory(root) {
                Ok(directory) => session.main_git = directory,
                Err(error) => session.state = Sandbox::Unavailable(error),
            }
            match session.scan() {
                Ok(policy) => session.initial_git = policy.read_only.into_iter().collect(),
                Err(error) => session.state = Sandbox::Unavailable(error),
            }
        }
        Ok(session)
    }

    fn configure_toolchains(&mut self) -> Result<(), String> {
        use std::os::unix::fs::symlink;
        let cargo = std::env::var_os("CARGO_HOME")
            .map_or_else(|| self.policy.home.join(".cargo"), PathBuf::from);
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map_or_else(|| self.policy.home.join(".rustup"), PathBuf::from);
        for directory in [cargo.join("bin"), self.policy.home.join(".bun/bin")] {
            if directory.is_dir() {
                self.policy.readable.push(directory.clone());
                self.tool_dirs.push(directory);
            }
        }
        let registry = cargo.join("registry");
        if registry.is_dir() {
            self.policy.readable.push(registry.clone());
            symlink(registry, self.policy.temp.join("cargo/registry"))
                .map_err(|error| error.to_string())?;
        }
        // Settings contain only toolchain selection; Cargo credentials are never opened.
        let settings = rustup.join("settings.toml");
        if settings.is_file() {
            let text = fs::read_to_string(&settings).map_err(|error| error.to_string())?;
            let selected = text.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "default_toolchain").then(|| value.trim().trim_matches('"'))
            });
            if let Some(selected) = selected {
                if selected.is_empty()
                    || selected.contains(['/', '\\'])
                    || selected == "."
                    || selected == ".."
                {
                    return Err("invalid rustup default toolchain".into());
                }
                let toolchain = rustup.join("toolchains").join(selected);
                if toolchain.is_dir() {
                    self.policy.readable.extend([settings, toolchain]);
                    self.rustup_home = Some(rustup);
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn state(&self) -> &Sandbox {
        &self.state
    }

    /// Explicit host configuration opt-out; model input cannot change this state.
    pub fn disable(&mut self) {
        self.state = Sandbox::Disabled;
    }

    /// Rescans every directory for git metadata, and secret names without reading their contents.
    ///
    /// # Errors
    /// Any unreadable directory or unsupported git indirection fails closed.
    pub fn scan(&self) -> Result<Policy, String> {
        use std::os::unix::fs::MetadataExt;
        let mut policy = self.policy.clone();
        let mut queue = vec![(policy.root.clone(), false)];
        while let Some((directory, secret_pruned)) = queue.pop() {
            for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                let kind = entry.file_type().map_err(|error| error.to_string())?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.eq_ignore_ascii_case(".git") {
                    policy.read_only.push(path.clone());
                    if kind.is_symlink() {
                        return Err("symlink Git metadata is refused".into());
                    }
                    let gitdir = if kind.is_dir() {
                        path.clone()
                    } else {
                        git_reference(&path, true)?
                    };
                    let mut targets = vec![gitdir.clone()];
                    if gitdir.join("commondir").exists() {
                        targets.push(git_reference(&gitdir.join("commondir"), false)?);
                    }
                    for target in targets {
                        if !target.starts_with(&policy.root)
                            && !self
                                .main_git
                                .as_ref()
                                .is_some_and(|main| target.starts_with(main))
                        {
                            return Err(
                                "Git metadata points outside the root and its initial repository"
                                    .into(),
                            );
                        }
                        if !target.starts_with(&policy.root) {
                            policy.readable.push(target.clone());
                        }
                        if !policy.read_only.contains(&target) {
                            policy.read_only.push(target);
                        }
                    }
                }
                let in_git_metadata = path.components().any(|component| {
                    component
                        .as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(".git")
                });
                let hard_link = kind.is_file()
                    && !in_git_metadata
                    && !secret_pruned
                    && entry.metadata().map_err(|error| error.to_string())?.nlink() > 1;
                let secret = hard_link
                    || !secret_pruned
                        && (name == ".elefante"
                            || name == ".ssh"
                            || name == ".env"
                            || name.starts_with(".env.")
                            || matches!(
                                name.as_ref(),
                                ".netrc"
                                    | ".npmrc"
                                    | ".git-credentials"
                                    | "credentials"
                                    | "credentials.toml"
                            )
                            || name.starts_with("id_rsa")
                            || name.to_ascii_lowercase().ends_with(".pem")
                            || name.to_ascii_lowercase().ends_with(".key"));
                if secret {
                    policy.hidden.push(HiddenPath {
                        path: path.clone(),
                        kind: if kind.is_dir() {
                            HiddenKind::Directory
                        } else {
                            HiddenKind::File
                        },
                    });
                }
                if kind.is_dir() {
                    queue.push((
                        path,
                        secret_pruned || matches!(name.as_ref(), "target" | "node_modules"),
                    ));
                }
            }
        }
        policy.validate().map_err(|error| error.to_string())?;
        Ok(policy)
    }

    /// Executes a shell command under the scanned policy.
    ///
    /// # Errors
    /// Fails closed for unavailable isolation, policy errors, or failed process launch.
    pub async fn run_shell(&self, source: &str, network: Network) -> Result<ProcessOutput, String> {
        if let Sandbox::Unavailable(reason) = &self.state {
            return Err(unavailable(reason));
        }
        if !matches!(self.state, Sandbox::Disabled) {
            PROBED
                .get_or_try_init(|| async {
                    let output = self.run_shell_unprobed("true", Network::Off).await?;
                    if output.success {
                        Ok(())
                    } else {
                        Err(unavailable(&format!(
                            "probe failed: {}; {SETUP}",
                            output.content
                        )))
                    }
                })
                .await?;
        }
        self.run_shell_unprobed(source, network).await
    }

    async fn run_shell_unprobed(
        &self,
        source: &str,
        network: Network,
    ) -> Result<ProcessOutput, String> {
        let _lock = self.lock.read().await;
        let mut policy = if matches!(self.state, Sandbox::Disabled) {
            self.policy.clone()
        } else {
            self.scan()?
        };
        policy.network = network;
        let sandboxed = !matches!(self.state, Sandbox::Disabled);
        let env_source = self
            .rustup_home
            .as_ref()
            .map(|path| ("RUSTUP_HOME".into(), path.as_os_str().to_owned()));
        let environment = if sandboxed {
            policy
                .environment(env_source, &self.tool_dirs, true)
                .map_err(|error| error.to_string())?
        } else {
            self.disabled_environment()?
        };
        #[cfg(target_os = "linux")]
        let filters = if sandboxed {
            seccomp_filters(network)?
        } else {
            Vec::new()
        };
        let mut command = match &self.state {
            Sandbox::Seatbelt => {
                let (profile, params) =
                    sandbox::seatbelt_profile(&policy).map_err(|error| error.to_string())?;
                let mut command = launcher(&self.bash, &format!("{CLOSE_INHERITED}{EXEC}"));
                command.arg("/usr/bin/sandbox-exec").args(["-p", &profile]);
                for (key, value) in params {
                    command
                        .arg("-D")
                        .arg(format!("{key}={}", value.to_string_lossy()));
                }
                command.arg(&self.bash).args(["-c", source]);
                command
            }
            Sandbox::Disabled => {
                let mut command = launcher(&self.bash, &format!("{CLOSE_INHERITED}{EXEC}"));
                command.arg(&self.bash).args(["-c", source]);
                command
            }
            Sandbox::Bubblewrap { bwrap } => {
                #[cfg(target_os = "linux")]
                {
                    use std::os::fd::AsRawFd;
                    let spec = sandbox::LaunchSpec {
                        bash: self.bash.clone(),
                        command: source.into(),
                        environment: environment.clone(),
                        seccomp_fds: vec![3, 4],
                    };
                    let args =
                        sandbox::bwrap_args(&policy, &spec).map_err(|error| error.to_string())?;
                    let mut command =
                        launcher(&self.bash, &format!("{CLOSE_INHERITED}{SECCOMP_EXEC}"));
                    command.arg(bwrap);
                    for file in &filters {
                        command.arg(format!(
                            "/proc/{}/fd/{}",
                            std::process::id(),
                            file.as_raw_fd()
                        ));
                    }
                    command.args(args);
                    command
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = bwrap;
                    return Err("bubblewrap requires Linux".into());
                }
            }
            Sandbox::Unavailable(_) => unreachable!(),
        };
        command
            .env_clear()
            .envs(environment)
            .current_dir(&policy.root);
        let result = run_process(command, command_timeout()).await;
        #[cfg(target_os = "linux")]
        if sandboxed {
            self.quarantine_new_git()?;
        }
        result
    }

    /// The `Disabled` environment: the same allowlist, the host's `HOME`, `CARGO_HOME` and
    /// `RUSTUP_HOME`, and the toolchain directories ahead of the system `PATH`.
    fn disabled_environment(&self) -> Result<Vec<(OsString, OsString)>, String> {
        let host = ["CARGO_HOME", "RUSTUP_HOME"]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value)));
        let mut environment = sandbox::environment(host, false);
        for (key, value) in &mut environment {
            if key == "PATH" {
                let dirs = self
                    .tool_dirs
                    .iter()
                    .cloned()
                    .chain(std::env::split_paths(value));
                *value = std::env::join_paths(dirs).map_err(|error| error.to_string())?;
            }
        }
        environment.push(("HOME".into(), self.policy.home.clone().into_os_string()));
        Ok(environment)
    }

    #[cfg(target_os = "linux")]
    fn quarantine_new_git(&self) -> Result<(), String> {
        // Discovery must not parse gitdir/commondir: a malformed new file still needs moving.
        let mut queue = vec![self.policy.root.clone()];
        let mut found = Vec::new();
        while let Some(directory) = queue.pop() {
            for entry in fs::read_dir(directory).map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let path = entry.path();
                let kind = entry.file_type().map_err(|error| error.to_string())?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(".git")
                {
                    if !self.initial_git.contains(&path) {
                        found.push(path);
                    }
                } else if kind.is_dir() {
                    queue.push(path);
                }
            }
        }
        if found.is_empty() {
            return Ok(());
        }
        let session = self
            .policy
            .temp
            .file_name()
            .ok_or("missing session identifier")?;
        let quarantine = self.policy.harness_dir.join("quarantine").join(session);
        fs::create_dir_all(&quarantine).map_err(|error| error.to_string())?;
        for path in &found {
            let id = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            fs::rename(path, quarantine.join(id.to_string())).map_err(|error| {
                format!(
                    "new Git metadata at {} could not be quarantined: {error}",
                    path.display()
                )
            })?;
        }
        Err(format!(
            "{} new Git metadata entr{} quarantined",
            found.len(),
            if found.len() == 1 { "y" } else { "ies" }
        ))
    }
}

/// Writes each seccomp program into an anonymous memory file.
///
/// The memory files have no path, so a sandboxed command cannot redirect the write. The daemon
/// keeps close-on-exec set, so no concurrent child inherits them.
#[cfg(target_os = "linux")]
fn seccomp_filters(network: Network) -> Result<Vec<fs::File>, String> {
    use rustix::fs::{MemfdFlags, memfd_create};
    use std::io::Write as _;
    let architecture = if cfg!(target_arch = "x86_64") {
        sandbox::Architecture::X86_64
    } else {
        sandbox::Architecture::Aarch64
    };
    let programs =
        sandbox::seccomp_programs(architecture, network).map_err(|error| error.to_string())?;
    let mut files = Vec::new();
    for program in &programs {
        let mut file = fs::File::from(
            memfd_create("elefante-seccomp", MemfdFlags::CLOEXEC)
                .map_err(|error| error.to_string())?,
        );
        file.write_all(program).map_err(|error| error.to_string())?;
        files.push(file);
    }
    Ok(files)
}

/// Closes every descriptor above stderr that the daemon inherited from whatever launched it, so a
/// command never receives one. `bash`, unlike `dash`, accepts multi-digit descriptor numbers.
const CLOSE_INHERITED: &str = r#"for f in /dev/fd/*; do n=${f##*/}; if [ "$n" -gt 2 ] && [ -e "$f" ]; then eval "exec $n<&-"; fi; done; "#;

const EXEC: &str = r#"exec "$0" "$@""#;

/// Reopens the two seccomp programs as fds 3 and 4 in the launched process only, then execs
/// bubblewrap. A failed reopen stops the `exec`, so the command never runs unfiltered.
#[cfg(target_os = "linux")]
const SECCOMP_EXEC: &str = r#"a=$1 b=$2; shift 2; exec "$0" "$@" 3<"$a" 4<"$b""#;

/// Starts `script` in the fixed `bash`; the program to exec and its arguments follow.
fn launcher(bash: &Path, script: &str) -> Command {
    let mut command = Command::new(bash);
    command.args(["-c", script]);
    command
}

fn git_reference(path: &Path, gitfile: bool) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.nlink() > 1 || metadata.len() > 4096 {
        return Err("Git indirection must be a small regular file with one link".into());
    }
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value = if gitfile {
        text.strip_prefix("gitdir:")
            .ok_or("invalid gitdir file")?
            .trim()
    } else {
        text.trim()
    };
    if value.is_empty() || value.contains(['\n', '\r']) {
        return Err("invalid Git metadata target".into());
    }
    let target = path
        .parent()
        .ok_or("Git metadata has no parent")?
        .join(value)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !target.is_dir() {
        return Err("Git metadata target is not a directory".into());
    }
    Ok(target)
}

fn initial_git_directory(root: &Path) -> Result<Option<PathBuf>, String> {
    let path = root.join(".git");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() {
        return Err("symlink Git metadata is refused".into());
    }
    let directory = if metadata.is_dir() {
        path
    } else {
        git_reference(&path, true)?
    };
    let common = if directory.join("commondir").exists() {
        git_reference(&directory.join("commondir"), false)?
    } else {
        directory
    };
    if !common.starts_with(root)
        && (!common.join("objects").is_dir()
            || !common.join("refs").is_dir()
            || !common.join("HEAD").is_file())
    {
        return Err("external Git metadata is not a repository".into());
    }
    Ok(Some(common))
}

const OUTPUT_LIMIT: usize = 32 * 1024;

/// The wall-clock limit for one bash call or harness git write: `ELEFANTE_BASH_TIMEOUT_SECS`,
/// default 120 seconds.
#[must_use]
pub fn command_timeout() -> Duration {
    let seconds = std::env::var("ELEFANTE_BASH_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);
    Duration::from_secs(seconds)
}

/// The host isolation mechanism discovered at startup.
#[derive(Debug, Clone)]
pub enum Sandbox {
    Seatbelt,
    Bubblewrap { bwrap: PathBuf },
    Disabled,
    Unavailable(String),
}

/// A completed process, with bounded combined output.
#[derive(Debug)]
pub struct ProcessOutput {
    pub content: String,
    pub success: bool,
}

struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let group = self
            .0
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw);
        if let Some(group) = group {
            // The group may already be gone; nothing else can be done with an error here.
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

/// Each stream keeps this much from its start and its end, so either stream can fill the budget.
const KEEP: usize = OUTPUT_LIMIT / 2;
/// Room left in the 32 KiB result for the exit status line and the bash tool's `.git` hint.
const RESERVE: usize = 512;

#[derive(Default)]
struct CappedOutput {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}
impl CappedOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head_count = (KEEP - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_count]);
        for byte in &bytes[head_count..] {
            if self.tail.len() == KEEP {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    /// The kept text. When bytes were dropped, the text is at least `OUTPUT_LIMIT` long, so
    /// clipping it to a share of the budget always cuts inside the dropped middle.
    fn text(self) -> String {
        let tail: Vec<u8> = self.tail.into_iter().collect();
        if self.total == self.head.len() + tail.len() {
            let mut bytes = self.head;
            bytes.extend_from_slice(&tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        text.push_str(&String::from_utf8_lossy(&tail));
        text
    }
}

/// Splits one output budget across stdout and stderr. A stream that needs less than half the
/// budget keeps all of it, and the other stream gets the rest.
fn combined_text(out: CappedOutput, err: CappedOutput) -> String {
    let budget = OUTPUT_LIMIT - RESERVE;
    let (out_total, err_total) = (out.total, err.total);
    let (out, err) = (out.text(), err.text());
    let out_share = if out.len() + err.len() <= budget || out.len() <= budget / 2 {
        out.len()
    } else if err.len() <= budget / 2 {
        budget - err.len()
    } else {
        budget / 2
    };
    let mut content = crate::tools::clip_middle(&out, out_share, out_total);
    content.push_str(&crate::tools::clip_middle(
        &err,
        budget - out_share,
        err_total,
    ));
    content
}

async fn drain(
    mut pipe: impl AsyncRead + Unpin,
    output: std::sync::Arc<tokio::sync::Mutex<CappedOutput>>,
) -> std::io::Result<()> {
    let mut bytes = vec![0; 8192];
    loop {
        let count = pipe.read(&mut bytes).await?;
        if count == 0 {
            return Ok(());
        }
        output.lock().await.push(&bytes[..count]);
    }
}

/// Runs an already configured command with a deadline and bounded output.
///
/// # Errors
/// Returns launch, wait, pipe, or deadline failures. The process group is killed on every exit.
pub async fn run_process(mut command: Command, timeout: Duration) -> Result<ProcessOutput, String> {
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| format!("process launch failed: {error}"))?;
    let _group = ProcessGroup(child.id());
    let stdout = child.stdout.take().ok_or("missing stdout pipe")?;
    let stderr = child.stderr.take().ok_or("missing stderr pipe")?;
    let out = std::sync::Arc::new(tokio::sync::Mutex::new(CappedOutput::default()));
    let err = std::sync::Arc::new(tokio::sync::Mutex::new(CappedOutput::default()));
    // These futures remain owned here: cancellation drops both readers immediately.
    let readers = async {
        let (a, b) = tokio::join!(drain(stdout, out.clone()), drain(stderr, err.clone()));
        a.and(b)
    };
    tokio::pin!(readers);
    let wait = async {
        let mut read_result = None;
        let status = tokio::select! {
            status = child.wait() => status,
            result = &mut readers => {
                read_result = Some(result);
                child.wait().await
            }
        }
        .map_err(|error| format!("process wait failed: {error}"))?;
        if let Some(result) = read_result {
            result.map_err(|error| format!("process output failed: {error}"))?;
        } else if let Ok(result) = tokio::time::timeout(Duration::from_secs(2), &mut readers).await
        {
            result.map_err(|error| format!("process output failed: {error}"))?;
        }
        Ok::<_, String>(status)
    };
    let status = tokio::time::timeout(timeout, wait).await.map_err(|_| {
        format!(
            "command timed out after {} s; process group terminated",
            timeout.as_secs_f32()
        )
    })??;
    let mut content = combined_text(
        std::mem::take(&mut *out.lock().await),
        std::mem::take(&mut *err.lock().await),
    );
    if !status.success() {
        content.insert_str(0, &format!("exit status {status}\n"));
    }
    Ok(ProcessOutput {
        content,
        success: status.success(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("/usr/bin/true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait for true");
        pid
    }

    #[test]
    fn sweep_removes_only_own_dead_directories() {
        let base = tempfile::tempdir().expect("tempdir");
        let dead = dead_pid();
        let stale = base.path().join(format!("{TEMP_PREFIX}{dead}-0"));
        let stale_retry = base.path().join(format!("{TEMP_PREFIX}{dead}-1-00ff"));
        let live = base
            .path()
            .join(format!("{TEMP_PREFIX}{}-0", std::process::id()));
        let other = base.path().join(format!("other-{dead}-0"));
        let target = base.path().join("target");
        let link = base.path().join(format!("{TEMP_PREFIX}{dead}-2"));
        for directory in [&stale, &stale_retry, &live, &other, &target] {
            fs::create_dir(directory).expect("create directory");
        }
        fs::write(stale.join("cache"), "x").expect("write cache");
        symlink(&target, &link).expect("symlink");

        assert_eq!(sweep_stale_temp_dirs_in(base.path()), 2);
        assert!(!stale.exists() && !stale_retry.exists());
        // Positive controls: a live owner, a foreign name, and a symlink all stay.
        assert!(live.is_dir() && other.is_dir() && target.is_dir());
        assert!(fs::symlink_metadata(&link).is_ok());
    }

    #[test]
    fn temp_dir_creation_retries_with_a_random_suffix() {
        let base = tempfile::tempdir().expect("tempdir");
        let first = private_temp_dir_in(base.path()).expect("first directory");
        assert_eq!(
            first
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.split('-').count()),
            Some(4),
            "an unused name carries no suffix"
        );
        let next = SEQUENCE.load(std::sync::atomic::Ordering::Relaxed);
        for id in next..next + 256 {
            fs::create_dir(
                base.path()
                    .join(format!("{TEMP_PREFIX}{}-{id}", std::process::id())),
            )
            .expect("occupy name");
        }
        let retried = private_temp_dir_in(base.path()).expect("retried directory");
        let name = retried
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name");
        assert_eq!(name.split('-').count(), 5, "{name} carries a random suffix");
        let mode = fs::metadata(&retried)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
