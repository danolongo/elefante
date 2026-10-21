#!/usr/bin/env bash
# Manual, scratch-only experiment. Never source the host environment or secrets.
set -euo pipefail
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
evidence=${1:?usage: m4-linux-spike.sh OUTPUT_DIRECTORY}
mkdir -p "$evidence"
evidence=$(cd "$evidence" && pwd)
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
export M4_SCRATCH="$scratch" M4_EVIDENCE="$evidence" M4_SCRIPT_DIR="$script_dir"
python3 - <<'PY'
import ctypes, errno, json, os, pathlib, shutil, socket, struct, subprocess, time

root = pathlib.Path(os.environ['M4_SCRATCH'])
out = pathlib.Path(os.environ['M4_EVIDENCE'])
results = []

def record(name, status, detail, required=True):
    results.append(dict(probe=name, status=status, evidence=detail, required=required))
    (out / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
    print(f'{name}: {status}: {detail}', flush=True)

def run(argv, **kwargs):
    return subprocess.run(argv, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, timeout=kwargs.pop('timeout', 1200), **kwargs)

def check(name, argv, expected=0):
    try:
        p = run(argv)
        record(name, 'pass' if p.returncode == expected else 'fail',
               f'exit={p.returncode}; {p.stdout.strip()}')
        return p.returncode == expected
    except (OSError, subprocess.TimeoutExpired) as e:
        record(name, 'not-run', str(e))
        return False

for tool in ['bwrap', 'git', 'cargo', 'rustc', 'bun']:
    check('versions/' + tool, [tool, '--version'])

crate = root / 'dependency-probe'
(crate / 'src').mkdir(parents=True)
shutil.copyfile(pathlib.Path(os.environ['M4_SCRIPT_DIR']) / 'm4-linux-probe.rs', crate / 'src/main.rs')
(crate / 'Cargo.toml').write_text('[package]\nname="m4-linux-probe"\nversion="0.0.0"\nedition="2021"\n'
    '[dependencies]\nseccompiler="=0.5.0"\nrustix={version="=1.1.5",features=["fs"]}\n')
(root / 'outside').write_text('positive traversal target')
deps = check('S6-S7/MSRV-build', ['cargo', '+1.90.0', 'build', '--manifest-path', str(crate / 'Cargo.toml')])
probe_ok = deps and check('S7/Linux', [str(crate / 'target/debug/m4-linux-probe'), str(root)])
if (crate / 'Cargo.lock').exists():
    shutil.copyfile(crate / 'Cargo.lock', out / 'dependency-probe.Cargo.lock')

# Broad host read bind is intentional for S1-S3 mechanics only. S4 uses a
# separate explicit mount set. Neither profile is a production policy.
base = ['bwrap', '--ro-bind', '/', '/', '--bind', str(root), str(root),
        '--unshare-user', '--unshare-pid', '--proc', '/proc', '--die-with-parent', '--new-session']
p = run(base + ['--', '/bin/true'])
record('S1/default-userns', 'pass' if p.returncode == 0 else 'fail',
       f'pre-sysctl observation exit={p.returncode}; {p.stdout.strip()}', required=False)
setting = pathlib.Path('/proc/sys/kernel/apparmor_restrict_unprivileged_userns')
original = setting.read_text().strip() if setting.exists() else None
try:
    # Record the profile alternative before disabling the runner's restriction.
    profile = pathlib.Path('/etc/apparmor.d/bwrap-userns-restrict')
    if profile.exists():
        for name, argv in [('load', ['sudo', 'apparmor_parser', '-r', str(profile)]),
                           ('userns', base + ['--', '/bin/true'])]:
            p = run(argv)
            record('S1/apparmor-profile-' + name, 'pass' if p.returncode == 0 else 'fail',
                   f'exit={p.returncode}; {p.stdout.strip()}', required=False)
    else:
        record('S1/apparmor-profile-userns', 'not-run',
               '/etc/apparmor.d/bwrap-userns-restrict absent; profile alternative unresolved', required=False)
    if original is not None:
        check('S1/disable-apparmor-restriction', ['sudo', 'sysctl', '-w',
              'kernel.apparmor_restrict_unprivileged_userns=0'])
    available = check('S1/sysctl-userns', base + ['--', '/bin/true'])
    if not available:
        for name in ['S1/disable-userns', 'S2/stdin', 'S3/mounts', 'S3/loopback',
                     'S3/orphans', 'S4/toolchain', 'S6/stacked-filters']:
            record(name, 'not-run', 'bubblewrap unavailable; inspect S1 output')
    else:
        check('S1/nested-userns-positive', base + ['--', 'unshare', '-Ur', 'true'])
        p = run(base + ['--disable-userns', '--', 'unshare', '-Ur', 'true'])
        record('S1/disable-userns', 'pass' if p.returncode != 0 else 'fail', p.stdout.strip())
        (root / '.git').mkdir()
        (root / '.git/config').write_text('fixture')
        (root / 'hidden').mkdir()
        (root / 'hidden/value').write_text('fixture')
        (root / 'secret-fixture').write_text('fixture')
        check('S3/mounts-positive', ['sh', '-c',
              'test -s "$1/.git/config" && test -s "$1/hidden/value" && test -s "$1/secret-fixture"',
              'sh', str(root)])
        check('S3/mounts', base + ['--ro-bind', str(root / '.git'), str(root / '.git'),
              '--tmpfs', str(root / 'hidden'), '--ro-bind', '/dev/null', str(root / 'secret-fixture'),
              '--', 'sh', '-c', 'test -r "$1/.git/config" && ! touch "$1/.git/new" && '
              'test ! -e "$1/hidden/value" && test ! -s "$1/secret-fixture" && touch "$1/writable"',
              'sh', str(root)])
        check('S3/loopback', base + ['--unshare-net', '--', 'python3', '-c',
              'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); s.listen(); '
              'c=socket.create_connection(s.getsockname()); a,_=s.accept(); c.send(b"x"); assert a.recv(1)==b"x"'])
        marker = root / 'orphan'
        check('S3/orphans', base + ['--', 'sh', '-c',
              '(sleep 2; touch "$1") >/dev/null 2>&1 &', 'sh', str(marker)])
        time.sleep(3)
        record('S3/orphan-killed', 'pass' if not marker.exists() else 'fail',
               'delayed child marker absent' if not marker.exists() else 'orphan survived')
        record('S3/orphan-positive', 'pass' if run(['sh', '-c',
               'sleep 0.1; touch "$1"', 'sh', str(marker)]).returncode == 0 and marker.exists()
               else 'fail', 'same delayed marker operation outside namespace')
        # Dependency-free fixtures isolate toolchain access from network and registry access.
        fixture = root / 'toolchain-fixture'
        (fixture / 'src').mkdir(parents=True)
        (fixture / 'Cargo.toml').write_text('[package]\nname="sandbox-fixture"\nversion="0.0.0"\nedition="2021"\n')
        (fixture / 'src/lib.rs').write_text('#[test] fn addition() { assert_eq!(2+2,4); }\n')
        (fixture / 'fixture.test.js').write_text('import {test,expect} from "bun:test"; test("addition",()=>expect(2+2).toBe(4));\n')
        cargo_bin = pathlib.Path(shutil.which('cargo')).parent
        bun_bin = pathlib.Path(shutil.which('bun')).parent
        rustup_home = pathlib.Path(os.environ.get('RUSTUP_HOME', str(pathlib.Path.home() / '.rustup')))
        toolchain = pathlib.Path(run(['rustup', 'which', '--toolchain', '1.90.0', 'rustc']).stdout.strip()).parent.parent
        paths = list(dict.fromkeys(['/usr/bin', '/bin', str(cargo_bin), str(bun_bin)]))
        sandbox_home = root / 'tool-home'
        (sandbox_home / '.cargo').mkdir(parents=True)
        reads = [p for p in ['/usr', '/bin', '/lib', '/lib64', '/etc/ld.so.cache',
                             str(cargo_bin), str(bun_bin), str(toolchain),
                             str(rustup_home / 'settings.toml')] if pathlib.Path(p).exists()]
        compiler_links = []
        compiler = pathlib.Path(shutil.which('cc'))
        while compiler.is_symlink():
            target = pathlib.Path(os.readlink(compiler))
            target = target if target.is_absolute() else compiler.parent / target
            compiler_links.append(dict(link=str(compiler), target=str(target)))
            # Preserve each exact alternative entry, rather than mounting /etc.
            if not any(target.is_relative_to(pathlib.Path(p)) for p in reads):
                reads.append(str(target))
            compiler = target
        record('S4/compiler-resolution', 'pass', json.dumps(compiler_links))
        minimal = ['bwrap', '--unshare-user', '--disable-userns', '--unshare-net',
                   '--unshare-pid', '--die-with-parent', '--clearenv', '--proc', '/proc', '--dev', '/dev',
                   '--tmpfs', '/tmp']
        for path in dict.fromkeys(reads):
            minimal += ['--ro-bind', path, path]
        minimal += ['--bind', str(root), str(root), '--chdir', str(fixture)]
        variables = dict(PATH=':'.join(paths), HOME=str(sandbox_home), CARGO_HOME=str(sandbox_home / '.cargo'),
                         RUSTUP_HOME=str(rustup_home), RUSTUP_TOOLCHAIN=toolchain.name,
                         CARGO_NET_OFFLINE='true', CARGO_TARGET_DIR=str(fixture / 'target'))
        for key, value in variables.items():
            minimal += ['--setenv', key, value]
        commands = [('cargo-build', ['cargo', 'build', '--offline']),
                    ('cargo-test', ['cargo', 'test', '--offline']),
                    ('rustup-proxy', ['rustc', '--version']), ('bun-test', ['bun', 'test', './fixture.test.js'])]
        for name, argv in commands:
            positive_env = dict(os.environ, **variables)
            positive_env['CARGO_TARGET_DIR'] = str(root / 'positive-target')
            p = run(argv, cwd=fixture, env=positive_env)
            record('S4/positive-' + name, 'pass' if p.returncode == 0 else 'fail', p.stdout.strip())
            check('S4/minimal-' + name, minimal + ['--'] + argv)
        record('S4/paths', 'pass', json.dumps(dict(PATH=paths, readable=reads,
               writable=[str(fixture), str(sandbox_home), '/tmp'],
               cache_files=[str(p.relative_to(root)) for p in sandbox_home.rglob('*') if p.is_file()],
               scope='sufficient explicit set for dependency-free fixtures; registry/package workloads not established')))
        check('S4/host-credentials-hidden', minimal + ['--', 'sh', '-c',
              'test ! -e "$1/credentials" && test ! -e "$1/credentials.toml"', 'sh',
              str(pathlib.Path.home() / '.cargo')])
        # Copy tracked source only into scratch; never copy host secrets or build trees.
        source = pathlib.Path(os.environ['M4_SCRIPT_DIR']).parent
        repo_copy = root / 'repo'
        repo_copy.mkdir()
        tracked = subprocess.run(['git', '-C', str(source), 'ls-files', '-z'],
                                 check=True, stdout=subprocess.PIPE).stdout.decode().split('\0')
        for relative in filter(None, tracked):
            parts = pathlib.Path(relative).parts
            if any(p.startswith('.env') or p in ('credentials', 'credentials.toml') or p in
                   ('.ssh', '.aws', '.elefante', '.omx', 'node_modules', 'target') for p in parts):
                continue
            src, dst = source / relative, repo_copy / relative
            if src.is_file() and not src.is_symlink():
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src, dst)
        repo_env = dict(variables, CARGO_TARGET_DIR=str(repo_copy / 'target'),
                        CARGO_NET_OFFLINE='false', BUN_INSTALL_CACHE_DIR=str(sandbox_home / 'bun-cache'))
        # This fresh CARGO_HOME cannot read real credentials, even during warmup.
        host_env = dict(repo_env, LANG='C.UTF-8', TERM='dumb')
        warm = [('cargo-build', ['cargo', 'build', '--workspace']),
                ('cargo-tests', ['cargo', 'test', '--workspace', '--no-run']),
                ('bun-install', ['bun', 'install', '--frozen-lockfile']),
                ('bun-test', ['bun', 'test'])]
        for name, argv in warm:
            cwd = repo_copy / 'tui' if name.startswith('bun') else repo_copy
            p = run(argv, cwd=cwd, env=host_env)
            record('S4/repo-positive-' + name, 'pass' if p.returncode == 0 else 'fail', p.stdout.strip())
        cargo_home = sandbox_home / '.cargo'
        registry_reads = [str(cargo_home / name) for name in ('registry', 'git') if (cargo_home / name).exists()]
        cache_writes = [str(cargo_home / name) for name in
                        ('.package-cache', '.package-cache-mutate', '.global-cache', '.global-cache-wal', '.global-cache-shm')]
        for path in cache_writes:
            pathlib.Path(path).touch(exist_ok=True)
        # Synthetic credential file has its own readable positive control and hidden negative.
        fake_credentials = cargo_home / 'credentials.toml'
        fake_credentials.write_text('# synthetic sandbox visibility fixture; no token\n')
        check('S4/fake-credentials-positive', ['sh', '-c', 'test -s "$1"', 'sh', str(fake_credentials)])
        repo_profile = list(minimal)
        for path in registry_reads:
            repo_profile += ['--ro-bind', path, path]
        repo_profile += ['--ro-bind', '/dev/null', str(fake_credentials), '--chdir', str(repo_copy)]
        repo_env['CARGO_NET_OFFLINE'] = 'true'
        for key, value in repo_env.items():
            repo_profile += ['--setenv', key, value]
        for name, argv in [('cargo-build', ['cargo', 'build', '--workspace', '--offline']),
                           ('cargo-test', ['cargo', 'test', '--workspace', '--no-fail-fast', '--offline']),
                           ('bun-test', ['bun', 'test'])]:
            profile = repo_profile + (['--chdir', str(repo_copy / 'tui')] if name.startswith('bun') else [])
            check('S4/repo-sandbox-' + name, profile + ['--'] + argv)
        check('S4/fake-credentials-hidden', repo_profile + ['--', 'sh', '-c',
              'test ! -s "$1"', 'sh', str(fake_credentials)])
        record('S4/repo-paths', 'pass', json.dumps(dict(PATH=paths, readable=reads + registry_reads,
               writable_root=str(repo_copy), private_home=str(sandbox_home), writable_cargo_cache_files=cache_writes,
               bun_cache=str(sandbox_home / 'bun-cache'),
               observed_cargo_files=[str(p.relative_to(cargo_home)) for p in cargo_home.iterdir()],
               limits='warm build artifacts reused; dependency cache read-only; Bun dependencies preinstalled; no network install inside sandbox')))
        if probe_ok:
            # ctypes calls happen only in the disposable child, never in Rust/product code.
            prefix = 'import ctypes,errno,os; l=ctypes.CDLL(None,use_errno=True); '
            positive = prefix + 'assert l.syscall(140,0,0)>=0; assert l.syscall(145,0)>=0; print("both sentinels succeed")'
            check('S6/filter-sentinels-positive', base + ['--', 'python3', '-c', positive])
            stdin_observer = ('\ntry:\n data=os.read(0,16); print("stdin bytes read:",len(data))'
                              '\nexcept OSError as e:\n print("stdin errno:",e.errno)\n')
            single = (prefix + 'ctypes.set_errno(0); r=l.syscall(140,0,0); '
                      'assert r==-1 and ctypes.get_errno()==errno.EPERM; print("EPERM sentinel verified")'
                      + stdin_observer)
            p = run(['python3', '-c', prefix + stdin_observer], input='positive')
            record('S2/stdin-observer-positive',
                   'pass' if p.returncode == 0 and 'stdin bytes read: 8' in p.stdout else 'fail', p.stdout.strip())
            with (root / 'eperm.bpf').open('rb') as ep:
                p = run(base + ['--seccomp', '0', '--', 'python3', '-c', single], stdin=ep)
            record('S2/seccomp-stdin', 'pass' if p.returncode == 0 else 'fail', p.stdout.strip())
            code = (prefix + 'ctypes.set_errno(0); r=l.syscall(140,0,0); '
                    'assert r==-1 and ctypes.get_errno()==errno.EPERM; '
                    'ctypes.set_errno(0); r=l.syscall(145,0); '
                    'assert r==-1 and ctypes.get_errno()==errno.ENOSYS; '
                    'ctypes.set_errno(0); r=l.syscall(435,0,0); '
                    'assert r==-1 and ctypes.get_errno()==errno.ENOSYS; '
                    'print("EPERM and ENOSYS sentinels and clone3 verified")' + stdin_observer)
            with (root / 'eperm.bpf').open('rb') as ep, (root / 'enosys.bpf').open('rb') as en:
                p = run(base + ['--add-seccomp-fd', '0', '--add-seccomp-fd', str(en.fileno()),
                        '--', 'python3', '-c', code], stdin=ep, pass_fds=(en.fileno(),))
            record('S2/stdin-and-S6/stacked-filters', 'pass' if p.returncode == 0 else 'fail', p.stdout.strip())
            def evaluate(program, number, arch):
                instructions = list(struct.iter_unpack('=HBBI', program))
                pc, accumulator = 0, 0
                while pc < len(instructions):
                    op, yes, no, value = instructions[pc]
                    if op == 0x20:
                        accumulator = {0: number, 4: arch}.get(value, 0)
                    elif op == 0x06:
                        return value
                    elif op == 0x05:
                        pc += value
                    elif op in (0x15, 0x25, 0x35, 0x45):
                        condition = {0x15: accumulator == value, 0x25: accumulator > value,
                                     0x35: accumulator >= value, 0x45: bool(accumulator & value)}[op]
                        pc += yes if condition else no
                    else:
                        raise ValueError(f'unsupported BPF opcode {op:#x}')
                    pc += 1
                raise ValueError('BPF fell through')
            actions = {}
            for name in ['eperm', 'enosys']:
                program = (root / (name + '.bpf')).read_bytes()
                shutil.copyfile(root / (name + '.bpf'), out / (name + '.bpf'))
                actions[name] = {kind: hex(evaluate(program, number, arch)) for kind, number, arch in
                    [('native_getpid', 39, 0xc000003e), ('x32_getpid', 0x40000027, 0xc000003e),
                     ('wrong_arch', 39, 0x40000003)]}
            syscall_code = ('import ctypes,os; l=ctypes.CDLL(None,use_errno=True); '
                            'assert l.syscall(39)==os.getpid(); ctypes.set_errno(0); '
                            'r=l.syscall(0x40000027); print("native positive passed; x32 result",r,"errno",ctypes.get_errno())')
            check('S6/x32-positive', ['python3', '-c', syscall_code])
            with (root / 'eperm.bpf').open('rb') as ep:
                p = run(base + ['--seccomp', '0', '--', 'python3', '-c', syscall_code], stdin=ep)
            x32_action = int(actions['eperm']['x32_getpid'], 16)
            expected_runtime = 0 if x32_action == 0x7fff0000 else -31
            valid = (all(a['native_getpid'] == '0x7fff0000' and a['wrong_arch'] in
                         ('0x0', '0x80000000') for a in actions.values())
                     and p.returncode == expected_runtime)
            record('S6/x32', 'pass' if valid else 'fail', json.dumps(dict(generated_bpf_actions=actions,
                   runtime_exit=p.returncode, runtime_output=p.stdout.strip(),
                   interpretation='ALLOW=0x7fff0000; KILL_PROCESS=0x80000000; KILL_THREAD=0x0; explicit x32 guard required if ALLOW')))
        else:
            record('S2/stdin-and-S6/stacked-filters', 'not-run', 'dependency probe failed')
finally:
    if original is not None:
        check('S1/restore-apparmor-restriction', ['sudo', 'sysctl', '-w',
              'kernel.apparmor_restrict_unprivileged_userns=' + original])

# An exact long option is the positive control for the abbreviated spelling.
repo = root / 'git-fixture'
check('S10/init', ['git', 'init', '-q', str(repo)])
env = dict(os.environ, GIT_TEST_DISALLOW_ABBREVIATED_OPTIONS='1')
exact = run(['git', '-C', str(repo), 'status', '--porcelain'], env=env)
short = run(['git', '-C', str(repo), 'status', '--porc'], env=env)
record('S10/abbreviated-options', 'pass' if exact.returncode == 0 and short.returncode != 0 else 'fail',
       f'exact exit={exact.returncode}; abbreviated exit={short.returncode}; {short.stdout.strip()}')
if not deps:
    record('S7/Linux', 'not-run', 'MSRV dependency build failed')
record('scope', 'not-run', 'S5/macOS, S7/macOS, S8 and credentialed maintainer-only S9 are outside this Linux harness')
(out / 'summary.md').write_text('# Linux sandbox spike\n\n' + '\n'.join(
    f"- {r['probe']}: **{r['status']}** — {r['evidence']}" for r in results) + '\n')
# Unanswered questions are a failing gate, never a silently green spike.
raise SystemExit(1 if any(r['required'] and r['status'] != 'pass' and r['probe'] != 'scope' for r in results) else 0)
PY
