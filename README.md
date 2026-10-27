# Elefante

Elefante is a small, modifiable coding-agent harness. Its current implementation is a
Rust command-line agent loop with a provider-neutral core, OpenRouter streaming transport,
eight guarded tools, and policy enforced in code rather than in a long system prompt. Every shell
command runs in an operating-system sandbox.

The project goes deep on four pillars: security, sandboxing, memory (including isolated
subagents) and adversarial review.

## Project status

The agent loop, the daemon, the TUI, the repository index and learned memory have shipped. The OS
sandbox is in progress on the `sandboxing` branch. [`docs/PLAN.md`](docs/PLAN.md) lists the
remaining milestones and their gates. Pull requests and `git log` record what each milestone
shipped. CI runs the hermetic gates on every pull request and on pushes to `main`, and the
credentialed tests on manual dispatch.

## Quick start

Requirements:

- Rust 1.90 or newer
- Bun, for the interactive TUI only — the headless CLI does not need it. See `tui/README.md`
- An OpenRouter API key supplied through the process environment
- macOS: nothing more. The shell sandbox uses the system's `/usr/bin/sandbox-exec`.
- Linux: `bwrap` (bubblewrap) in `/usr/bin`, `/bin`, `/usr/local/bin` or `/opt/homebrew/bin`,
  and unprivileged user namespaces. On Ubuntu 24.04, AppArmor restricts those namespaces until
  you lift the restriction:

  ```sh
  sudo apt-get install -y bubblewrap
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
  ```

Without a working sandbox, every shell command fails with the reason and these steps.
`ELEFANTE_SANDBOX=0` opts out: shell commands then run unsandboxed, and each one needs your
approval. The daemon reads the switch when it starts, so stop a running daemon before you change
it.

Run the agent from the repository it should be allowed to modify:

```sh
export OPENROUTER_API_KEY="..."
cargo run -p elefante -- "Inspect this repository and summarize its structure."
```

When no prompt argument is supplied, Elefante reads the prompt from standard input:

```sh
printf '%s\n' 'Explain the current test strategy.' | cargo run -p elefante
```

A run with a prompt cannot ask you anything, so it denies every approval. In a terminal,
`cargo run -p elefante` with no prompt opens the TUI, which asks before each escalated command.

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

You can skip the file entirely: launched with no key, the TUI asks for one and the daemon stores it
in `~/.elefante/credentials.toml` at mode `0600`, outside every working tree. An exported
`OPENROUTER_API_KEY` still wins.

## Safety model

Every shell command runs in an operating-system sandbox: Seatbelt on macOS, bubblewrap and
seccomp on Linux. Inside it, a command:

- can write only to the working directory and a private temp directory;
- can read system directories and your Cargo, Bun and rustup toolchains, but not the rest of your
  home directory, secret files such as `.env` or `*.pem`, or Elefante's own state;
- cannot write to `.git`;
- has no network, unless you approve network for that one command;
- never sees the API key, and cannot reach the daemon's socket.

The sandbox fails closed: when Elefante cannot apply it, the shell runs nothing. Git writes such
as `git commit` run through the harness, after your approval, with hooks off. The `web_search`
tool lets the model research without giving the shell any network. The first search of a session
asks for your approval; after that, 29 more run without asking. A run with a prompt denies every
approval, so a headless run cannot search.

The file tools also stay inside the working directory and refuse known secret paths. The shell
classifier still parses each command first: a command it escalates waits for your approval, and
one it denies never runs. Every tool result passes a scrubber for common credential shapes.

Code you run yourself later, such as your own build or editor tasks, is outside this guarantee.
`docs/DEVELOPER_GUIDE.md` § 9 lists the remaining risks.

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
testing. The [design of record](docs/PLAN.md) describes the milestones still ahead.

If you are about to edit one crate rather than understand the system, read that crate's own
`README.md` — each carries its module map, entry points, invariants, and how to test it
alone.
