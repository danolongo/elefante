#![forbid(unsafe_code)]
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
use std::{collections::BTreeMap, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).ok_or("scratch argument missing")?);
    let arch = match std::env::consts::ARCH {
        "x86_64" => TargetArch::x86_64,
        "aarch64" => TargetArch::aarch64,
        _ => return Err("unsupported architecture".into()),
    };
    let (unshare, clone3) = if cfg!(target_arch = "x86_64") {
        (272, 435)
    } else {
        (97, 435)
    };
    // Normally successful sentinels prove loading even when the host already
    // blocks a production syscall (notably clone3 on hosted CI).
    let (getpriority, sched_getscheduler) = if cfg!(target_arch = "x86_64") {
        (140, 145)
    } else {
        (141, 120)
    };
    for (name, syscalls, action) in [
        ("eperm", [unshare, getpriority], SeccompAction::Errno(1)),
        (
            "enosys",
            [clone3, sched_getscheduler],
            SeccompAction::Errno(38),
        ),
    ] {
        let filter = SeccompFilter::new(
            syscalls
                .into_iter()
                .map(|syscall| (syscall, vec![]))
                .collect::<BTreeMap<_, _>>(),
            SeccompAction::Allow,
            action,
            arch.clone(),
        )?;
        let program: BpfProgram = filter.try_into()?;
        let mut bytes = Vec::new();
        for instruction in program {
            bytes.extend(instruction.code.to_ne_bytes());
            bytes.push(instruction.jt);
            bytes.push(instruction.jf);
            bytes.extend(instruction.k.to_ne_bytes());
        }
        fs::write(root.join(format!("{name}.bpf")), bytes)?;
    }
    fs::write(root.join("regular"), b"fixture")?;
    std::os::unix::fs::symlink("regular", root.join("link"))?;
    assert_eq!(fs::read(root.join("link"))?, b"fixture");
    let traversal = format!(
        "../{}/outside",
        root.file_name()
            .ok_or("missing scratch name")?
            .to_string_lossy()
    );
    assert_eq!(
        fs::read(root.join(&traversal))?,
        b"positive traversal target"
    );
    let dir = rustix::fs::open(
        &root,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )?;
    let flags = rustix::fs::ResolveFlags::BENEATH | rustix::fs::ResolveFlags::NO_SYMLINKS;
    rustix::fs::openat2(
        &dir,
        "regular",
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
        flags,
    )?;
    assert!(rustix::fs::openat2(
        &dir,
        "link",
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
        flags
    )
    .is_err());
    assert!(rustix::fs::openat2(
        &dir,
        &traversal,
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
        flags
    )
    .is_err());
    println!("openat2 regular positive control passed; symlink and traversal rejected");
    Ok(())
}
