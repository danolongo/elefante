# Lockfile — why there is exactly one

`tui/bun.lock` is authoritative. `package-lock.json` was removed on 2026-09-06. Do not
reintroduce it, and do not add `bun.lock` to `.gitignore`. Both failure modes are silent.

## The rule

**One committed lockfile, and it is the one the tool you actually run reads.** Everything in
this project runs through Bun — `bun test`, `bun run src/main.tsx`, `bun build --compile` —
and npm cannot produce a working client regardless: `@opentui/core` loads its native library
through Bun's FFI (`bun-ffi-structs`), so Node cannot execute this code at all. An npm
lockfile was pinning dependencies for a program only Bun can run.

## What was measured

Bun 1.4.2, in scratch copies of `tui/`. Three results, in order of how much they matter.

**1. `bun.lock` completely shadows `package-lock.json`.** Not deprioritized — never read.
With a valid `bun.lock` present and `package-lock.json` replaced by the literal bytes
`THIS IS NOT JSON {{{`:

```
bun install --frozen-lockfile   → exit 0, 26 packages installed, react 19.2.8
```

The first `bun install` in a tree with only `package-lock.json` prints
`migrated lockfile from package-lock.json` and writes `bun.lock`. Every subsequent install
prints no migration line. Migration happens once, then the npm lockfile is inert.

**2. With both present and disagreeing, both tools report success and install different
trees.** Same directory, both lockfiles valid, both satisfying `package.json`'s `^19.2.0`:

| | pins | result |
|---|---|---|
| `package-lock.json` | react 19.2.0 | `npm ci` → exit 0, react **19.2.0** |
| `bun.lock` | react 19.2.8 | `bun install --frozen-lockfile` → exit 0, react **19.2.8** |

Neither warns. There is no ambiguity to notice at the terminal — each tool reports a
successful, reproducible, frozen install while producing a different `node_modules`.

**3. `--frozen-lockfile` does enforce, against whichever lockfile wins.** Editing
`package.json` to want `react@^18` while the lockfile pinned `19.2.8`:

```
error: lockfile had changes, but lockfile is frozen
exit code 1, nothing installed
```

This is worth stating because it is the part that works. The hazard is not that
`--frozen-lockfile` is weak; it is that it faithfully enforces a file you may not have
realised it chose.

## Why gitignoring `bun.lock` is worse than it looks

The tempting alternative — keep `package-lock.json` committed, add `bun.lock` to
`.gitignore` — is broken by result 1, and breaks in the direction that is hardest to see.

`bun.lock` would still exist on every machine that has ever run `bun install`. Being
untracked does not stop it shadowing the committed lockfile. So a dependency bump committed
to `package-lock.json` has **no effect** on any developer holding a stale `bun.lock`: their
next `bun install` reads the local file, finds no changes, and reports success. CI, cloning
fresh with no `bun.lock`, migrates from `package-lock.json` and gets the intended tree.

CI is then correct and every developer silently wrong, with `--frozen-lockfile` reporting
green on both sides. That is precisely the failure a lockfile exists to prevent, manufactured
by the arrangement meant to avoid committing a second one.

## Why this was safe until now, and why that is not a design

Before the change the repo tracked only `package-lock.json`, and `tui/.gitignore` listed only
`node_modules/`. `bun.lock` was therefore neither tracked nor ignored, so it surfaced as an
untracked file in `git status` — visible, and not yet committed by anyone. The tree was one
`bun install` plus one reflexive `git add` away from the two-lockfile state on any machine.

That is an accident holding, not a decision. This document is the decision.

## If this ever needs to change

Reintroducing npm as a supported install path requires first answering: what runs the client?
Node cannot. If a future OpenTUI release drops the FFI dependency, or the TUI moves to a
runtime-agnostic renderer, revisit this — and at that point commit **one** lockfile for
whichever tool the project actually invokes, not both.

If Bun ever gains the ability to read `package-lock.json` as authoritative without migrating,
that also changes the analysis. It did not in 1.4.2.

## Related

- CI installs with `bun install --frozen-lockfile` (`.github/workflows/ci.yml`), which is
  meaningful only because the lockfile it verifies against is the one committed here.
- Bun distribution — whether the client ships as a compiled binary rather than requiring Bun
  on PATH — is [`M1-PROTOCOL.md`](./M1-PROTOCOL.md) open question 2. That decision does not
  affect this one: `bun build --compile` bundles from the same resolved tree.
