---
id: D-0003
title: Replace running binaries by rename only, never by opening the target
status: active
guards:
  - cli/dira/src/update/replace.rs
  - install.sh
origin: recorded
verified: true
---

## Decision

An update stages the new binary inside the destination directory as a
temporary file, then `rename(2)`s it over the target. The target path is
never opened for writing. `dirad` is renamed before `dira`.

## Why

Replacing an executable that is currently running is legal on Unix because
the running process holds the *inode*, not the path: it keeps executing the
old image until it exits, and the next invocation picks up the new one.
Opening that same path for writing is a different syscall and fails with
`ETXTBSY` on Linux. This is the trap: a `File::create`-based implementation
passes every test (nothing is running under test) and then fails in
production on exactly the platform most users are on.

Staging must happen in the destination directory, not `$TMPDIR`, so the
rename is same-filesystem and therefore atomic. A cross-device `fs::rename`
fails `EXDEV`, and the natural copy fallback is not atomic — it can leave a
half-written binary where a working one used to be.

Order matters for the same reason: dying between the two renames leaves a
new `dirad` under an old `dira`, which is the state `dira version` already
detects and warns about. The reverse looks like success.

## Rejected

- **Write directly to the target path** — `ETXTBSY` on Linux, and
  non-atomic everywhere.
- **Stage in `$TMPDIR` and rename across filesystems** — `EXDEV`, and the
  fallback copy reintroduces the torn-write window.
- **Stop the daemon first, then write** — makes the update path depend on a
  successful shutdown, and still leaves `dira` replacing itself.

## Agent directives

- Never `File::create`, `OpenOptions::write`, or truncate a path that is or
  might be an installed binary. Only `rename` onto it.
- Keep the staging file in the same directory as its target.
- Preserve the `dirad`-then-`dira` order, and keep the hard-linked `.bak`
  copies until the post-restart health check passes.
- Verify by running the installed binary's `--version` as a subprocess before
  tearing down the daemon — it catches a wrong-target download early.
