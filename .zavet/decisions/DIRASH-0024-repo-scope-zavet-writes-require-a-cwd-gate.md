---
id: DIRASH-0024
title: Repo-scope zavet writes are gated on cwd, and dira never sets core.hooksPath
status: active
guards:
  - cli/dira/src/zavet_adapters.rs
  - cli/dira/src/zavet_install.rs
origin: recorded
verified: false
---

## Decision

`dira zavet install` may refresh the current repo's zavet adapters, but only
after dira itself has established, **before spawning zavet at all**, that:

1. cwd resolves to a git toplevel, and
2. that toplevel carries `.zavet/`, and
3. the installed plugin reports `version >= 1.3.0` from `zavet version --json`.

Only then may `zavet adapters --check` run, and only a non-zero exit from it
may trigger a write. `--no-adapters` opts out entirely.

dira **never** runs `zavet hooks install` and never writes `core.hooksPath`.
It reports that the git-hook floor is inactive and stops there.

## Why

`dira zavet install` is machine-scope: user scope by default, valid to run from
`$HOME`, and it neither knows nor cares which repo you are standing in.
`zavet adapters` is the exact opposite — it writes *tracked, committed* files
into whatever working tree it finds. Folding one into the other unconditionally
means a global install command silently rewriting committed files in an
unrelated repo.

Both gates are load-bearing, not defensive, and both were confirmed against real
binaries rather than inferred:

- Run from a non-repo directory, `zavet adapters --check` prints
  `zavet: not inside a git repository`, then reports **all six artifacts
  missing** and exits 1. Without dira's own repo gate, a `--update` from `$HOME`
  would treat that as "stale" and write adapter files into the home directory.
- zavet **1.2.0 has no `adapters` subcommand at all**. Asked for
  `adapters --check` it prints its general usage text and exits **1** — the same
  exit code 1.3.0 uses for "stale". Exit codes therefore cannot feature-detect
  here, and a version guard is the only correct discriminator.

The binary must be the *plugin root's* `bin/zavet`, never the repo's vendored
`.zavet/bin/zavet`: the vendored copy is precisely the artifact being
regenerated, so using it would ask a stale tool whether it is stale.

Git hooks are excluded because `core.hooksPath` is a single, exclusive setting
also owned by Husky, lefthook and pre-commit. zavet itself refuses to take it
over and prints a delegation line instead; dira silently claiming it would be
strictly worse than the tool that owns the feature.

## Rejected

- **Refresh adapters unconditionally after a successful install** — turns a
  machine-scope command into one that mutates whichever repo you happen to be
  standing in. The saved step is not worth a surprise diff in an unrelated tree.
- **Feature-detect `adapters` by exit code** — 1.2.0 and "stale" are both exit
  1, so this silently treats every pre-1.3.0 install as a repo needing a write.
- **Let `zavet adapters` do its own repo detection** — it does, and it still
  reports everything missing outside a repo. Its contract is "generate here",
  not "decide whether here is appropriate"; that judgement belongs to the
  machine-scope caller.
- **Run `zavet hooks install` too, to finish the job** — silently seizes
  `core.hooksPath` from whatever hook manager the repo already uses.

## Agent directives

- Never invoke `zavet adapters` (or any repo-writing zavet subcommand) without
  first passing the three-part gate above. `RepoGate` exists to make that
  unskippable — construct it, do not hand-roll the check.
- Never call `zavet hooks install` from dira, and never write `core.hooksPath`.
  Reporting the floor as inactive is the whole contract.
- Resolve the zavet binary from the plugin root, never from `.zavet/bin/zavet`.
- Any new repo-scoped zavet capability inherits this gate. If a test can pass
  with zero commands stubbed on a `NotGit` gate, keep it that way — that
  property is what proves nothing was spawned.
