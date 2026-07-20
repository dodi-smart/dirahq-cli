---
id: D-0004
title: The installer and updater refuse to overwrite a development install
status: active
guards:
  - cli/dira/src/update/replace.rs
  - install.sh
  - justfile
origin: recorded
verified: true
---

## Decision

If the `dira` on the install path is a symlink into `target/release` or
`target/debug`, or the running executable itself lives under `target/`, both
`install.sh` and `dira update` refuse. `--force` overrides the symlink case
only — never a build-tree binary.

## Why

`just install` symlinks `target/release/{dira,dirad}` into `~/.local/bin`, so
a contributor's PATH entry points into their build tree. Silently replacing
that symlink with a released binary destroys their dev loop in a way that is
confusing to diagnose: `cargo build` keeps succeeding, the binary on PATH
just stops changing. Overwriting a file *inside* `target/` is worse — the
next `cargo build` clobbers it, so the update never sticks.

Detection has to work from the PATH entry, not `current_exe()`.
`current_exe()` resolves symlinks on both Linux and macOS, so by the time you
have it the symlink is gone and the dev install is indistinguishable from a
managed one. `symlink_metadata` on the unresolved path is the only thing that
sees it.

## Rejected

- **Overwrite and print a warning** — the warning scrolls past in a `curl | sh`
  and the damage is silent thereafter.
- **Detect via `current_exe()`** — cannot see the symlink at all.
- **Let `--force` override a `target/` binary** — there is no coherent meaning
  to installing into a build directory.

## Agent directives

- Resolve the install location from the PATH entry with `symlink_metadata`;
  never treat `current_exe()` as authoritative for this check.
- Any new install-like path must reuse `discover_install` rather than
  re-deriving the rule.
- Error text must name `just install` as the way to update a dev build, and
  give the exact commands to switch to released binaries.
