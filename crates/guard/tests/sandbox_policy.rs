use elefante_guard::sandbox::{
    HiddenKind, HiddenPath, LaunchSpec, Network, Policy, bwrap_args, environment, seatbelt_profile,
    validate_root,
};
use std::{ffi::OsString, path::PathBuf};

fn policy() -> Policy {
    Policy {
        root: PathBuf::from("/work/project[1]"),
        temp: PathBuf::from("/scratch/session"),
        home: PathBuf::from("/users/alice"),
        harness_dir: PathBuf::from("/users/alice/.elefante"),
        protected: vec![],
        readable: vec![PathBuf::from("/usr")],
        writable: vec![],
        read_only: vec![PathBuf::from("/work/project[1]/.git")],
        hidden: vec![],
        network: Network::Off,
        runtime_dir: None,
    }
}

#[test]
fn roots_cannot_enclose_home_or_overlap_harness() {
    for root in [
        "/",
        "/users",
        "/users/alice",
        "/users/alice/.elefante",
        "/users/alice/.elefante/x",
        "/work/project/.git",
        "/work/project/.GIT/objects",
    ] {
        assert!(
            validate_root(
                root.as_ref(),
                "/users/alice".as_ref(),
                "/users/alice/.elefante".as_ref()
            )
            .is_err()
        );
    }
    assert!(
        validate_root(
            "/work/project".as_ref(),
            "/users/alice".as_ref(),
            "/users/alice/.elefante".as_ref()
        )
        .is_ok()
    );
    assert!(
        validate_root(
            "/work/project/.github".as_ref(),
            "/users/alice".as_ref(),
            "/users/alice/.elefante".as_ref()
        )
        .is_ok()
    );
}

#[test]
fn bubblewrap_keeps_its_private_devices_writable() {
    let mut p = policy();
    p.readable
        .extend(["/dev/null", "/dev/random", "/dev/urandom"].map(PathBuf::from));
    let launch = LaunchSpec {
        bash: "/bin/bash".into(),
        command: "true".into(),
        environment: vec![("TERM".into(), "dumb".into())],
    };
    let args = bwrap_args(&p, &launch).expect("rendered bubblewrap command");
    assert!(args.windows(2).any(|parts| parts == ["--dev", "/dev"]));
    for device in ["/dev/null", "/dev/random", "/dev/urandom"] {
        assert!(
            !args
                .windows(3)
                .any(|parts| parts == ["--ro-bind-try", device, device]),
            "{device} was covered by a read-only overlay"
        );
    }
}

#[test]
fn bubblewrap_masks_its_init_process_from_shell_inspection() {
    let p = policy();
    let launch = LaunchSpec {
        bash: "/bin/bash".into(),
        command: "ps eww $PPID".into(),
        environment: vec![("TERM".into(), "dumb".into())],
    };
    let args = bwrap_args(&p, &launch).expect("rendered bubblewrap command");
    assert!(
        args.windows(4)
            .any(|parts| parts == ["--proc", "/proc", "--tmpfs", "/proc/1"])
    );
}

#[test]
fn seatbelt_never_interpolates_paths_and_rejects_injection() {
    let p = policy();
    let (profile, params) = seatbelt_profile(&p).unwrap();
    assert!(!profile.contains("/work/project"));
    assert!(params.iter().any(|(_, value)| value == p.root.as_os_str()));
    assert!(profile.contains("(deny default)"));
    assert!(profile.contains(r#"regex #"/\.ssh(/|$)""#));
    assert!(profile.contains(r#"regex #"/\.aws/credentials(/|$)""#));
    assert!(profile.contains(r#"regex #"/\.cargo/credentials(\.toml)?(/|$)""#));
    assert_eq!(profile, include_str!("snapshots/seatbelt.sb"));
    for name in [
        "/work/quote\"",
        "/work/\\",
        "/work/\n",
        "/work/\")(allow default)",
    ] {
        let mut p = policy();
        p.root = name.into();
        assert!(seatbelt_profile(&p).is_err());
    }
}

#[test]
fn approved_seatbelt_network_does_not_grant_arbitrary_unix_sockets() {
    let mut policy = policy();
    policy.network = Network::Approved;
    let (profile, _) = seatbelt_profile(&policy).expect("approved profile");
    assert!(!profile.contains("(allow network*)\n"));
    assert!(profile.contains("(allow network-outbound (remote tcp) (remote udp)"));
    assert!(profile.contains("(literal \"/private/var/run/mDNSResponder\")"));
    assert!(profile.contains("(remote unix-socket (subpath (param \"TEMP\")))"));
}

fn traversal_paths(policy: &Policy) -> Vec<OsString> {
    let (profile, params) = seatbelt_profile(policy).unwrap();
    assert!(profile.contains("(allow file-read* (literal (param \"TRAVERSE_0\")))"));
    assert!(!profile.contains("(subpath (param \"TRAVERSE_"));
    params
        .into_iter()
        .filter(|(key, _)| key.starts_with("TRAVERSE_"))
        .map(|(_, path)| path)
        .collect()
}

#[test]
fn seatbelt_grants_literal_ancestors_only_of_mounted_paths() {
    let defaults = traversal_paths(&policy());
    for required in ["/", "/work", "/scratch"] {
        assert!(defaults.iter().any(|path| path == required), "{required}");
    }
    // The harness directory is never mounted, so its ancestors stay unlisted.
    for absent in ["/users", "/users/alice"] {
        assert!(!defaults.iter().any(|path| path == absent), "{absent}");
    }
    let mut p = policy();
    p.readable.push("/users/alice/.cargo/bin".into());
    p.hidden.push(HiddenPath {
        path: "/elsewhere/secret/file".into(),
        kind: HiddenKind::File,
    });
    p.protected.push("/sockets/run/daemon.sock".into());
    let paths = traversal_paths(&p);
    for required in ["/users", "/users/alice", "/users/alice/.cargo"] {
        assert!(paths.iter().any(|path| path == required), "{required}");
    }
    for absent in [
        "/elsewhere",
        "/elsewhere/secret",
        "/sockets",
        "/sockets/run",
    ] {
        assert!(!paths.iter().any(|path| path == absent), "{absent}");
    }
}

#[test]
fn seatbelt_denies_follow_every_allow() {
    let mut p = policy();
    p.writable = vec![p.root.clone()];
    p.readable.push("/work".into());
    let (profile, params) = seatbelt_profile(&p).unwrap();
    assert!(
        params
            .iter()
            .any(|(key, value)| key == "WRITE_0" && value == p.root.as_os_str())
    );
    let allow = profile
        .find("(allow file-write* (subpath (param \"WRITE_0\")))")
        .expect("writable root allow");
    let deny = profile
        .find(r#"(deny file-write* (require-all (subpath (param "ROOT")) (regex #"/\.[Gg][Ii][Tt](/|$)")))"#)
        .expect(".git write deny");
    assert!(allow < deny);
    let first_deny = profile.find("(deny file-").expect("a file deny");
    assert!(!profile[first_deny..].contains("(allow file-"));
}

#[test]
fn writable_entries_cannot_reopen_git_harness_or_masked_paths() {
    for path in [
        "/work/project[1]/.git",
        "/work/project[1]/.GIT/hooks",
        "/work/project[1]/sub/.elefante",
        "/work/project[1]/vendor/cache",
        "/work/project[1]/masked/cache",
        "/outside",
    ] {
        let mut p = policy();
        p.read_only.push("/work/project[1]/vendor".into());
        p.hidden.push(HiddenPath {
            path: "/work/project[1]/masked".into(),
            kind: HiddenKind::Directory,
        });
        p.writable.push(path.into());
        assert!(p.validate().is_err(), "{path}");
    }
    let mut p = policy();
    p.writable = [
        "/work/project[1]",
        "/work/project[1]/target",
        "/scratch/session/cache",
    ]
    .map(PathBuf::from)
    .into();
    assert!(p.validate().is_ok());
}

#[test]
fn read_only_mounts_outside_the_root_cannot_expose_protected_locations() {
    for path in [
        "/users/alice",
        "/users/alice/.ssh",
        "/users/alice/.elefante",
        "/run/user/1000",
    ] {
        let mut p = policy();
        p.read_only.push(path.into());
        assert!(p.validate().is_err(), "{path}");
    }
    let mut p = policy();
    p.read_only.push("/repos/main/.git".into());
    assert!(p.validate().is_ok());
}

#[test]
fn non_normalized_paths_are_refused() {
    for path in [
        "/work/project[1]//.elefante",
        "/work/project[1]/./.elefante",
        "/work/project[1]/.elefante/",
    ] {
        let mut p = policy();
        p.hidden.push(HiddenPath {
            path: path.into(),
            kind: HiddenKind::Directory,
        });
        assert!(seatbelt_profile(&p).is_err(), "{path}");
    }
    let mut p = policy();
    p.hidden.push(HiddenPath {
        path: "/work/project[1]/.elefante".into(),
        kind: HiddenKind::Directory,
    });
    assert!(seatbelt_profile(&p).is_ok());
    for root in ["/work/project/", "/work//project"] {
        assert!(
            validate_root(
                root.as_ref(),
                "/users/alice".as_ref(),
                "/users/alice/.elefante".as_ref()
            )
            .is_err(),
            "{root}"
        );
    }
}

#[test]
fn environment_drops_untrusted_variables() {
    let vars = environment(
        [
            (
                OsString::from("OPENROUTER_API_KEY"),
                OsString::from("synthetic"),
            ),
            (OsString::from("PATH"), OsString::from("/evil")),
        ],
        true,
    );
    assert!(
        !vars
            .iter()
            .any(|(key, value)| key == "OPENROUTER_API_KEY" || value == "/evil")
    );
    assert!(
        vars.iter()
            .any(|(key, value)| key == "TERM" && value == "dumb")
    );
}

#[test]
fn bubblewrap_snapshot_preserves_overlay_and_descriptor_order() {
    let mut p = policy();
    p.hidden = vec![
        HiddenPath {
            path: p.root.join(".elefante"),
            kind: HiddenKind::Directory,
        },
        HiddenPath {
            path: p.root.join(".env"),
            kind: HiddenKind::File,
        },
    ];
    let launch = LaunchSpec {
        bash: "/bin/bash".into(),
        command: "printf '%s' '$HOME'".into(),
        environment: vec![("TERM".into(), "dumb".into())],
    };
    let args = bwrap_args(&p, &launch).unwrap();
    let expected = [
        "--unshare-user",
        "--disable-userns",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--unshare-net",
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
        "--ro-bind-try",
        "/usr",
        "/usr",
        "--bind",
        "/work/project[1]",
        "/work/project[1]",
        "--bind",
        "/scratch/session",
        "/scratch/session",
        "--ro-bind",
        "/work/project[1]/.git",
        "/work/project[1]/.git",
        "--tmpfs",
        "/work/project[1]/.elefante",
        "--ro-bind",
        "/dev/null",
        "/work/project[1]/.env",
        "--setenv",
        "TERM",
        "dumb",
        "--chdir",
        "/work/project[1]",
        "--add-seccomp-fd",
        "3",
        "--add-seccomp-fd",
        "4",
        "--",
        "/bin/bash",
        "-c",
        "printf '%s' '$HOME'",
    ];
    assert_eq!(args, expected.map(OsString::from));
    p.network = Network::Approved;
    let approved = bwrap_args(&p, &launch).unwrap();
    assert!(!approved.iter().any(|value| value == "--unshare-net"));
    assert!(approved.windows(3).any(|values| {
        values
            == [
                "--ro-bind-try",
                "/run/systemd/resolve",
                "/run/systemd/resolve",
            ]
            .map(OsString::from)
    }));
}

#[test]
fn readable_mounts_cannot_reintroduce_secrets_or_daemon_paths() {
    for mount in [
        "/users/alice",
        "/users",
        "/users/alice/.ssh",
        "/users/alice/.elefante",
        "/run",
        "/tmp",
        "/outside/socket-dir",
        "/run/user/1000",
        "/run/dbus",
        "/run/systemd",
        "/var/run/docker.sock",
        "/private/var/run/mDNSResponder",
        "/tmp/.X11-unix",
        "/private/tmp/session",
        "/dev",
        "/dev/shm",
    ] {
        let mut p = policy();
        p.readable.push(mount.into());
        p.protected.push("/outside/socket-dir/custom.sock".into());
        assert!(p.validate().is_err(), "{mount}");
    }
    let mut p = policy();
    p.hidden.push(HiddenPath {
        path: "/users/alice/.cargo/credentials.toml".into(),
        kind: HiddenKind::File,
    });
    p.readable.push("/users/alice/.cargo".into());
    assert!(p.validate().is_err());
    p.readable.pop();
    p.readable.push("/users/alice/.cargo/bin".into());
    assert!(p.validate().is_ok());
    p.temp = p.root.join("scratch");
    assert!(p.validate().is_err());
    for allowed in ["/run/systemd/resolve", "/dev/null", "/dev/urandom"] {
        let mut p = policy();
        p.readable.push(allowed.into());
        assert!(p.validate().is_ok(), "{allowed}");
    }
}

#[test]
fn shared_temp_admits_only_validated_git_directories_and_root_paths() {
    let mut p = policy();
    p.root = "/tmp/worktree".into();
    p.read_only = vec!["/tmp/main/.git".into()];
    p.readable.push("/tmp/main/.git".into());
    assert!(p.validate().is_ok());
    p.readable.push("/tmp/worktree/vendor".into());
    assert!(p.validate().is_ok());
    p.read_only.clear();
    assert!(p.validate().is_err());
}

#[test]
fn readable_mounts_exclude_the_runtime_directory() {
    let mut p = policy();
    p.runtime_dir = Some("/outside/xdg-runtime".into());
    p.readable.push("/outside/xdg-runtime/bus".into());
    assert!(p.validate().is_err());
    p.readable.pop();
    // Positive control: a sibling that only shares the prefix text stays mountable.
    p.readable.push("/outside/xdg-runtime-other".into());
    assert!(p.validate().is_ok());
}

#[test]
fn session_environment_uses_fixed_path_and_private_temp() {
    let p = policy();
    let vars = p
        .environment(
            [
                (OsString::from("PATH"), OsString::from("/evil")),
                (OsString::from("SDKROOT"), OsString::from("/evil")),
            ],
            &[],
        )
        .unwrap();
    assert!(
        vars.iter()
            .any(|(key, value)| key == "TMPDIR" && value == p.temp.as_os_str())
    );
    assert!(vars.iter().any(|(key, value)| {
        key == "HOME"
            && value
                == if cfg!(target_os = "linux") {
                    OsString::from("/tmp/home")
                } else {
                    p.temp.join("home").into_os_string()
                }
                .as_os_str()
    }));
    assert!(!vars.iter().any(|(_, value)| value == "/evil"));
    assert!(p.environment([], &["/unmounted/bin".into()]).is_err());
    assert!(
        p.environment([], &["/usr/local/unlisted-bin".into()])
            .is_err()
    );
    let mut explicit = policy();
    explicit.readable.push("/opt/tool/bin".into());
    let with_tool = explicit.environment([], &["/opt/tool/bin".into()]).unwrap();
    assert!(
        with_tool
            .iter()
            .any(|(key, value)| key == "PATH" && value.to_string_lossy().contains("/opt/tool/bin"))
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use elefante_guard::sandbox::{Architecture, Network, seccomp_programs};

    // Small interpreter for exactly the classic BPF operations emitted by seccompiler.
    fn action(bytes: &[u8], arch: u32, syscall: u32, family: u64) -> u32 {
        let mut data = [0_u8; 64];
        data[..4].copy_from_slice(&syscall.to_ne_bytes());
        data[4..8].copy_from_slice(&arch.to_ne_bytes());
        data[16..24].copy_from_slice(&family.to_ne_bytes());
        data[24..32].copy_from_slice(&family.to_ne_bytes());
        let mut accumulator = 0;
        let mut pc = 0;
        for _ in 0..10_000 {
            let instruction = &bytes[pc * 8..pc * 8 + 8];
            let code = u16::from_ne_bytes(instruction[..2].try_into().unwrap());
            let k = u32::from_ne_bytes(instruction[4..].try_into().unwrap());
            let branch = match code {
                0x20 => {
                    let offset = usize::try_from(k).unwrap();
                    accumulator = u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap());
                    None
                }
                0x15 => Some(accumulator == k),
                0x25 => Some(accumulator > k),
                0x35 => Some(accumulator >= k),
                0x45 => Some(accumulator & k != 0),
                0x05 => {
                    pc += usize::try_from(k).unwrap();
                    None
                }
                0x54 => {
                    accumulator &= k;
                    None
                }
                0x06 => return k,
                _ => panic!("unsupported BPF opcode {code:x}"),
            };
            pc += 1 + branch.map_or(0, |yes| {
                usize::from(if yes { instruction[2] } else { instruction[3] })
            });
        }
        panic!("BPF program did not terminate");
    }

    #[test]
    fn filters_reject_wrong_arch_and_x32_and_enforce_every_syscall() {
        for (arch, audit, enosys, eperm, socket, ioctl) in [
            (
                Architecture::X86_64,
                0xc000_003e,
                vec![435, 425, 426, 427],
                vec![
                    272, 308, 165, 166, 155, 428, 429, 430, 431, 432, 433, 442, 101, 310, 311, 321,
                    298, 323, 250, 248, 249, 175, 313, 176, 246, 320,
                ],
                41,
                16,
            ),
            (
                Architecture::Aarch64,
                0xc000_00b7,
                vec![435, 425, 426, 427],
                vec![
                    97, 268, 40, 39, 41, 428, 429, 430, 431, 432, 433, 442, 117, 270, 271, 280,
                    241, 282, 219, 217, 218, 105, 273, 106, 104, 294,
                ],
                198,
                29,
            ),
        ] {
            for network in [Network::Off, Network::Approved] {
                let programs = seccomp_programs(arch, network).unwrap();
                assert_eq!(programs.len(), 2);
                for program in &programs {
                    assert_eq!(action(program, audit ^ 1, 0, 0) & 0xffff, 0);
                    assert_ne!(action(program, audit ^ 1, 0, 0), 0x7fff_0000);
                    assert_eq!(action(program, audit, 0, 0), 0x7fff_0000);
                    if arch == Architecture::X86_64 {
                        assert_eq!(action(program, audit, 0x4000_0000, 0), 0x8000_0000);
                    }
                }
                for syscall in &enosys {
                    assert_eq!(action(&programs[0], audit, *syscall, 0), 0x0005_0026);
                    assert_eq!(action(&programs[1], audit, *syscall, 0), 0x7fff_0000);
                }
                for syscall in &eperm {
                    assert_eq!(action(&programs[1], audit, *syscall, 0), 0x0005_0001);
                    assert_eq!(action(&programs[0], audit, *syscall, 0), 0x7fff_0000);
                }
                for family in [0, 1, 2, 3, 10, 16, 40] {
                    let allowed =
                        [2, 10, 16].contains(&family) || (family == 1 && network == Network::Off);
                    assert_eq!(
                        action(&programs[1], audit, socket, family),
                        if allowed { 0x7fff_0000 } else { 0x0005_0001 }
                    );
                }
                assert_eq!(action(&programs[1], audit, ioctl, 0x5412), 0x0005_0001);
                // The kernel ignores the high 32 bits of the request, so these must match too.
                assert_eq!(
                    action(&programs[1], audit, ioctl, 0x1_0000_5412),
                    0x0005_0001
                );
                assert_eq!(action(&programs[1], audit, ioctl, 0x5401), 0x7fff_0000);
            }
        }
    }
}
