# elefante-guard

Deterministic policy for filesystem access, shell commands, and output. **This crate decides
whether an operation is acceptable; it never performs one.** That separation is not stylistic —
it is what lets the entire safety layer be unit-tested with no filesystem, no network, and no
subprocess.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 11 | Module declarations and the three re-exports. Nothing else. |
| `src/bash.rs` | 390 | `BashClassifier` — tree-sitter full-parse-tree shell classification. |
| `src/path.rs` | 256 | `PathPolicy` — repository-bound path resolution and secret denial. |
| `src/scrub.rs` | 56 | `Scrubber` — credential redaction over untrusted output. |

The three policies are independent. Nothing in `bash.rs` calls into `scrub.rs`; the only shared
code is `path::built_in_secret_rule`, which `bash.rs` reuses so a shell argument naming a secret
is denied by the same rule that denies opening it directly.

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

**`known_gaps()` exists so callers cannot overstate this gate.** It names four things static
classification cannot catch: runtime aliases and shell functions, commands assembled through
variable expansion or `eval`, obfuscated command names and interpreter payloads, and filesystem
races after classification. Stated plainly: shell obfuscation is unbounded. This is
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
cargo test -p elefante-guard        # 15 tests, no fixtures, no network
```

Three files mirroring the three modules — `tests/bash_classifier.rs` (4),
`tests/path_policy.rs` (8), `tests/scrubber.rs` (3). Nothing is `#[ignore]`d and nothing needs
credentials; the whole crate is pure decision logic, which is the point.

The path tests do real symlink and `$HOME` manipulation inside `tempfile` directories —
`denies_credentials_when_workspace_is_home_or_credential_directory` and
`denies_canonical_credentials_target_with_isolated_home` both rewrite `HOME`, so they must stay
hermetic rather than reading the developer's actual home.

## Boundaries

`guard` must never depend on `tools` or `mem`. Both depend on it, so an edge in the other
direction cycles the graph — but the deeper reason is that a policy crate that can perform
syscalls stops being testable as policy. If a check here needs to touch the filesystem to
decide, that is a signal the check belongs at the call site instead.

It decides. Something else does.
