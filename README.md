# Elefante

Elefante is a small, modifiable coding-agent harness. Its current implementation is a
Rust command-line agent loop with a provider-neutral core, OpenRouter streaming transport,
seven guarded tools, and policy enforced in code rather than in a long system prompt.

The long-term design adds a daemon, OpenTUI client, graph-backed project memory,
adversarial critics, and token analysis. Those features are deliberately milestone-gated;
the repository currently contains the M0 loop and compile-safe scaffolds for later crates.

## Project status

M0 is implemented and all hermetic checks pass. Formal milestone completion still requires
the two credentialed checks described in [`docs/MILESTONES.md`](docs/MILESTONES.md): a real
prompt-cache hit through OpenRouter and a live edit against this repository. M1 work has not
begun.

## Quick start

Requirements:

- Rust 1.85 or newer
- An OpenRouter API key supplied through the process environment

Run the agent from the repository it should be allowed to modify:

```sh
export OPENROUTER_API_KEY="..."
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

Environment defaults are `OPENROUTER_MODEL` (default `anthropic/claude-opus-5`),
`ELEFANTE_EFFORT`, and `OPENROUTER_BASE_URL` (default `https://openrouter.ai/api/v1`).
Command-line values take precedence where both forms exist. Models are named by OpenRouter
slug, so reaching a different vendor is a different slug rather than a different transport.

### Environment file

Elefante reads configuration from the process environment; it does not load a `.env` file
itself. Keep one for your shell to source, structured like this:

```sh
# Required. https://openrouter.ai/keys
OPENROUTER_API_KEY=sk-or-v1-...

# Optional — all have defaults.
OPENROUTER_MODEL=anthropic/claude-opus-5
OPENROUTER_BASE_URL=https://openrouter.ai/api/v1
ELEFANTE_EFFORT=xhigh
```

Two properties of this file are deliberate rather than incidental:

- **`.env` is gitignored, and Elefante's own path policy refuses to read it.** The agent
  editing this repository cannot open the file holding its own key. `.env.example` is
  refused for the same reason — the policy matches the `.env.` prefix, and carving an
  exception into a secret deny-list is how holes start.
- **The key never needs to reach the model's context.** It is read from the environment at
  process start, so no tool result and no memory row should ever contain it. The scrubber
  redacts `sk-or-v1-` and `sk-ant-` shapes from every tool result as a second line of
  defence, in case a key reaches Elefante by some path the policy did not anticipate.

`.env.example` in the repository root is an interim convenience. It goes away once the
harness prompts for a key on first launch and stores it outside the working tree — see
[`docs/PLAN.md`](docs/PLAN.md) § First-run key setup.

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
