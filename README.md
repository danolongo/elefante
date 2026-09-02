# Elefante

Elefante is a small, modifiable coding-agent harness. Its current implementation is a
Rust command-line agent loop with a provider-neutral core, Anthropic streaming support,
seven guarded tools, and policy enforced in code rather than in a long system prompt.

The long-term design adds a daemon, OpenTUI client, graph-backed project memory,
adversarial critics, and token analysis. Those features are deliberately milestone-gated;
the repository currently contains the M0 loop and compile-safe scaffolds for later crates.

## Project status

M0 is implemented and all hermetic checks pass. Formal milestone completion still requires
the two credentialed checks described in [`docs/MILESTONES.md`](docs/MILESTONES.md): a real
Anthropic prompt-cache hit and a live edit against this repository. M1 work has not begun.

## Quick start

Requirements:

- Rust 1.85 or newer
- An Anthropic API key supplied through the process environment

Run the agent from the repository it should be allowed to modify:

```sh
export ANTHROPIC_API_KEY="..."
cargo run -p elefante-cli -- "Inspect this repository and summarize its structure."
```

When no prompt argument is supplied, Elefante reads the prompt from standard input:

```sh
printf '%s\n' 'Explain the current test strategy.' | cargo run -p elefante-cli
```

Useful options:

```text
--model MODEL
--effort low|medium|high|xhigh|max
--max-rounds N
--max-tokens N
```

Environment defaults are `ANTHROPIC_MODEL`, `ELEFANTE_EFFORT`, and
`ANTHROPIC_BASE_URL`. Command-line values take precedence where both forms exist.

## Safety model

Filesystem tools are restricted to the current working directory, known secret paths are
denied, shell commands are parsed before execution, and every tool result is scrubbed for
common credential shapes. Commands requiring approval do not execute in M0 because the
interactive approval transport arrives with the M1 daemon/TUI.

This is a safety gate, not an operating-system sandbox. Run Elefante in a repository and
account whose filesystem and process permissions match the intended blast radius.

## Development

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The provider transport tests open a loopback-only mock HTTP server. Restricted execution
environments may need to permit local socket binding.

Start with the [developer guide](docs/DEVELOPER_GUIDE.md) for crate responsibilities,
runtime data flow, data models, safety invariants, extension workflows, and testing. The
[design of record](docs/PLAN.md) describes future milestones; the
[milestone record](docs/MILESTONES.md) reports what has actually passed its gate.
