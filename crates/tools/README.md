# elefante-tools

The syscall boundary. This crate owns the seven permanent tool schemas and the executor that
runs them. Every filesystem operation resolves through `PathPolicy`; every result — success or
failure — passes through `Scrubber` on its way out.

## Layout

| File | Lines | Contents |
|---|---:|---|
| `src/lib.rs` | 508 | `ToolExecutor`, the seven implementations, `definitions()`, and the input structs each tool decodes. |

## Entry points

```rust
use elefante_tools::{ToolExecutor, definitions};

let tools = ToolExecutor::new("/path/to/repo")?;   // roots PathPolicy here
let result = tools.execute("read", json!({ "path": "src/main.rs" })).await;

let catalog = definitions();   // 7 schemas, alphabetical, for the provider request
```

`ToolExecutor` is cloneable and holds the policy and scrubber, so cloning it cannot accidentally
produce an executor rooted somewhere else. `definitions()` is sorted alphabetically because a
stable catalog order keeps the prompt cacheable.

The seven tools: `bash`, `edit`, `glob`, `grep`, `memory_search`, `read`, `write`.
`DEVELOPER_GUIDE.md` §8 documents each one's arguments and behavior.

## Invariants

**Scrubbing happens at the single exit point, not per tool.** `execute` matches on the tool
name, then rebuilds the result with `self.scrubber.scrub(&result.content)` before returning
(`src/lib.rs:114`). A new tool therefore cannot forget to scrub — the structure makes it
impossible rather than relying on the author remembering. Preserve that shape when adding one.

**Every path argument goes through `self.resolve`, which delegates to `PathPolicy`.** `glob` and
`grep` also filter their *results* through the policy, so a traversal or a symlink into a secret
cannot leak through a listing even when the caller never named the path.

**`bash` does not classify its own command, and that is intentional.** Classification lives in
`core::ToolGate`, which runs before dispatch and can suspend for approval; a gate inside the
executor would have nowhere to wait. This crate spawns the process — `bash -lc`, rooted at the
policy root, `kill_on_drop`, its own process group on Unix so an interrupt takes the children
with it. Do not reintroduce a classifier here.

**`memory_search` returns a hardcoded refusal string** (`src/lib.rs:375`) rather than
`elefante_mem::NOT_AVAILABLE`. The two have drifted; see `crates/mem/README.md`. This crate does
not depend on `elefante-mem` at all.

## Testing

```sh
cargo test -p elefante-tools    # 13 tests
```

`tests/tools.rs` covers all seven tools against `tempfile` workspaces — real files, real
`bash`, no network. Nothing is `#[ignore]`d.

## Boundaries

Do not add an alternate read, write, process, glob, or search path that bypasses this crate's
policy calls. The point of a single syscall boundary is that auditing it means auditing one
file; a second path means the audit is worthless even if the second path happens to be correct
today.

This crate depends on `guard` and nothing else in the workspace. It performs; `guard` decides.
