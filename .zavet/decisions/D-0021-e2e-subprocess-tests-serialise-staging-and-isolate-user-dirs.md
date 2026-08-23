---
id: D-0021
title: Tests that exec the real binary serialise staging against forks and isolate every user dir
status: active
guards:
  - cli/dira/tests/update_e2e.rs
checks:
  - the exec-staging lock still exists :: grep -qF 'static EXEC_STAGING: Mutex<()>' cli/dira/tests/update_e2e.rs
  - staging still holds the lock :: grep -qF 'let _staging = lock_staging();' cli/dira/tests/update_e2e.rs
  - no fork bypasses the lock :: sh -c '! grep -nE "\.(output|status)\(\)" cli/dira/tests/update_e2e.rs'
  - the suite is clean and repeatable :: cargo test -p dira --test update_e2e
origin: recorded
verified: true
---

## Decision

Any test that writes an executable and then execs it holds `EXEC_STAGING`
across the write **and** across every `spawn` in the file. That means using
`output_staged` / `status_staged`, never bare `Command::output()` /
`status()`. That includes forks unrelated to the binary under test, such as
`tar` building the fixture archive.

The same tests redirect `HOME` and the `XDG_*_HOME` trio into their own
tempdir through `isolate_user_dirs`.

## Why

Two independent ways this suite corrupts something outside itself, both of
which have actually happened:

**ETXTBSY (#80).** `execve`'s busy check is per *inode*, not per path. Thread A
opens `A/bin/dira` to stage it. Thread B forks, and its child inherits A's
write fd. `O_CLOEXEC` only applies on a *successful* exec, so the fd survives
and pins the inode until the child exits. A then execs and gets `ETXTBSY`.
Per-test tempdirs do not help, and neither does write-then-rename: renaming
gives the inode another name while the inherited fd still refers to it. The
window has to close on the fork side.

`6d29a12` fixed this, and `8ae49e1`, an unrelated rewrite of the same file,
deleted the static and all three helpers. The race then sat live on `develop`
until it failed a PR days later. Nothing caught the deletion because the
original fix was verified "by construction plus 25 green runs", which is not
something CI can re-run.

**Real user directories (#90).** `--bin-dir` bounds where an update writes
*binaries*, not where the CLI keeps state. The update-check cache resolves from
the real `$HOME` via `project_dirs()` and is reachable by no CLI flag, so
`dira update --check` wrote the mock's fixture tag into the developer's own
cache. Every later real `dira` run on that machine then advertised
`42.0.0 is available`. That release does not exist. The suite was green
throughout; the damage only showed up on an unrelated invocation.

## Rejected

- **Retry on ETXTBSY**: hides the race and would mask a genuine regression in
  the replacement path, which is the exact failure this suite exists to catch.
- **`--test-threads=1`**: narrows the window without closing it, and any other
  parallel test that writes an executable reopens it.
- **Write-then-rename alone**: see above; wrong side of the race.
- **Leaving the user-dir redirect to individual tests**: that is the state that
  produced #90; two of the four vars were set at one site and none at another.

## Agent directives

- Never call `Command::output()` or `Command::status()` in this file. Use
  `output_staged` / `status_staged` so the fork happens under the lock.
- Never hold the lock across the *wait*, only the spawn. Holding it longer
  serialises the whole suite.
- A new env var that steers a **write** path belongs in `isolate_user_dirs`,
  not inline in one test.
- When rewriting this file wholesale, re-read this record first: its
  protections are invisible in a diff that simply replaces the helpers.

## Verification

The `checks:` above are greps plus the suite itself, deliberately: the ETXTBSY
race cannot be reproduced on demand (it is timing-dependent and macOS never
hits it), so a behavioural test would be vacuous. Asserting the lock's
*existence* and that no fork bypasses it is checkable and would have failed the
moment `8ae49e1` deleted it. That is the failure mode that actually occurred.

The user-dir isolation IS reproducible and was confirmed both ways: with the
redirect removed, one test run leaves `{"latest":"42.0.0"}` in the real cache;
with it in place the file stays absent. That check is not automated here
because asserting on the developer's real `$HOME` from a test would itself be
the hazard being prevented.
