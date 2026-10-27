# M4 — Sandbox: gate implementation plan (reviewed)

## Context

`docs/PLAN.md` § M4 (commit 878474e) specifies the sandbox milestone. This plan turns it into
ordered, testable work, and records the decisions taken since then:

- **Linux uses bubblewrap + seccomp (2026-10-21).** Landlock can only *grant* access, so nothing
  inside a writable root can be protected: no read-only `.git`, no hidden `.elefante/`, no masked
  `.env`. Codex moved its Linux default to bubblewrap for the same reason. This drops three pieces
  of the committed design:
  - the `.git` snapshot-and-restore;
  - the `/proc/self/exe sandbox-exec` helper;
  - "loopback needs approval" on Linux, because the private network namespace has its own
    loopback.
- **Git hooks stay off** on commits the harness makes. Running them in the sandbox becomes a
  PLAN.md proposal.
- **The "second lock" (harness git under its own sandbox) becomes M4.1**, built after M4's gate is
  green. M4 protects harness git with a strict per-subcommand option allowlist. M4.1 makes that
  allowlist a second line of defence rather than the only one.
- **A contextless adversarial review (2026-10-21) changed this plan**; the
  [mapping](#review-findings--changes) is at the end.

**Before M4 (on `main`):**
- bash ran `bash -lc` with the daemon's full environment, no timeout, and uncapped output
  (`ToolExecutor::bash` in `crates/core/src/tools.rs`);
- the daemon socket authenticated nobody (`crates/elefante/src/daemon/server.rs`);
- `session.create` accepted any cwd, and `key.set` existed. A command that reached the socket
  therefore escaped fully.

**Outcome:** every bash call runs in an OS sandbox that fails closed. Inside it:
- no network (and on macOS not even the host's loopback);
- no secrets;
- no writable `.git`;
- no way to reach the daemon, other local services or the host GUI.

Git writes and web research go through harness-owned, allowlisted paths.

## Ground rules

- **Gates.** M4's hermetic gate plus the M0–M3 hermetic gates must be green before M4.1.
  Credentialed checks are deferred until the end of M4.1 at the maintainer's request; until
  then they are `not run`, not green. M4.1's gate, including those deferred checks, must be
  green before M5 or completion is reported.
- **Plan in the repo.** This file is M4's working plan. The pull request that closes M4.1
  deletes it, after lasting decisions move into `PLAN.md` and `DEVELOPER_GUIDE.md`.
- **Commits.** Commits land on `sandboxing` as each fix lands, following `DEVELOPER_GUIDE.md`
  § 13.
- **No `unsafe`.** The workspace forbids it. New dependencies:
  - `seccompiler`, Linux only, in `guard`: pure BPF compilation.
  - `rustix` 1.1.5 or later compatible release, in `core` and `mem`: safe `openat2` and
    `O_NOFOLLOW_ANY`. The macOS probe found that 1.1.4 lacks the latter flag.

  The spike checked both against MSRV 1.90.
- **Tests never skip silently.** A sandbox test that finds no sandbox fails with setup
  instructions, and CI provides the sandbox on both operating systems.
- **Every negative test has a positive control.** The same operation must succeed unsandboxed or
  with approval, so a test cannot pass because the operation was broken for another reason.

## Phase 0 — Amend PLAN.md, then spike (no product code)

### 0a. Amend `docs/PLAN.md`

**§ M4:**
- Linux becomes bubblewrap + seccomp.
- Remove the Landlock wording, the helper, and the detect-and-restore.
- Loopback:
  - macOS: needs approval (Seatbelt has no private loopback);
  - Linux: works inside the sandbox.
- Read git runs in the sandbox; write git runs in the harness with an option allowlist.
- New: root validation and the private temp directory.
- The threat model is narrowed. Code the user later chooses to run, such as their build or their
  IDE's tasks, is a residual, not a guarantee.

**New § M4.1 — Sandboxed harness git.**
- **Baseline:** write git runs under its own sandbox profile: network off, nothing outside the
  root readable, `.git` writable except `hooks/`, `config` and `info/`.
- **Gate:** a test-only extra flag added to the allowlist, such as `--output=$HOME/x`, is still
  blocked by the OS.

**Other updates:**
- **Proposals:** Landlock inside bubblewrap; hooks run in the sandbox; per-domain proxy; microVM.
- **Decisions → Isolation row:** updated.

Then copy this plan to `docs/M4_milestone.md`.

### 0b. Spike

The spike runs in scratch directories only. Its results go into a "Spike results" section of
`docs/M4_milestone.md`. If a result breaks a decision below, PLAN.md is amended before any
code is written.

| # | Question | Decides |
|---|---|---|
| S1 | ubuntu-24.04: does `bwrap` (0.9) work after `sysctl kernel.apparmor_restrict_unprivileged_userns=0`, and alternatively with the `bwrap-userns-restrict` AppArmor profile? Is `--disable-userns` honoured? | CI step; the setup text in the error; whether clone-flag masks are needed |
| S2 | Do `--seccomp 0` and `--add-seccomp-fd` load the filters when the BPF bytes arrive on stdin? What does the child see on fd 0? | Use repeated `--add-seccomp-fd` (including fd 0), because bubblewrap rejects combining it with `--seccomp`; the child sees fd 0 closed. No helper binary is needed. |
| S3 | Do these bubblewrap behaviours hold: ro-bind `.git` over an rw root, `--tmpfs` over hidden directories, `--ro-bind /dev/null` over hidden files, loopback up with `--unshare-net`, and orphans killed by `--unshare-pid --die-with-parent`? | bubblewrap argv renderer |
| S4 | What is the minimum toolchain set for `cargo build/test`, rustup proxies and `bun test`? Exact `PATH` dirs plus which read and write paths (e.g. `~/.cargo/.package-cache`, `~/.bun/install/cache`)? `$CARGO_HOME/credentials*` must stay hidden | Readable and writable lists |
| S5 | Seatbelt, written from scratch in Codex's style: do `cargo test`, `bun test` and in-sandbox `git status` pass on this repo? Refused or not: DNS, `~/.ssh`, writes to `.git` and `.GIT`, a hard link to `.git/config` or a `~/.ssh` fixture, loopback to a host listener, `ps eww $PPID`, `security find-generic-password`, `pbpaste`, `open -a`, `launchctl submit`, `osascript`. Does `-D`/`(param)` substitution work? | Seatbelt renderer; the hard-link fallback |
| S6 | seccompiler: does it reject x32 numbers on x86_64? With one action per filter, can stacked EPERM and ENOSYS programs load through bubblewrap? | Prepend arch + x32 checks if needed |
| S7 | `rustix` `openat2(RESOLVE_BENEATH\|RESOLVE_NO_SYMLINKS)` on Linux, and `O_NOFOLLOW_ANY` plus dirfd-relative `openat` on macOS, at MSRV 1.90? | Hardened file access |
| S8 | Walk cost on this repo: directories only, never pruned, looking for `.git` (any case); plus the name-based secret scan, pruning `target/` and `node_modules/` | Per-command or cached scan |
| S9 | OpenRouter `plugins:[{id:"web",engine:"exa",max_results}]` (credentialed; **the maintainer runs it at the end of M4.1**): stream shape, plain-text citations, cost per call | `web_search` backend; use documented shape and hermetic tests provisionally |
| S10 | Is `GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS=1` honoured by release git 2.50 (macOS) and the Ubuntu git? | Kept as an extra guard (the allowlist already refuses inexact options) |

### Spike results

These are observations from scratch probes, not implementation-gate results. Linux evidence is
from successful `ubuntu-24.04` Actions runs [39584938893](https://github.com/danolongo/elefante/actions/runs/39584938893)
and [39586238156](https://github.com/danolongo/elefante/actions/runs/39586238156). S9 ran live at the
maintainer's request in credentialed run [40486394912](https://github.com/danolongo/elefante/actions/runs/40486394912);
its probe lives in `crates/provider/tests/s9_web_plugin.rs`.

| Question | Observed result | Decision or follow-up |
|---|---|---|
| S1, Linux | On `ubuntu-24.04`, bubblewrap 0.9.0 failed before setup and succeeded after `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`. The runner had no `/etc/apparmor.d/bwrap-userns-restrict` profile, so that alternative was not run. With a fresh `/proc`, a nested user namespace succeeded without `--disable-userns` and failed with it. | Use the sysctl step in CI and keep `--disable-userns`. Document the missing-profile alternative as not run, rather than claiming it failed. |
| S2, Linux | Bubblewrap 0.9.0 rejects `--seccomp 0` combined with `--add-seccomp-fd`. `--seccomp 0` alone loads a filter from stdin. Repeated `--add-seccomp-fd`, one pointing at fd 0, loads stacked EPERM and ENOSYS programs: normally successful syscall sentinels changed to those errors. The child sees fd 0 closed (`EBADF`), while the unsandboxed observer read eight control bytes. | Use repeated `--add-seccomp-fd` and set the bash child's stdin to `/dev/null` explicitly. Safe Rust cannot map extra descriptors into a child, so a fixed `/bin/sh` trampoline reopens the daemon's close-on-exec memory files as bubblewrap's descriptors (see 2b). No compiled helper is needed. |
| S3, Linux | Overlaying `.git` read-only over a writable root, masking a hidden directory with `--tmpfs`, masking a file with `/dev/null`, private loopback, and PID-namespace orphan cleanup all passed with positive controls on the corrected runner. | Keep the planned bubblewrap argument order. |
| S4, Linux | On `ubuntu-24.04`, exact `PATH` directories `/usr/bin`, `/bin`, the runner's Cargo bin and Bun bin worked. Read-only mounts of `/usr`, `/bin`, `/lib`, `/lib64`, `/etc/ld.so.cache`, `/etc/alternatives`, `/etc/ssl/certs`, the exact Cargo/Bun bin directories, selected Rust 1.90 toolchain, rustup settings file and Cargo registry supported a fresh target build and workspace test, plus `bun test` (97 tests). Writable paths were the root and private scratch home/cache under `/tmp`. A synthetic Cargo credentials file was visible outside and masked inside. A read-only registry warmed outside the sandbox was used; network installation and cold registry download were not tested. | Bind those exact system/toolchain paths and the dependency registry, keep Cargo credentials masked, and put writable cache files under the private temp directory. Do not bind the whole real home or Cargo home. The cold target result proves compilation, not installation without a warmed dependency cache. |
| S5, macOS | A scratch deny-default Seatbelt profile ran `git status` and all 97 Bun tests. Network-off `cargo test --workspace --offline --no-fail-fast` had 543 passed and 10 loopback-bind failures; with approved network and identical file rules, 553 passed, 0 failed and 6 ignored. Off blocked DNS, a host listener, SSH and secret fixtures, `.git`/`.GIT` writes, protected hard links, process inspection, pasteboard, `open -a`, launchd submission, a synthetic keychain lookup and a Finder Apple Event; the matching unsandboxed operations succeeded. `-D` substitution worked, but Seatbelt itself accepted quote, backslash and newline values. `/usr/bin/clang` wrote assembly temporaries outside the private temp; direct Command Line Tools clang with fixed `SDKROOT` built successfully. | Keep host loopback behind network approval; the repository test suite needs approval on macOS. Put the direct compiler directory first in fixed `PATH`, pass fixed `SDKROOT`, and reject hostile `-D` values in the renderer. |
| S6, Linux | `seccompiler` 0.5.0 builds at Rust 1.90. Its generated filters killed wrong-architecture calls but evaluated x32 syscall numbers as `ALLOW`; x32 `getpid` returned ENOSYS with and without a filter. Stacked EPERM and ENOSYS filters loaded through bubblewrap, proven by normally successful syscall sentinels; `clone3` alone was inconclusive because the host already returns ENOSYS. | Prepend an explicit x32-number rejection on x86_64 and test both programs' architecture and syscall actions. |
| S7, Linux | `rustix` 1.1.5 and `seccompiler` 0.5.0 built with Rust 1.90. `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` opened a normal file and rejected symlink and parent traversal. | Use root-descriptor-relative `openat2` with both resolve flags. |
| S7, macOS | On arm64 macOS 15.7.9 with Rust 1.90, locked `rustix` 1.1.4 fails to compile `OFlags::NOFOLLOW_ANY`; 1.1.5 builds offline. A dirfd-relative open of an ordinary file succeeds and intermediate/final symlinks fail with `ELOOP`. `openat(rootfd, "../outside", NOFOLLOW_ANY)` succeeds. | Require `rustix` 1.1.5 or later compatible release. Reject absolute and parent components separately; `NOFOLLOW_ANY` alone does not enforce containment. |
| S8, this checkout | Metadata-only unpruned `.git` walk: 4,343 directories and 130,997 entries, one `.git`, median 232 ms over five warm runs; first cold run about 1.2 s. Name-only secret scan pruning `target/` and `node_modules/`: 319 directories and 7,315 entries, median 18 ms. No secret contents were opened. | Keep the unpruned `.git` scan before each command for correctness, and prune only the secret-name scan. The per-command latency is a measured cost; the production Rust walk must be checked later. |
| S9, live | With `anthropic/claude-sonnet-5` and three results, the documented plugin request streamed 15 events and `[DONE]`. Citations arrive as `delta.annotations[].url_citation` (url, title, content, indexes) and as plain Markdown links in the text. `usage.cost` was $0.0090 for the raw probe and $0.0096 through the daemon's backend; no separate web-search cost field came back. The backend's answer listed the three sources but did not state the asked date. | The documented shape holds and the text-only backend keeps working. Whether `usage.cost` includes the Exa fee is checked in OpenRouter Activity before choosing a cheaper search default. |
| S10, Linux | Ubuntu runner Git 2.55.0 accepted exact `--porcelain` and refused abbreviated `--porc` with `GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS=1` (exit 128). | Keep the environment guard alongside the exact option allowlist. |
| S10, macOS | Apple Git 2.50.1 accepts `status --porc` without `GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS=1` but exits 128 with it; exact `--porcelain` succeeds either way. | Keep the variable as a supplemental guard. The exact option allowlist remains primary. |

The S5 keychain control used an explicit scratch keychain and synthetic item: the identical
lookup exited 0 outside Seatbelt and 44 inside. Finder's version Apple Event exited 0 outside
and failed with `-1728` inside. No real keychain item or TCC setting was touched.

**Exit:** the selected implementation routes in S1–S8 and S10 are answered and recorded, and
PLAN.md is amended where a decision moved. The optional S1 AppArmor-profile alternative is
`not run` because the Ubuntu runner did not provide that profile; the chosen sysctl route was
tested. S9 remains `not run` under the explicit credentialed deferral above. Phase 1 may then
begin; the deferred S9 check remains a required final gate.
**First commit (Actions route):** `ci: probe Linux sandbox requirements`. This carries the
temporary workflow, spike programs and design amendment.
**Trigger fix:** `fix(ci): run the sandbox spike on branch pushes`. GitHub's manual dispatch
requires the workflow to exist on the default branch, so the temporary workflow must run on
pushes to `sandboxing` instead.
**Further CI iterations:** fix the temporary probe if its own setup fails. Do not treat a
probe-harness failure as a sandbox result.
**Results commit:** `docs: switch the Linux sandbox to bubblewrap and record the spike`.

**Recorded commits.** fd50d37 and 0843aca carry the first two subjects above. The CI iterations
are ffc3d4f `fix(ci): make sandbox spike controls exercise the intended paths`, d420cc6
`fix(ci): expose compiler links and CA roots in sandbox spike` and 464169a `test(ci): verify
sandbox toolchains from a cold build`. 1e76818 carries the results subject.

---

## Phase 1 — `guard`: pure policy (renderers, git router)

Everything here is pure and tested by rendering. The classifier shrink moves to Phase 2, so no
intermediate commit is weaker than today.

### 1a. `guard::sandbox` (new module)

```rust
pub struct Policy {
    root: PathBuf,           // canonical, validated (below)
    home: PathBuf,           // canonical home for readable-mount validation
    harness_dir: PathBuf,    // user-level harness state, never readable
    protected: Vec<PathBuf>, // exact socket and token paths, including overrides
    temp: PathBuf,           // private per-session dir, 0700, outside root and ~/.elefante
    readable: Vec<PathBuf>,  // system dirs + exact PATH dirs + S4's toolchain set
    writable: Vec<PathBuf>,  // root, temp, S4's exact cache files
    read_only: Vec<PathBuf>, // every .git (any case) + validated gitdir/commondir + in-root core.hooksPath / include targets
    hidden: Vec<HiddenPath>, // typed file/dir masks for harness state and credentials
    network: Network,        // Off | Approved
}
pub struct HiddenPath { path: PathBuf, kind: HiddenKind } // File | Directory
pub fn validate_root(root, home, harness_dir) -> Result<(), RootError>;
pub fn seatbelt_profile(&Policy) -> Result<(String, Vec<(String, OsString)>), RootError>; // profile + -D params
pub fn bwrap_args(&Policy, &LaunchSpec) -> Result<Vec<OsString>, RootError>;
pub fn seccomp_programs(arch, network: Network) -> Result<Vec<Vec<u8>>, _>;
pub fn environment(source, sandboxed: bool) -> Vec<(OsString, OsString)>;
// Policy::environment adds the exact PATH dirs and per-session HOME/TMPDIR.
```

**Root validation.** The root is refused when it is `/`, `$HOME`, an ancestor of `$HOME`, a path
containing or contained by `~/.elefante`, or a path with a case-insensitive `.git` component.
A refused root makes the session's sandbox
`Unavailable`, so bash refuses.

**The readable set never contains:**
- `$HOME` itself, `~/.ssh` or `~/.elefante`;
- a tool root (exact PATH dirs only);
- `/run` (apart from `/run/systemd/resolve`), `/var/run`, `/private/var/run`, `$XDG_RUNTIME_DIR`
  or `/dev` (apart from `/dev/null`, `/dev/random` and `/dev/urandom`), or a path under them;
- the host `/tmp` or `/private/tmp`, or a path under them, unless that path lies in the root or
  is also a `read_only` entry (a validated git directory);
- the socket or token path. The renderer asserts this.

`read_only` entries outside the root pass the same checks. Every path must use normalized
spelling, with no `//`, `/./` or trailing `/`, because Seatbelt never matches such a spelling.
Linux includes only the exact resolver files `/etc/resolv.conf`, `/etc/nsswitch.conf` and
`/etc/hosts`, so approved DNS can use libc without exposing the rest of `/etc`; Off still has
an isolated network namespace.

**Seatbelt** (deny default):
- **No path is pasted into the profile text.** Every path is a `-D KEY=value` parameter referenced
  with `(param "KEY")`. A path containing a quote, backslash or control character is refused. Only
  constant regexes are used, so there is nothing to regex-quote.
- **Writes:** allowed under `ROOT` and `TEMP`, plus an exact literal `/dev/null` exception
  needed by sandboxed Git reads using the cleared `GIT_CONFIG_GLOBAL=/dev/null`. No other
  out-of-root write path is granted. Denied: `(require-all (subpath ROOT)
  (regex #"/\.[Gg][Ii][Tt](/|$)"))`, `.elefante`, and each `read_only` entry. A `writable` entry
  must lie under `ROOT` or `TEMP`, contain no `.git` or `.elefante` component, and lie under no
  `read_only` or `hidden` entry.
- **Rule order:** Seatbelt applies the last matching rule, so the profile emits every allow
  before every deny. A later allow therefore cannot cancel a `.git`, secret-name or hidden deny.
- **Reads:** allowed for the readable set; denied for `hidden` and secret-name regexes under `ROOT`.
  `/private/etc` and narrowly required `/private/var` descendants (for example `/private/var/db`)
  are listed explicitly; the host's whole `/private/var` and shared temp directories are not.
  The ancestors of the root, temp, readable and writable entries get exact `literal` read
  grants, never `subpath` grants. A literal grant lets a command list the names in that
  directory, but grants no read of the entries' contents. The S5 probe needed literal reads of
  `/`, `/Users` and `$HOME` for process startup. `$HOME` gets one when the root or a toolchain
  directory lies beneath it.
- **Network:**
  - Off: **no** loopback; Unix sockets only under `TEMP` and `ROOT`.
  - Approved: outbound TCP/UDP and the exact system mDNSResponder socket; Unix sockets remain
    limited to `TEMP` and `ROOT`. A broad `allow network*` would expose daemon sockets.
- **Mach services:** a short allowlist from S5.
- **Denied:** `process-info*` on other processes, Apple Events, and the pasteboard, keychain and
  launchd submission (S5).

**bwrap argv:**
- `--unshare-user --disable-userns --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup-try`
- `--unshare-net` unless `Approved`
- `--die-with-parent --new-session --clearenv`
- `--proc /proc --tmpfs /proc/1 --dev /dev --tmpfs /tmp --dir /tmp/home`; PID 1 is
  bubblewrap's minimal reaper, not the host parent, and its process details are hidden from tools
- `--ro-bind-try` for each readable path except `/dev/null`, `/dev/random` and `/dev/urandom`,
  which `--dev /dev` already supplies writable; then `--bind <root> <root>`
- then, layered over the root: `--ro-bind` for each `read_only`; `--tmpfs` over hidden
  directories; `--ro-bind /dev/null` over hidden files
- under `Approved`, also `--ro-bind-try /run/systemd/resolve`
- `--setenv` for each allowlisted variable, `--chdir <root>`, repeated
  `--add-seccomp-fd <fd>` for the two filters, `--`, `<bash> -c <cmd>`

The renderer fails closed unless it receives two distinct inherited seccomp descriptors above
stderr and only allowlisted environment variables. The caller opens and passes the descriptors;
the renderer performs no filesystem lookup.

`/run` (apart from that one directory), `/var/run`, the host `/tmp`, `$XDG_RUNTIME_DIR` and
`$HOME` are never bound.

**seccomp:** two stacked programs, because seccompiler allows one action per filter.

| Program | Calls | Why |
|---|---|---|
| ENOSYS | `clone3`, `io_uring_setup/enter/register` | Runtimes fall back to `clone` and plain I/O |
| EPERM | `unshare`, `setns`, the mount family, `ptrace`, `process_vm_readv/writev`, `bpf`, `perf_event_open`, `userfaultfd`, `keyctl`, `add_key`, `request_key`, module loading, `kexec_*`, `ioctl(TIOCSTI)` (the rule compares the low 32 bits of the request, as the kernel does) | Block escape, terminal injection and other-process access |
| EPERM | `socket()` for any family except `AF_UNIX`, `AF_INET`, `AF_INET6`, `AF_NETLINK` | `AF_VSOCK` is blocked |
| EPERM, only under `Approved` | `socket(AF_UNIX)` | The host network namespace exposes abstract Unix sockets (X11, D-Bus); the DNS stub is UDP |

Both programs carry an architecture check and, on x86_64, an explicit x32 syscall-number check.
Clone-flag masks are added only if S1 shows `--disable-userns` is unavailable.

**Environment.**
- Always: `PATH` (fixed: the system dirs plus the exact readable PATH dirs), `LANG`, `LC_ALL`,
  `TERM=dumb`, `CARGO_HOME`, `RUSTUP_HOME`; on macOS, a fixed `SDKROOT` and the resolved
  Command Line Tools compiler directory ahead of `/usr/bin` in `PATH`.
- Sandboxed only: `HOME` (Linux `/tmp/home`, macOS `TEMP/home`), `TMPDIR` (the temp dir),
  `GIT_OPTIONAL_LOCKS=0`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`.
- `Disabled` only: the host `HOME`, the host `CARGO_HOME` and `RUSTUP_HOME` when set, and the
  toolchain directories ahead of the system directories in `PATH`.
- Nothing else ever, including `OPENROUTER_API_KEY`.

**Binaries.** The daemon resolves `bash`, `git` and `bwrap` from fixed system directories
(`/opt/homebrew/bin`, `/usr/local/bin`, `/usr/bin`, `/bin`). No environment variable overrides
them. `session.create` refuses a resolved path inside the session root.

### 1b. `guard::git` (new module): write-git router

```rust
pub enum GitRoute { NotHarness, Harness(GitWrite), Refused(&'static str) }
pub fn route(source: &str, root: &Path) -> GitRoute;
pub fn invocation(git: &Path, root: &Path, w: &GitWrite, id: &Identity) -> Invocation;
pub fn hardened(git: &Path, root: &Path) -> Invocation;   // base for mem/cochange.rs
```

**Which commands route.**
- Only a single plain `git <write-subcommand>` routes. Its words must be plain words or
  single-quoted strings, with no `$'…'`, `~`, glob, brace, expansion, substitution, redirection,
  pipe, list or env prefix. That way the command shown for approval and the argv executed are the
  same.
- A write subcommand in any other shape is `Refused` ("run git write commands on their own").
  That includes a write behind `env`, `command`, `exec`, `nice`, `nohup`, `time`, `timeout`,
  `xargs`, `sudo` or `builtin`.
- Read subcommands are **not** routed: they run in the sandbox like any bash command.
- The write subcommands are the table below plus `reset`, `clean`, `push`, `fetch`, `pull` and
  `config`. Every other subcommand is a read, such as `grep`, `blame`, `remote -v` or an alias.
  The listing forms are reads too: `branch` or `tag` with only `-a`, `-l`, `--list` or `-v`,
  `stash list|show`, `config --get|--get-all|--list|-l`, and `git --version`.
  The router and the bash classifier share these two lists.

**Per-subcommand allowlist.** Each option must match exactly; anything else is refused, which
covers abbreviations too.

| Subcommand | Allowed |
|---|---|
| `add` | `-A`, `-u`, `--`, paths |
| `rm` | `--cached`, `-r`, `--`, paths |
| `mv` | paths |
| `restore` | `--staged`, `--worktree`, `--`, paths |
| `commit` | `-m <msg>`+ (required), `-a`, `--amend`, `--no-edit` |
| `switch` | `-c <ref>`, `<ref>` |
| `checkout` | `-b <ref>`, `<ref>` that names no existing path; the harness appends `--` |
| `branch` | `<ref>`, `-d <ref>` |
| `stash` | `push [-m <msg>]`, `pop`, `apply` |
| `tag` | `<ref>`, `-a <ref> -m <msg>` |
| `merge` | `<ref>`, `--no-ff`, `--ff-only`, `-m <msg>`, `--abort` |
| `cherry-pick` | `<rev>`, `--abort`, `--continue` |
| `rebase` | `<ref>`, `--abort`, `--continue` |

**Argument checks.**
- Refs match `^[A-Za-z0-9._/-]+$` and do not start with `-`.
- Paths are refused if they use pathspec magic (`:` at the start), or if they fail
  `PathPolicy::resolve_for_write` (inside the root, never `.git` in any case).
- Messages are single argv elements.

**Verdicts are unchanged from today:** writes `Escalate`, `reset` and `clean` `Deny`, everything
else refused.

**Invocation.**
- Always runs the absolute git binary.
- Prefix flags: `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.pager=cat
  -c core.editor=false -c sequence.editor=false -c protocol.allow=never -c credential.helper=
  -c commit.gpgSign=false -c tag.gpgSign=false -c gpg.program=false -c gpg.ssh.program=false
  -c core.sshCommand=false -c diff.external= -c core.attributesFile=/dev/null
  -c merge.default=text`.
- Environment cleared to: the fixed `PATH`, `LANG`, `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_ATTR_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`,
  `GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS=1` (per S10), `GIT_CEILING_DIRECTORIES`, and
  `GIT_AUTHOR_*` / `GIT_COMMITTER_*` from `Identity`.

**Preconditions, checked by the caller (Phase 2).** Harness git refuses when:
- `root/.git` is absent or names a different git directory than at session start. Harness git
  checks only `root/.git` and never searches parent directories;
- the local `config` or `config.worktree`, or any file they include, has `include.path` or
  `includeIf` targets, or `core.hooksPath`, inside the root or written with `~/`;
- any worktree or indexed `.gitattributes`, or `.git/info/attributes`, activates a custom
  `filter=` or `merge=` driver. Git can execute these during `add`, `checkout`, `merge` and related
  writes. The scan is unpruned and includes the index fallback for a missing worktree file.
  This covers Git LFS too: skipping its global config would otherwise store full blobs.
- local repository config defines an executable `filter.*` command or `merge.*.driver`, even
  when current attributes do not select it: a target revision checked out by the agent may
  introduce attributes that activate the driver.

**`mem/src/cochange.rs:280`** switches to `hardened` with the absolute git path.

### Phase 1 gate

- Render snapshot tests for both renderers, including hostile names (quotes, `")(allow default)`,
  newlines, regex metacharacters in the root). The profile text never contains a path.
- `validate_root` table test.
- Seccomp program tests: architecture check, the S6 outcome, and the call lists per network mode.
- `route` table test (~80 cases):
  - every allowlist entry;
  - abbreviations (`--amen`), `--output`, `-S`, `-s`, `--exec`, `$'…'`, `~`, globs, pathspec
    magic;
  - `.GIT/config` paths;
  - write commands inside pipes and lists.
- `invocation` flag tests, and `environment` never contains `OPENROUTER_API_KEY`.

**Commit:** `feat(guard): render sandbox profiles and route git writes`.

**Recorded commits.** 7fc0b99 carries that subject. The separately approved gate fix is 62394d4
`fix(guard): keep Linux seccomp policy lint-clean`.

---

## Phase 2 — `core`: run every bash call in the sandbox

### 2a. `core::sandbox`

```rust
pub enum Sandbox {
    Seatbelt,
    Bubblewrap { bwrap: PathBuf },
    Disabled,             // ELEFANTE_SANDBOX=0
    Unavailable(String),  // reason + setup
}
```

**Startup.** `detect` and `probe` run once per daemon. The probe runs `true` under the full
profile.

**Per session:** `validate_root` (failure gives `Unavailable` for that session), the private temp
dir (created 0700, removed at close, swept at daemon start), and the resolved binaries.

**What each state does:**
- `Unavailable` makes every bash call fail with `sandbox unavailable: <reason>`. Only a missing
  mechanism adds setup advice: on Linux, install bubblewrap and enable unprivileged user
  namespaces; on every OS, set `ELEFANTE_SANDBOX=0` to opt out. A refused root gets no advice.
- `Disabled` makes **every bash call `Escalate`** (rule `sandbox-off`), because the classifier
  rules deleted in 2d no longer guard it. The environment allowlist, timeout and output cap still
  apply.

**Root scan, before each command.**
- Find every `.git` entry, in any case: a directories-only walk that is never pruned.
- Validate `gitdir`/`commondir` targets. They must point inside the root or into the main
  repository's git dir; otherwise the target is not bound and git there fails.
- Read each repository's local `config` and `config.worktree`, following includes. Every
  `hooksPath` or include target inside the root becomes read-only. A missing target, or one
  reached through a symlink inside the root, refuses the call until the user creates it or
  removes the setting.
- Refuse any `.git` entry that was absent at session start, naming its path, until the user
  removes it. Harness git runs the same scan, so it refuses too.
- Run the name-based secret scan (pruned per S8).

The results feed `read_only` and `hidden`.

**Linux, after each command:** a new `.git` (any case) anywhere in the root is moved to
`~/.elefante/quarantine/<session>/<n>/` and reported. If a move fails, the next root scan finds the
entry and refuses.

**Per-root `RwLock`.** Sandboxed bash takes it shared; harness git takes it exclusive and runs the
post-scan first. Parallel bash calls stay parallel.

### 2b. Bash runner (rewrite `ToolExecutor::bash`)

**Launch:**
- Every state starts the fixed `bash` with a launcher script. The script closes every descriptor
  above stderr that the daemon inherited, then execs the next program.
- macOS: the launcher execs `/usr/bin/sandbox-exec -p <profile> -D… <bash> -c <cmd>`.
- Linux: `<bash> -c <launcher> <bwrap> /proc/<daemon>/fd/<a> /proc/<daemon>/fd/<b> <args>`.
  The daemon writes the two seccomp programs into anonymous memory files with close-on-exec set.
  The launcher reopens them as fds 3 and 4 and execs bubblewrap with
  `--add-seccomp-fd 3 --add-seccomp-fd 4` (per S2). A failed reopen stops the `exec`. The daemon
  never writes a program at a path the sandbox can reach and never holds an inheritable
  descriptor, so concurrent spawns inherit nothing. Bash receives `/dev/null` on stdin.
  (The S2 row above names a `/bin/sh` trampoline; f1d745a and 022c503 replaced it with this
  `bash` launcher.)
- In every case: the environment comes from `guard::sandbox::environment`, there is a process
  group, and `kill_on_drop` is set.

**`CappedOutput`.**
- Reads stdout and stderr concurrently, keeping the head and a tail ring buffer, and records the
  total size.
- Fits both streams into one 32 KiB budget. A stream that needs at most half keeps all of it, and
  the other stream gets the rest. Each clipped stream's marker states its original size.
- Stops reading 2 s after bash exits. A leftover process holding the pipe can no longer stall the
  turn until the timeout.
- Replaces `wait_with_output`.

**Timeout.** `ELEFANTE_BASH_TIMEOUT_SECS` (default 120). On expiry, the process-group guard in
`core/src/sandbox.rs` kills the group. On Linux the PID namespace also takes down `setsid`
orphans.

**Input and output.**
- Input: optional `network: bool`.
- Output cap: `run_process` fits bash output into 32 KiB. `clip_middle` in `tools.rs` caps grep
  and web_search at 32 KiB and read at 128 KiB, and clips any growth from scrubbing.
- When stderr shows EROFS on `.git` (from a read-only git command that tried to write), one hint
  is appended: "run git write commands on their own".

### 2c. Harness git

`ToolExecutor::git(GitWrite)`:
- checks the 1b preconditions;
- takes the root lock exclusively and runs the root scan;
- runs `invocation`;
- applies the same cap, scrubber and `ELEFANTE_BASH_TIMEOUT_SECS` timeout as bash.

### 2d. Gate changes and classifier shrink (`core/src/lib.rs:195` `check_in`; `guard/src/bash.rs`)

**For `bash`, in order:**
1. `Disabled`: `Escalate (sandbox-off)`.
2. `guard::git::route`:
   - `Refused`: `Deny`;
   - `Harness`: `Escalate`, then `tools.git` runs it.
3. The classifier.
4. `network: true`: any non-denied shell command becomes `Escalate (network-requested)`;
   `Deny` stays `Deny`. Harness Git keeps its own approval and never gets network. This also
   covers an otherwise unclassified `curl`, so one approved request can actually fetch.

**For `web_search`:** `Allow` up to the per-session cap, then `Escalate`. The actual
`execute_calls` path uses `check_with_tools`, layered over `check_in`, so it can reserve against
the executor's shared session counter before concurrent calls start; `check_in` remains the pure
bash policy entry point.

**Approval events** carry a structured `network: bool` field, not a prefix in `command`. An
approved escalation still runs sandboxed; only `network-requested` switches to
`Network::Approved`.

**Classifier shrink (lands with this phase):**
- Delete `names_harness_dir_by_glob`, `searches_harness_state`, and the `GREP_*`/`RG_*` tables.
- Keep the literal secret-path `Deny`, destructive and privileged `Deny`, and
  `environment-disclosure`.
- A directory-only `cd` is an explicit exception to literal secret-path `Deny`, so the former
  `cd .elefante && cat x` bypass reaches the OS test; literal content readers remain denied.
  Only the read-only `sqlite3 <db> .schema` form and exact `xargs cat` join the intent allowlist.
  `Disabled` still escalates every Bash call before the classifier runs.
- In `known_gaps()`, the four harness-state entries become "enforced by the OS sandbox".
- The asserting tests in `crates/guard/tests/bash_classifier.rs` become sandbox tests. The
  table below maps old to new; it is the written renegotiation of those M0/M3 gate items.

**Old → new test mapping.** The left column names the test on `main`. The OS column names the
sandbox test that now carries the assertion. Every sandbox test there pairs the blocked read with
an unsandboxed control and an ordinary-file control.

| Test on `main` | Classifier test now | Old verdict → new | OS test |
|---|---|---|---|
| `denies_final_component_and_glob_spellings_of_harness_state`, literal forms (`cat .elefante`, `grep -ra x .elefante`, `ls -a .elefante`) | `denies_literal_harness_state_paths` | `Deny` → `Deny` | Not needed: the classifier still denies. |
| the same test, glob forms (`cat .elef*/memory.db`, `.[e]lefante`, `.*`, `sqlite3 .elef*/memory.db .schema`) | `leaves_harness_glob_expansion_to_the_os_sandbox`, with `data*` controls | `Deny` → `Allow` | None runs a glob spelling. `former_harness_state_bypasses_reach_the_os_boundary` (`crates/core/tests/sandbox.rs`) exercises the name rule that hides `.elefante/`. |
| `escalates_recursive_content_readers_over_the_workspace_root` (`grep -r x .`, `rg -uuu x`, rule `recursive-search-includes-harness-state`) | `leaves_recursive_reader_path_enforcement_to_the_os_sandbox` | `Escalate` → `Allow` (`read-only-allowlist`) | None runs a recursive reader over a root that holds `.elefante/`. The same name rule applies. |
| `recursive_readers_stay_allowed_when_they_cannot_reach_harness_state` | `recursive_readers_allow_ordinary_paths_and_exclusions` | `Allow` → `Allow` | Not needed. |
| `documents_remaining_harness_state_gaps`: `D=.elefante; cat $D/memory.db` | `documents_harness_state_paths_enforced_by_the_os_sandbox` | `Allow` → `Allow` | `former_harness_state_bypasses_reach_the_os_boundary` |
| the same test: `cd "$D" && cat memory.db` | the same test, plus `allows_narrow_runtime_path_readers_with_ordinary_path_controls` | `Escalate` → `Allow` | `former_harness_state_bypasses_reach_the_os_boundary` (`cd .elefante && cat x`) |
| the same test: `sqlite3 "$DB" .schema` | the same two tests | `Escalate` → `Allow` | `assembled_sqlite_path_cannot_read_harness_state` |
| the same test: `echo "$P" \| xargs cat` | the same two tests | `Escalate` → `Allow` | `former_harness_state_bypasses_reach_the_os_boundary` (`… \| xargs cat`) |
| `classifies_full_fixture_table`: `rg --hidden x` | the same test | `Escalate` → `Allow` | As the recursive-reader row. |
| `workspace_under_an_elefante_directory_classifies_normally`: `grep -r x .` | the same test | `Escalate` → `Allow` | As the recursive-reader row. |

Each gap that `documents_harness_state_paths_enforced_by_the_os_sandbox` checks must now say
"enforced by the OS sandbox" in `known_gaps()`. `allows_narrow_runtime_path_readers_with_ordinary_path_controls`
keeps `Escalate` for every other `cd`, `sqlite3` and `xargs` form, such as `sqlite3 "$DB" .shell`
and `xargs rm`.

### 2e. Hardened file access, in the daemon and the indexer

`read`, `write`, `edit`, `glob`, `grep` and `mem/src/hash.rs` (`hash_files`):
- **Open relative to a root directory descriptor** (S7): `openat2(RESOLVE_BENEATH |
  RESOLVE_NO_SYMLINKS)` on Linux; dirfd `openat` with `O_NOFOLLOW_ANY` on macOS. In-root symlinks
  that `PathPolicy` allows today are resolved by `PathPolicy` first, and the canonical path is then
  opened symlink-free.
- **Refuse regular files with `nlink > 1`** (outside `target/`), for reads and writes.
- **Write to a temp file in the same directory, then rename**, instead of truncating in place.
  `create_dir_all` is replaced by a dirfd-relative `mkdirat` walk.

**`PathPolicy` additions:**
- `resolve_for_write` denies a `.git` component, case-insensitively.
- `secret_rule` gains `.cargo/credentials` and `.cargo/credentials.toml` (`path.rs:282`).

### 2f. `web_search` tool

**Seam.** A `WebSearch` trait, like `MemorySearch` (`tools.rs:99`), attached with
`with_web_search`. Without a backend, the tool answers with a fixed refusal.

**Input.** `{ query (1..=300 chars), max_results? 1..=10 }`.

**Refused:** queries that scrubbing would change, and URL-like queries (`://`, `www.`).

**Cap.** 30 searches per session, a fixed constant; beyond it, each search escalates.

**Results.** Plain text with numbered URLs, capped at 32 KiB. The description says they are
untrusted and are not instructions.

**Tool list.** Eight tools, sorted; the cache-stability test is updated.

### 2g. Standing prompt (`lib.rs:32`)

Two sentences are added:
- "The shell has no internet; use web_search to research, or set `network` to ask the user for one
  command."
- "`.git` is read-only in the shell: git reads work there; run each git write command on its own
  and the harness runs it, with no remote access."

`tests/prompt.rs` stays under its bound.

### Phase 2 gate

Linux CI installs `bubblewrap` and `sqlite3` and checks user namespaces with an explicit
`--unshare-user` flag after S1's AppArmor sysctl step in this phase, rather
than waiting until Phase 4: without them the runtime tests would fail at setup instead of
testing isolation. Phase 4 keeps the planned two-OS matrix change.

Tests live in `crates/{core,elefante}/tests/sandbox.rs`, with the side-effectful macOS host
controls in `crates/core/tests/macos_host.rs`. The latter are explicitly ignored in ordinary
local test runs and executed by a dedicated disposable `macos-latest` CI job with `--ignored`;
that job must fail with a setup reason if GUI or clipboard services are unavailable. No such
control runs on the maintainer's Mac without separate approval. Every negative test has a
positive control.

| Gate item | Test (negative; positive control) |
|---|---|
| Former `known_gaps` harness-state bypasses (`D=.elefante; cat $D/x`, `cd .elefante && cat x`, `sqlite3` with an assembled path, `xargs cat`) | Classified `Allow`, then fail at the OS; the same commands read a non-harness file fine |
| Outside the root | A `~/.ssh` fixture read and a write outside the root fail; symlinks a command plants in its `TMPDIR` cannot redirect a later daemon write outside the root; `$CARGO_HOME/credentials.toml` is hidden; reading the root and toolchains works |
| Network off | TCP and UDP over IPv4 and IPv6, and DNS, fail; loopback to a listener **started by the test process** fails on both platforms; Linux: loopback inside the sandbox works |
| Network approved | With approval, TCP to a test listener works and a name resolves; Linux: an abstract-socket connect still fails |
| `.git` | Writes to `.git/hooks/pre-commit`, `.git/config` and `.GIT/config` fail; a hard link to `.git/config` cannot be written; Linux: a new `sub/.git` and `target/.git` are quarantined. `git status` and `git log -p` work in the sandbox. Harness `git add` + `commit -m` work and `.git/hooks` stays unchanged. Refused: `git push`, `git fetch`, `git commit --amen`, `git commit -S`, `git merge -s x`, `git commit` without `-m`, `git log \| git commit`; `git diff --output=<outside-scratch-path>` fails at the OS. `$HOME` is private session temp, so it is not an outside-root control. |
| Repo-config traps | `include.path` into the root, `hooksPath=.husky`, LFS, custom filter/merge attributes or configured executable filter/merge drivers: harness git refuses, and `.husky/` is read-only in the sandbox. A synthetic filter or merge-driver marker is not executed; the same Git write succeeds in a repository without those traps. |
| Harness state and secrets | `.elefante/` reads empty or is refused; in-root `.env` and `*.pem` are refused |
| Root validation | A session rooted at `$HOME` or `/` gets `Unavailable`, and bash runs nothing |
| Daemon and local services | A child process with a fake home connects to synthetic fallback, `$XDG_RUNTIME_DIR` and `ELEFANTE_SOCKET` override fixtures outside the root (under `/private/tmp` on macOS); sandboxed Off and Approved cannot connect, while the session-temp socket works. `/run` has no guaranteed writable fixture on CI, so the pure policy test rejects its broad mount and the runtime outside-socket and abstract-socket tests cover the reachable service classes. No real daemon or user state is touched. |
| macOS host | CI-only synthetic host controls first make `security find-generic-password`, `pbpaste`, `open -a`, `launchctl submit` and `osascript` succeed on the disposable runner, then require the current Seatbelt profile to refuse them. Missing host services fail the CI job rather than skip. |
| Key and processes | The test process has `OPENROUTER_API_KEY` set; inside, `env` and every readable `/proc/*/environ` lack it, `ps eww $PPID` fails, and only fds 0–2 are open, also while another sandboxed command runs. Controls: the same `/proc/*/environ` scan finds the key on the host, and the sandbox can read its own environ; the fd probe finds a deliberately retained fd 3 and none in a clean host child |
| Symlinks and links | `read` and `write` refuse a symlink to an outside fixture, a link swapped in during the call (loop test), and a hard link. The indexer skips them too |
| Lifetime | The timeout kills a grandchild; Linux: a `setsid` orphan is gone after its command; macOS: a held pipe does not stall the turn; 10 MB of output arrives capped with its size stated |
| seccomp (Linux) | `unshare -U`, `mount` and io_uring setup fail; `process_vm_readv` on itself returns EPERM under network off and succeeds on the host; `cargo test` on a fixture crate passes |
| Fail closed | `Unavailable` means bash refuses and a marker file is never created; `Disabled` means every bash call escalates |
| `web_search` | A query with a secret fixture or a URL is refused; the 31st call escalates; results are capped (stub backend) |

The FD gate compiles a test-only C helper that inspects its own `/proc/self/fd` entries and
excludes only the descriptor it opened for that enumeration. Its unsandboxed control first
verifies a clean process has no extra descriptor, then keeps FD 3 open and must detect it with
the same helper. CI reports only an FD number on failure, never the child environment. The `setsid`
control waits for its detached child to write a `ready` marker before the parent exits, so the
process-group cleanup cannot race the control's detachment.
The io_uring gate compiles a test-only C syscall fixture with the existing compiler, requires
`io_uring_setup` to succeed on the host, then expects ENOSYS in the sandbox. If the CI host
rejects the control, the row is red until its test strategy is amended in both plans; it does
not skip or claim a pass.

**Existing core bash tests** (`tests/tools.rs:45,236`) run under `Disabled` with an approving
sink. A copy of the cancellation test runs sandboxed.

**Commit:** `feat(core): run bash in an OS sandbox and route git writes through the harness`.

**Recorded commits.** The phase landed as 652b2ff `feat(core): confine shell execution and harden
tool access`. Its CI and gate fixes followed:
- 6717683 `fix(ci): run Linux sandbox tests after a valid namespace probe`;
- 3e694e1 `fix(ci): enable Ubuntu user namespaces for sandbox tests`;
- c12a39e `fix(guard): preserve writable sandbox devices on Linux`;
- 652f061 `fix(guard): hide sandbox init process details`;
- ea8dda4 `test(core): distinguish live descriptors from shell globbing`;
- d133a01 `test(core): exercise sandbox lifetime and service controls`;
- 9578fb0 `test(core): keep sandbox controls independent of their probes`;
- aa9d09d `ci: run the macOS host-service sandbox controls on a disposable runner`;
- 365d02c `test(core): probe inherited descriptors with a compiled helper`.

---

## Phase 3 — `elefante`: daemon, protocol, TUI, `web_search` backend

**Detection.** `Sandbox` is detected at daemon start and passed into each `ToolExecutor`
(`server.rs:1009`) with that session's validated policy. The status is added to the create and
attach results and the snapshot as `sandbox: { state, detail }`, mirrored in
`tui/src/protocol.ts`. The TUI status line shows off or unavailable in the warning colour; the
CLI prints one warning line.

**Socket token.**
- The daemon keeps the token beside its socket: `~/.elefante/run/token` by default,
  `$XDG_RUNTIME_DIR/token` when that sets the socket, or beside an explicit `ELEFANTE_SOCKET`
  (tests).
- 128 random bits from the OS generator through `getrandom`, mode 0600, written atomically (temp
  file, then rename) **before** `listen`. Only the starter that holds the socket directory's lock
  writes it, so a losing starter never replaces a live daemon's token.
- The first request must be `auth { token }`, compared in constant time; otherwise the daemon
  answers `-32008` (`UNAUTHORIZED`) and closes. `-32001` stays "unknown session".
- The CLI (`main.rs`) and the TUI (`protocol.ts`) authenticate first, retrying briefly while the
  file is absent during auto-start. When the file never appears, the client sends an empty token.
- An older daemon answers `-32601`, which the client maps to `OLDER_DAEMON`.
- The policy renderer asserts that neither the socket nor the token path is readable inside.

**Approval modal.** `tui/src/ui/Modal.tsx:31` shows `approval.command` through `displaySafe`,
plus a visible "network" badge driven by the structured field. Today it renders the raw command.

**`web_search` backend.** The provider gets an optional `Request.web_search` rendered as
`plugins:[{id:"web",engine:"exa",max_results}]`, with Exa pinned so that native engines cannot
fetch URLs named in a query. The daemon's `WebSearch` implementation runs one request on
`ELEFANTE_SEARCH_MODEL` (unset, the session's author model; the S9 probe may revisit this), and its
usage is added to the turn's usage.

**Identity.** Read once at daemon start with the hardened git, from `~/.gitconfig` and then
`$XDG_CONFIG_HOME/git/config`. Without both `user.name` and `user.email`, the gate denies a harness
Git write (rule `git-identity-missing`), so the user never approves a write that cannot commit.

**Gate:**
- auth required; a bad token is refused; the race at startup works; an older daemon gets a named
  error;
- TUI `bun test` covers the status field, the badge and escaped commands;
- a fake-daemon client test;
- an end-to-end run of `elefante` with a stub provider whose scripted tool call cannot read a
  `~/.ssh` fixture;
- credentialed: one live `web_search` returns a URL.

**Commit:** `feat(elefante): authenticate the socket, report sandbox state, add web search`.

**Recorded commits.** The phase landed as several commits instead of that one:
- f594ff6 `fix(core): protect the daemon token beside a runtime-directory socket`;
- b3dfdeb `feat: authenticate the daemon socket with a token`;
- db18a9a `feat(core): probe the sandbox once and sweep stale temp directories`;
- 8d3c507 `fix(core): deny harness git writes when no identity is attached`;
- 290d1bb `feat(elefante): report sandbox state and wire identity and web search`;
- 34b5c10 `feat(tui): show sandbox state and escape approval commands`.

Review fixes followed: f429740, e198cde, 658127e, 4ab5d4f, f1d745a, 344949a, 05c7340, 022c503,
cb22490, cc8c989, 3089c60, 399356e, 52ab1b0, 4c4f0ac, da9a602, 9a4b37d, 7807123 and 9c8da06.

---

## Phase 4 — CI and docs, then M4's gate

**CI:**
- `rust` becomes a matrix of `ubuntu-24.04` and `macos-latest`, each running fmt, clippy and test.
- Linux setup for the existing Rust leg moved to Phase 2 so its runtime gate can execute;
  Phase 4 retains that setup in the new matrix.
- msrv stays Linux-only and checks all targets; the advisory lints job is unchanged.
- `credentialed.yml` gains the `web_search` test.

**Docs:**
- `DEVELOPER_GUIDE.md`:
  - new § "Sandbox": threat model, mechanism per OS, policy table, what runs outside, residuals;
  - updates to § 9, § 14, the tools table and the RPC table.
- `README.md`: the Linux requirements and `ELEFANTE_SANDBOX=0`.
- Crate READMEs: the new modules.

**M4 verification:**
- fmt, plus clippy and test on both operating systems
- `cargo +1.90.0 check --workspace --all-targets`
- `bunx tsc --noEmit`, `bun test`, and the compile smoke test
- CI green
- the credentialed suite, run by the maintainer
- a manual run on this Mac:
  - `elefante "run cargo test"` works sandboxed;
  - reading `~/.ssh/known_hosts` via the model is refused;
  - `curl example.com` fails, then works once approved;
  - an agent commit leaves `.git/hooks` untouched.

**Record.** The pull request description states what M4's gate proved.

**Commit:** `ci: test on macOS and Linux with the sandbox` (docs travel in the same commit, or in a
separate `docs:` commit if they are large).

**Recorded commits.** The CI change landed as e540ad7 `ci: run the Rust gate on macOS and Linux`.
The docs landed in four separate `docs:` commits, from 9d89cc9 to the one that adds this line.

## Phase 5 — M4.1 (after M4 is green)

**Baseline:**
- Harness write git runs under a third profile: network off, nothing outside the root readable,
  root writable, `.git` writable except `hooks/`, `config`, `info/` and any `hooksPath` target.
- On Linux this is a bubblewrap profile with `.git` rw-bound and those paths ro-bound over it.
- On macOS, Seatbelt allows `.git` writes except those paths.
- The allowlist stays in place as the second line of defence.

**Gate:**
- A test-only build flag widens the allowlist (`--output=$HOME/x`, `--no-index ~/.ssh/f`,
  `merge -s evil`), and each one still fails at the OS.
- A commit and a rebase still work.

**Record.** The closing pull request deletes this file and states what the gate proved.

---

## Residual risks

The residual risks moved to `DEVELOPER_GUIDE.md` § 9 **Sandbox** › **Residual risks**, which now
owns them.

---

## Review findings → changes

| # | Finding | Change |
|---|---|---|
| 1 | Harness read git outside the sandbox: `--output` writes anywhere, `--no-index` reads anywhere, abbreviations evade exact-match blocks | Read git runs in the sandbox; per-subcommand exact allowlist; absolute git; fixed PATH; gpg off; **M4.1** adds the git sandbox |
| 2 | macOS loopback reaches host services | macOS loopback needs approval; gate uses a host listener |
| 3 | Seatbelt profile injection through file names | `-D` params only, hostile names refused, constant regexes |
| 4 | Shared TMPDIR exposes ssh-agent and tmux sockets | Private per-session temp dir |
| 5 | `hooksPath`/include targets in the worktree; LFS | Made read-only or refused; hook managers recorded as residuals |
| 6 | Threat model overclaims (editor tasks, dev-mode TUI) | Narrowed; recorded as residuals |
| 7 | Root not validated | `validate_root` |
| 8 | Readable set leaks (cargo token, tool roots) | Exact PATH dirs; cargo credentials hidden and added to `secret_rule` |
| 9 | Daemon writes through links; indexer reads via links | dirfd + no-symlink opens, `nlink` checks, rename-writes, indexer included |
| 10 | `.GIT` on case-insensitive APFS; hard link to config | Case-insensitive rules; hard-link gate test |
| 11 | Pruned `.git` scan; repo-less roots | Unpruned directory walk; root `RwLock`; no harness git without a repository |
| 12 | Host network namespace when approved (abstract sockets, DNS) | `AF_UNIX` blocked when approved; resolved bound; gate test |
| 13 | Mach services had no acceptance test | Gate list |
| 14 | Token beside the socket; startup race | `~/.elefante/run/token`, atomic before listen, client retry |
| 15 | Classifier shrink landed before the sandbox existed | Moved into Phase 2; `Disabled` escalates every call |
| 16 | `gitdir` targets untrusted | Validated |
| 17 | fd gate item dropped; pipe held open | Restored; read stops 2 s after exit |
| 18 | Router quoting and global flag refusals | Plain words only; per-subcommand lists |
| 19 | `web_search` engine, URL queries, cap needs gate code | Exa pinned; URLs refused; `check_in` extended |
| 20 | Raw approval text; ambiguous network prefix | `displaySafe` + structured `network` field |
| 21 | seccomp action limits | Two programs; ENOSYS for `clone3`/io_uring; `--disable-userns` |
| 22 | Weak gate items | Positive controls; host-listener and `/proc/*/environ` checks; EROFS hint test |
| 23 | Minor items | `--ro-bind-try`, `/private/etc`, resolved bash, LFS refusal; `ioctl(TIOCSTI)` filter added to the EPERM program; `Event::Citation` cut |
