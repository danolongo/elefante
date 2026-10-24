//! Pure sandbox policy rendering. Applying a policy belongs to the process launcher.
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fmt::Write as _,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Off,
    Approved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    Aarch64,
}

#[cfg(target_os = "linux")]
const ENOSYS_CALLS: &[i64] = &[435, 425, 426, 427];
#[cfg(target_os = "linux")]
const X86_64_EPERM_CALLS: &[i64] = &[
    272, 308, 165, 166, 155, 428, 429, 430, 431, 432, 433, 442, 101, 310, 311, 321, 298, 323, 250,
    248, 249, 175, 313, 176, 246, 320,
];
#[cfg(target_os = "linux")]
const AARCH64_EPERM_CALLS: &[i64] = &[
    97, 268, 40, 39, 41, 428, 429, 430, 431, 432, 433, 442, 117, 270, 271, 280, 241, 282, 219, 217,
    218, 105, 273, 106, 104, 294,
];

#[cfg(target_os = "linux")]
fn syscall_numbers(
    arch: Architecture,
) -> (
    seccompiler::TargetArch,
    &'static [i64],
    &'static [i64],
    i64,
    i64,
) {
    match arch {
        Architecture::X86_64 => (
            seccompiler::TargetArch::x86_64,
            ENOSYS_CALLS,
            X86_64_EPERM_CALLS,
            41,
            16,
        ),
        Architecture::Aarch64 => (
            seccompiler::TargetArch::aarch64,
            ENOSYS_CALLS,
            AARCH64_EPERM_CALLS,
            198,
            29,
        ),
    }
}

#[derive(Debug, Error)]
pub enum SeccompError {
    #[error("seccomp compilation is available only on Linux")]
    Unsupported,
    #[error("cannot compile seccomp filter: {0}")]
    Compile(String),
}

/// Compiles two stacked filters, with an explicit x32 guard on `x86_64`.
///
/// # Errors
/// Returns an error on unsupported platforms or failed compilation.
#[cfg(target_os = "linux")]
pub fn seccomp_programs(
    arch: Architecture,
    network: Network,
) -> Result<Vec<Vec<u8>>, SeccompError> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::collections::BTreeMap;
    let (target, enosys, eperm, socket, ioctl) = syscall_numbers(arch);
    let mut output = Vec::new();
    for (calls, errno) in [(enosys, 38), (eperm, 1)] {
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
            calls.iter().map(|nr| (*nr, Vec::new())).collect();
        if errno == 1 {
            let allowed = if network == Network::Approved {
                vec![2, 10, 16]
            } else {
                vec![1, 2, 10, 16]
            };
            let conditions = allowed
                .into_iter()
                .map(|family| {
                    SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::Ne, family)
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| SeccompError::Compile(e.to_string()))?;
            let mut socket_rules = vec![
                SeccompRule::new(conditions).map_err(|e| SeccompError::Compile(e.to_string()))?,
            ];
            if network == Network::Approved {
                socket_rules.push(
                    SeccompRule::new(vec![
                        SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, 1)
                            .map_err(|e| SeccompError::Compile(e.to_string()))?,
                    ])
                    .map_err(|e| SeccompError::Compile(e.to_string()))?,
                );
            }
            rules.insert(socket, socket_rules);
            let tiocsti =
                SeccompCondition::new(1, SeccompCmpArgLen::Qword, SeccompCmpOp::Eq, 0x5412)
                    .map_err(|e| SeccompError::Compile(e.to_string()))?;
            rules.insert(
                ioctl,
                vec![
                    SeccompRule::new(vec![tiocsti])
                        .map_err(|e| SeccompError::Compile(e.to_string()))?,
                ],
            );
        }
        let filter = SeccompFilter::new(
            rules,
            SeccompAction::Allow,
            SeccompAction::Errno(errno),
            target,
        )
        .map_err(|e| SeccompError::Compile(e.to_string()))?;
        let program: BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| SeccompError::Compile(e.to_string()))?;
        let mut bytes = Vec::new();
        if arch == Architecture::X86_64 {
            // seccompiler checks architecture, but treats x32 syscall numbers as ordinary misses.
            for (code, jt, jf, k) in [
                (0x20_u16, 0, 0, 0_u32),
                (0x45, 0, 1, 0x4000_0000),
                (0x06, 0, 0, 0x8000_0000),
            ] {
                bytes.extend(code.to_ne_bytes());
                bytes.extend([jt, jf]);
                bytes.extend(k.to_ne_bytes());
            }
        }
        for instruction in program {
            bytes.extend(instruction.code.to_ne_bytes());
            bytes.extend([instruction.jt, instruction.jf]);
            bytes.extend(instruction.k.to_ne_bytes());
        }
        output.push(bytes);
    }
    Ok(output)
}

/// # Errors
/// Seccomp is a Linux facility.
#[cfg(not(target_os = "linux"))]
pub fn seccomp_programs(
    _arch: Architecture,
    _network: Network,
) -> Result<Vec<Vec<u8>>, SeccompError> {
    Err(SeccompError::Unsupported)
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub root: PathBuf,
    pub home: PathBuf,
    pub harness_dir: PathBuf,
    /// Exact daemon socket and authentication token paths, including non-default locations.
    pub protected: Vec<PathBuf>,
    pub temp: PathBuf,
    pub readable: Vec<PathBuf>,
    pub writable: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
    pub hidden: Vec<HiddenPath>,
    pub network: Network,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenPath {
    pub path: PathBuf,
    pub kind: HiddenKind,
}

#[derive(Debug, Error)]
pub enum RootError {
    #[error("sandbox paths must be absolute and normalized")]
    InvalidPath,
    #[error("sandbox root overlaps the home or harness directory")]
    ProtectedRoot,
    #[error("unsafe sandbox parameter: {0}")]
    UnsafeParameter(PathBuf),
    #[error("sandbox readable mount exposes a protected location: {0}")]
    ProtectedMount(PathBuf),
    #[error("private sandbox temp must be outside the workspace")]
    InvalidTemp,
    #[error("bubblewrap requires two distinct inherited seccomp descriptors above stderr")]
    InvalidSeccompDescriptors,
    #[error("sandbox environment contains an unsafe or duplicate variable: {0}")]
    UnsafeEnvironment(String),
}

/// Checks canonical paths supplied by the session creator.
///
/// # Errors
/// Rejects broad roots, harness overlap, and non-normalized paths.
pub fn validate_root(root: &Path, home: &Path, harness_dir: &Path) -> Result<(), RootError> {
    for path in [root, home, harness_dir] {
        validate_path(path)?;
    }
    if root.parent().is_none()
        || home.starts_with(root)
        || root.starts_with(harness_dir)
        || harness_dir.starts_with(root)
        || root.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git")
        })
    {
        return Err(RootError::ProtectedRoot);
    }
    Ok(())
}

fn validate_path(path: &Path) -> Result<(), RootError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(RootError::InvalidPath);
    }
    let Some(value) = path.to_str() else {
        return Err(RootError::UnsafeParameter(path.into()));
    };
    if value
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(RootError::UnsafeParameter(path.into()));
    }
    Ok(())
}

impl Policy {
    /// Builds the cleared environment with session paths and exact tool binary directories.
    /// The caller supplies only discovered tool directories, never the daemon's PATH.
    ///
    /// # Errors
    /// Rejects binary directories outside the readable mount set.
    pub fn environment(
        &self,
        source: impl IntoIterator<Item = (OsString, OsString)>,
        tool_bin_dirs: &[PathBuf],
        sandboxed: bool,
    ) -> Result<Vec<(OsString, OsString)>, RootError> {
        self.validate()?;
        let mut dirs: Vec<PathBuf> = Vec::new();
        #[cfg(target_os = "macos")]
        dirs.push("/Library/Developer/CommandLineTools/usr/bin".into());
        dirs.extend([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        for path in tool_bin_dirs {
            validate_path(path)?;
            if !self.readable.iter().any(|allowed| path == allowed) {
                return Err(RootError::ProtectedMount(path.clone()));
            }
            if !dirs.contains(path) {
                dirs.push(path.clone());
            }
        }
        let joined = std::env::join_paths(dirs).map_err(|_| RootError::InvalidPath)?;
        let mut vars = environment(source, sandboxed);
        vars.retain(|(key, _)| key != "PATH" && key != "HOME" && key != "TMPDIR");
        vars.push(("PATH".into(), joined));
        if sandboxed {
            let home = if cfg!(target_os = "linux") {
                PathBuf::from("/tmp/home")
            } else {
                self.temp.join("home")
            };
            vars.extend([
                ("HOME".into(), home.into_os_string()),
                ("TMPDIR".into(), self.temp.as_os_str().into()),
            ]);
        }
        for (key, fallback) in [
            ("CARGO_HOME", self.temp.join("cargo")),
            ("RUSTUP_HOME", self.temp.join("rustup")),
        ] {
            if !vars.iter().any(|(name, _)| name == key) {
                vars.push((key.into(), fallback.into_os_string()));
            }
        }
        Ok(vars)
    }
    /// Validates renderer invariants; root/home identity is checked at session creation.
    ///
    /// # Errors
    /// Rejects hostile parameter values, broad mounts, or overlapping private temp.
    pub fn validate(&self) -> Result<(), RootError> {
        validate_root(&self.root, &self.home, &self.harness_dir)?;
        for path in [&self.temp]
            .into_iter()
            .chain(self.readable.iter())
            .chain(self.writable.iter())
            .chain(self.read_only.iter())
            .chain(self.hidden.iter().map(|hidden| &hidden.path))
            .chain(self.protected.iter())
        {
            validate_path(path)?;
        }
        if overlaps(&self.temp, &self.root) || overlaps(&self.temp, &self.harness_dir) {
            return Err(RootError::InvalidTemp);
        }
        for path in &self.readable {
            if path.parent().is_none()
                || matches!(
                    path.to_str(),
                    Some(
                        "/home"
                            | "/Users"
                            | "/tmp"
                            | "/private/tmp"
                            | "/run"
                            | "/var/run"
                            | "/private/var"
                    )
                )
                || self.home.starts_with(path)
                || overlaps(path, &self.harness_dir)
                || self.protected.iter().any(|secret| overlaps(path, secret))
                || self
                    .hidden
                    .iter()
                    .any(|secret| overlaps(path, &secret.path))
                || path
                    .components()
                    .any(|c| matches!(c.as_os_str().to_str(), Some(".ssh" | ".elefante")))
                || path.file_name().is_some_and(|name| {
                    matches!(
                        name.to_str(),
                        Some("credentials" | "credentials.toml" | "token" | "daemon.sock")
                    )
                })
            {
                return Err(RootError::ProtectedMount(path.clone()));
            }
        }
        for path in &self.writable {
            if !path.starts_with(&self.root) && !path.starts_with(&self.temp) {
                return Err(RootError::ProtectedMount(path.clone()));
            }
        }
        if self
            .protected
            .iter()
            .any(|path| path.starts_with(&self.root) || path.starts_with(&self.temp))
        {
            return Err(RootError::ProtectedRoot);
        }
        Ok(())
    }
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Renders a deny-default Seatbelt profile and separate `-D` path parameters.
///
/// # Errors
/// Rejects unsafe paths or invalid mount policy.
pub fn seatbelt_profile(policy: &Policy) -> Result<(String, Vec<(String, OsString)>), RootError> {
    policy.validate()?;
    let mut params = vec![
        ("ROOT".into(), policy.root.clone().into_os_string()),
        ("TEMP".into(), policy.temp.clone().into_os_string()),
    ];
    let mut profile = String::from(
        "(version 1)\n(deny default)\n(allow process-exec process-fork)\n(allow signal (target self))\n(allow sysctl-read)\n(allow file-read-metadata)\n(allow file-write* (literal \"/dev/null\"))\n(allow file-read* (subpath (param \"ROOT\")) (subpath (param \"TEMP\")))\n(allow file-write* (subpath (param \"ROOT\")) (subpath (param \"TEMP\")))\n(deny file-write* (require-all (subpath (param \"ROOT\")) (regex #\"/\\.[Gg][Ii][Tt](/|$)\")))\n(deny file-read* (require-all (subpath (param \"ROOT\")) (regex #\"/(\\.env(\\.[^/]*)?|\\.netrc|\\.npmrc|\\.git-credentials|id_rsa[^/]*|[^/]*\\.[Pp][Ee][Mm]|[^/]*\\.[Kk][Ee][Yy])(/|$)\")))\n(deny file-read* file-write* (require-all (subpath (param \"ROOT\")) (regex #\"/\\.elefante(/|$)\")))\n",
    );
    profile = profile.replace("(target self)", "(target same-sandbox)");
    let mut traversal = BTreeSet::new();
    for path in [&policy.root, &policy.temp, &policy.harness_dir]
        .into_iter()
        .chain(policy.readable.iter())
        .chain(policy.writable.iter())
        .chain(policy.read_only.iter())
        .chain(policy.hidden.iter().map(|hidden| &hidden.path))
        .chain(policy.protected.iter())
    {
        traversal.extend(path.ancestors().skip(1).map(Path::to_path_buf));
    }
    for (index, path) in traversal.into_iter().enumerate() {
        let key = format!("TRAVERSE_{index}");
        writeln!(profile, "(allow file-read* (literal (param \"{key}\")))")
            .expect("writing to String cannot fail");
        params.push((key, path.into_os_string()));
    }
    profile.push_str(concat!(
        r#"(deny file-read* (require-all (subpath (param "ROOT")) (regex #"/\.ssh(/|$)")))"#,
        "\n",
        r#"(deny file-read* (require-all (subpath (param "ROOT")) (regex #"/\.aws/credentials(/|$)")))"#,
        "\n",
        r#"(deny file-read* (require-all (subpath (param "ROOT")) (regex #"/\.cargo/credentials(\.toml)?(/|$)")))"#,
        "\n",
    ));
    let hidden: Vec<PathBuf> = policy
        .hidden
        .iter()
        .map(|item| item.path.clone())
        .chain([policy.harness_dir.clone()])
        .chain(policy.protected.iter().cloned())
        .collect();
    for (prefix, paths, rule) in [
        ("READ", &policy.readable, "allow file-read*"),
        ("WRITE", &policy.writable, "allow file-write*"),
        ("RO", &policy.read_only, "deny file-write*"),
        ("HIDDEN", &hidden, "deny file-read* file-write*"),
    ] {
        for (index, path) in paths.iter().enumerate() {
            let key = format!("{prefix}_{index}");
            writeln!(profile, "({rule} (subpath (param \"{key}\")))")
                .expect("writing to String cannot fail");
            params.push((key, path.as_os_str().into()));
        }
    }
    profile.push_str("(allow mach-lookup (global-name \"com.apple.system.logger\") (global-name \"com.apple.system.notification_center\") (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n");
    profile.push_str("(allow mach-lookup (global-name \"com.apple.cfprefsd.daemon\") (global-name \"com.apple.cfprefsd.agent\") (global-name \"com.apple.bsd.dirhelper\"))\n");
    if policy.network == Network::Approved {
        profile.push_str("(allow network-outbound (remote tcp) (remote udp) (literal \"/private/var/run/mDNSResponder\"))\n(allow mach-lookup (global-name \"com.apple.SystemConfiguration.configd\") (global-name \"com.apple.networkd\") (global-name \"com.apple.mDNSResponder\"))\n");
    }
    profile.push_str("(allow network* (local unix-socket (subpath (param \"ROOT\"))) (remote unix-socket (subpath (param \"ROOT\"))) (local unix-socket (subpath (param \"TEMP\"))) (remote unix-socket (subpath (param \"TEMP\"))))\n");
    Ok((profile, params))
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub bash: PathBuf,
    pub command: OsString,
    pub environment: Vec<(OsString, OsString)>,
    /// Inheritable descriptors prepared by the caller; repeated `--add-seccomp-fd` is required.
    pub seccomp_fds: Vec<u32>,
}

/// Renders bubblewrap arguments, preserving mount overlay order.
///
/// # Errors
/// Rejects invalid policy or executable paths.
pub fn bwrap_args(policy: &Policy, launch: &LaunchSpec) -> Result<Vec<OsString>, RootError> {
    policy.validate()?;
    validate_path(&launch.bash)?;
    validate_bwrap_environment(policy, &launch.environment)?;
    if launch.seccomp_fds.len() != 2
        || launch.seccomp_fds[0] <= 2
        || launch.seccomp_fds[1] <= 2
        || launch.seccomp_fds[0] == launch.seccomp_fds[1]
    {
        return Err(RootError::InvalidSeccompDescriptors);
    }
    let mut args: Vec<OsString> = [
        "--unshare-user",
        "--disable-userns",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
    ]
    .map(Into::into)
    .into();
    if policy.network == Network::Off {
        args.push("--unshare-net".into());
    }
    args.extend(
        [
            "--proc",
            "/proc",
            "--tmpfs",
            "/proc/1",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/tmp/home",
        ]
        .map(Into::into),
    );
    for path in &policy.readable {
        if matches!(
            path.to_str(),
            Some("/dev/null" | "/dev/random" | "/dev/urandom")
        ) {
            continue;
        }
        mount(&mut args, "--ro-bind-try", path, path);
    }
    mount(&mut args, "--bind", &policy.root, &policy.root);
    mount(&mut args, "--bind", &policy.temp, &policy.temp);
    for path in &policy.writable {
        mount(&mut args, "--bind", path, path);
    }
    for path in &policy.read_only {
        mount(&mut args, "--ro-bind", path, path);
    }
    for hidden in &policy.hidden {
        if hidden.kind == HiddenKind::Directory {
            args.extend([OsString::from("--tmpfs"), hidden.path.as_os_str().into()]);
        } else {
            mount(&mut args, "--ro-bind", Path::new("/dev/null"), &hidden.path);
        }
    }
    if policy.network == Network::Approved {
        mount(
            &mut args,
            "--ro-bind-try",
            Path::new("/run/systemd/resolve"),
            Path::new("/run/systemd/resolve"),
        );
    }
    for (key, value) in &launch.environment {
        args.extend(["--setenv".into(), key.clone(), value.clone()]);
    }
    args.extend(["--chdir".into(), policy.root.as_os_str().into()]);
    for fd in &launch.seccomp_fds {
        args.extend(["--add-seccomp-fd".into(), fd.to_string().into()]);
    }
    args.extend([
        "--".into(),
        launch.bash.as_os_str().into(),
        "-c".into(),
        launch.command.clone(),
    ]);
    Ok(args)
}

fn validate_bwrap_environment(
    policy: &Policy,
    environment: &[(OsString, OsString)],
) -> Result<(), RootError> {
    let mut seen = std::collections::HashSet::new();
    for (key, value) in environment {
        let Some(name) = key.to_str() else {
            return Err(RootError::UnsafeEnvironment(
                key.to_string_lossy().escape_debug().to_string(),
            ));
        };
        let allowed = matches!(
            name,
            "PATH"
                | "LANG"
                | "LC_ALL"
                | "TERM"
                | "CARGO_HOME"
                | "RUSTUP_HOME"
                | "HOME"
                | "TMPDIR"
                | "GIT_OPTIONAL_LOCKS"
                | "GIT_CONFIG_GLOBAL"
                | "GIT_CONFIG_NOSYSTEM"
        );
        let expected = match name {
            "TERM" => Some(OsString::from("dumb")),
            "HOME" => Some(OsString::from("/tmp/home")),
            "TMPDIR" => Some(policy.temp.as_os_str().to_owned()),
            "GIT_OPTIONAL_LOCKS" => Some(OsString::from("0")),
            "GIT_CONFIG_GLOBAL" => Some(OsString::from("/dev/null")),
            "GIT_CONFIG_NOSYSTEM" => Some(OsString::from("1")),
            _ => None,
        };
        if !allowed || !seen.insert(name) || expected.is_some_and(|expected| *value != expected) {
            return Err(RootError::UnsafeEnvironment(name.to_owned()));
        }
    }
    Ok(())
}

fn mount(args: &mut Vec<OsString>, flag: &str, source: &Path, target: &Path) {
    args.extend([
        flag.into(),
        source.as_os_str().into(),
        target.as_os_str().into(),
    ]);
}

/// Produces a fresh environment; source PATH and all unspecified variables are discarded.
pub fn environment(
    source: impl IntoIterator<Item = (OsString, OsString)>,
    sandboxed: bool,
) -> Vec<(OsString, OsString)> {
    let mut result = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
    ];
    #[cfg(target_os = "macos")]
    {
        result[0].1 = "/Library/Developer/CommandLineTools/usr/bin:/usr/bin:/bin".into();
        result.push((
            "SDKROOT".into(),
            "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk".into(),
        ));
    }
    for (key, value) in source {
        if matches!(
            key.to_str(),
            Some("LANG" | "LC_ALL" | "CARGO_HOME" | "RUSTUP_HOME")
        ) {
            result.retain(|(old, _)| old != &key);
            result.push((key, value));
        }
    }
    if sandboxed {
        result.extend(
            [
                ("HOME", "/tmp/home"),
                ("TMPDIR", "/tmp"),
                ("GIT_OPTIONAL_LOCKS", "0"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
            ]
            .map(|(k, v)| (k.into(), v.into())),
        );
    }
    result
}
