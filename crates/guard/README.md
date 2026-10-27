# elefante-guard

Deterministic policy for filesystem access, shell commands, git writes, the OS sandbox and output.
**This crate decides whether an operation is acceptable; it never performs one.** That separation is not stylistic —
it is what lets the entire safety layer be unit-tested with no filesystem, no network, and no
subprocess.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 13 | Module declarations, the public `git` and `sandbox` modules, and the three re-exports. Nothing else. |
| `src/bash.rs` | 458 | `BashClassifier` — tree-sitter full-parse-tree shell classification. |
| `src/path.rs` | 361 | `PathPolicy` — repository-bound path resolution and secret denial. |
| `src/scrub.rs` | 56 | `Scrubber` — credential redaction over untrusted output. |
| `src/sandbox.rs` | 609 | `Policy`, `validate_root`, the Seatbelt renderer `seatbelt_profile`, the bubblewrap renderer `bwrap_args`, `seccomp_programs` (Linux only) and the cleared `environment`. |
| `src/git.rs` | 445 | The write-git router `route`, the harness `invocation`, and `hardened`, the base invocation for trusted git reads. |

`bash.rs` and `git.rs` share the list of write subcommands, so the classifier and the router agree
on what a git read is. `bash.rs` also reuses `path::built_in_secret_rule`, so a shell argument
naming a secret meets the same rule that denies opening it directly. `git.rs` checks
routed paths with `PathPolicy::resolve_for_write`.

## Entry points

```rust
use elefante_guard::{BashClassifier, PathPolicy, Scrubber, Verdict};

// Resolve a caller-supplied path against the workspace root.
let policy = PathPolicy::new("/path/to/repo")?;
let safe = policy.resolve("src/main.rs")?;      // Err for traversal, secrets, escapes

// Classify a shell command before running it.
let decision = BashClassifier::new().classify_in("rm -rf /", policy.root());
assert_eq!(decision.verdict, Verdict::Deny);
assert_eq!(decision.rule, "destructive-command");

// Redact credentials from anything a tool returns.
let clean = Scrubber::new().scrub(&tool_output);
```

`PathPolicy` denies any `.elefante` path component, relative to the workspace root, as harness
state (learned notes, the index, keys). The bare directory counts too, so `cat .elefante` and
`grep -ra x .elefante` fail as well as `cat .elefante/memory.db`. The rule looks at the
root-relative path, so a workspace that itself lives under some `.elefante/` still works. A
separate canonical-prefix rule denies the user-level `<home>/.elefante`. In shell commands the
classifier denies only a literal `.elefante` path. It allows globs that can expand to it
(`.elef*`) and recursive readers over the root (`grep -r x .`), because the OS sandbox hides
`.elefante/` at run time. Notes reach the agent through the injected context and `memory_search`.

```rust
use elefante_guard::{git, sandbox};

// Route a whole bash call: a plain git write goes to the harness, a read stays in the shell.
match git::route("git commit -m 'fix'", policy.root()) {
    git::GitRoute::Harness(write) => { /* approve, then git::invocation(&git_bin, root, &write, &identity) */ }
    git::GitRoute::Refused(reason) => { /* deny: unsupported shape or option */ }
    git::GitRoute::NotHarness => { /* run in the sandbox */ }
}

// Render a validated session policy for the host's sandbox.
let (profile, params) = sandbox::seatbelt_profile(&policy)?;      // macOS: text plus -D params
let args = sandbox::bwrap_args(&policy, &launch)?;                // Linux: bubblewrap argv
let programs = sandbox::seccomp_programs(arch, policy.network)?;  // Linux: two BPF programs
```

**The sandbox renderers take every path as data.** `seatbelt_profile` passes each path as a `-D`
parameter, so the profile text never contains a path. It refuses a path with a quote, backslash or
control character. Seatbelt applies the last matching rule, so the profile emits every allow
before every deny. `Policy::validate` runs first in both renderers. It refuses:

- a path that is not absolute and normalized;
- a readable mount that would expose `$HOME`, `~/.elefante`, a protected socket or token, or a
  hidden path;
- a readable mount of host `/tmp`, `/run`, `/var/run`, `/dev` or `$XDG_RUNTIME_DIR`, apart from
  the device nodes and the resolver directory;
- a private temp directory that overlaps the root.

`bwrap_args` always passes `--add-seccomp-fd 3 --add-seccomp-fd 4`; the caller's launcher opens
those descriptors. The caller fills `Policy::runtime_dir`, so the renderer reads no process
environment. The Seatbelt name rules repeat the `PathPolicy` secret list, and a test checks that
they cover it.

**The git router routes only literal commands.** `route` routes a write only when the whole call is
one plain `git` command of literal words or single-quoted strings. Each write subcommand has an
exact option allowlist. A write in any other shape, behind a wrapper such as `env` or `xargs`, or
with an unknown option is `Refused`. `route` refuses `reset`, `clean`, `push`, `fetch`, `pull` and
`config` writes outright. `hardened` sets `-c` flags that turn off hooks, fsmonitor, the pager,
editors, credential helpers, signing, external diff and transports, and clears the environment
to a fixed list. `elefante-mem` and the daemon's identity read use it too.

`PathPolicy::with_denied_patterns` adds repository-relative globs on top of the built-in secret
rules. `BashClassifier::classify` is the same as `classify_in` rooted at `.`.

## Invariants

**A `Verdict` is a modality, not a severity.** `Allow` proceeds, `Deny` is terminal, and
`Escalate` is *a question* — it means the classifier has no opinion and a human must answer.
Collapsing the three into a boolean would erase the distinction between "this is forbidden" and
"I don't know," and those need different handling: a denied command emits no approval request at
all, while an escalated one suspends and waits. The enum orders by severity so
`max_by_key` over a whole parse tree yields the strongest verdict.

**Classification covers the full parse tree, not the first command.** Every `command` node is
inspected, including nodes nested under lists, pipelines, subshells, and command substitutions.
A parse error is `Escalate`, never `Allow` — unparseable input is exactly when a static
classifier knows least.

**`known_gaps()` exists so callers cannot overstate this gate.** It names what static
classification cannot catch: runtime aliases and shell functions, commands assembled through
variable expansion or `eval`, obfuscated command names and interpreter payloads, filesystem
races after classification. Four entries cover `.elefante/` and say "enforced by the OS
sandbox": shell variables naming the path (`D=.elefante; cat $D/memory.db`), `cd` then a relative
path, interpreters or `sqlite3` reading a path assembled at runtime, and `xargs` or command
substitution producing a path. Stated plainly: shell obfuscation is unbounded. This is
deny-by-default for destructive verbs outside an allowlist, not pattern-matching for badness,
and `documents_known_not_caught_obfuscation` asserts we don't claim otherwise.

**The `sk-ant-` scrub pattern stays regardless of which provider Elefante calls.** It is a
*secret shape*, not a provider dependency — Anthropic keys leak from the repositories Elefante
edits, and dropping the pattern when the provider changed would have opened a hole. The source
carries this as a comment for the same reason.

Path resolution is lexical first and canonical second: canonicalizing the nearest *existing*
ancestor is what lets the policy validate a write target that does not exist yet while still
following symlinks. `DEVELOPER_GUIDE.md` §9 has the full algorithm.

## Testing

```sh
cargo test -p elefante-guard        # no fixtures, no network
```

Five files mirroring the modules — `tests/bash_classifier.rs` (13), `tests/path_policy.rs` (13),
`tests/scrubber.rs` (3), `tests/sandbox_policy.rs` (17, plus one Linux-only seccomp test) and
`tests/git_router.rs` (6). `sandbox_policy.rs` compares the rendered Seatbelt profile with
`tests/snapshots/seatbelt.sb` and renders hostile path names. Nothing is `#[ignore]`d and nothing
needs credentials; the whole crate is pure decision logic, which is the point.

The path tests do real symlink and `$HOME` manipulation inside `tempfile` directories —
`denies_credentials_when_workspace_is_home_or_credential_directory` and
`denies_canonical_credentials_target_with_isolated_home` both rewrite `HOME`, so they must stay
hermetic rather than reading the developer's actual home.

## Boundaries

`guard` must never depend on `core` or `mem`. Both depend on it, so an edge in the other
direction cycles the graph — but the deeper reason is that a policy crate that can perform
syscalls stops being testable as policy. If a check here needs to touch the filesystem to
decide, that is a signal the check belongs at the call site instead.

It decides. Something else does.
