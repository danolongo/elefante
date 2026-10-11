# Elefante

Elefante is a small, modifiable coding-agent harness. Its current implementation is a
Rust command-line agent loop with a provider-neutral core, OpenRouter streaming transport,
seven guarded tools, and policy enforced in code rather than in a long system prompt.

The daemon, the OpenTUI client, and a per-repository index have shipped. The index gives each
turn relevant files and symbols and gives the model a `memory_search` tool. The long-term design
adds learned memory, adversarial critics, and token analysis, which are deliberately
milestone-gated.

## Project status

M0, M1 and M2 are complete, with their gates green — 350 hermetic Rust tests and 24 TypeScript
tests, plus three credentialed checks run against a live provider: a real prompt-cache hit through
OpenRouter, a live edit in a scratch repository, and a real model using the repository index it is
sent. See [`docs/MILESTONES.md`](docs/MILESTONES.md). CI runs the hermetic gates on every push and the
credentialed tests on dispatch. M3 — learned memory — is complete with its gate green (486 Rust tests, 67 TypeScript tests) and its
credentialed check, `live_learned_memory_is_extracted_stored_and_used`, passing against a live provider
on its second run.

## Quick start

Requirements:

- Rust 1.90 or newer
- Bun, for the interactive TUI only — the headless CLI does not need it. See `tui/README.md`
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

Environment defaults are `OPENROUTER_MODEL` (default `anthropic/claude-sonnet-5`),
`ELEFANTE_EFFORT`, `OPENROUTER_BASE_URL` (default `https://openrouter.ai/api/v1`), and
`ELEFANTE_MEMORY` (set to `0` to switch the repository index off entirely: nothing is indexed,
nothing is written under `.elefante/`, and no context is added to a turn).
Command-line values take precedence where both forms exist. Models are named by OpenRouter
slug, so reaching a different vendor is a different slug rather than a different transport.

### Environment file

Elefante reads configuration from the process environment; it does not load a `.env` file
itself. Keep one for your shell to source, structured like this:

```sh
# Required. https://openrouter.ai/keys
OPENROUTER_API_KEY=sk-or-v1-...

# Optional — all have defaults.
OPENROUTER_MODEL=anthropic/claude-sonnet-5
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
common credential shapes. A command the classifier escalates suspends and waits for an
explicit approval; one it denies never runs at all.

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
runtime data flow, the wire contract, data models, safety invariants, extension workflows, and
testing. The [design of record](docs/PLAN.md) describes future milestones; the
[milestone record](docs/MILESTONES.md) reports what has actually passed its gate.

If you are about to edit one crate rather than understand the system, read that crate's own
`README.md` — each carries its module map, entry points, invariants, and how to test it
alone.
