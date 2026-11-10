---
id: DIRASH-0050
title: Updates apply in the background by default; dirad schedules, the CLI performs
status: active
guards:
  - cli/dirad/src/autoupdate.rs
checks:
  - channel resolution and the rollback/keep-previous swap :: cargo test -p dira --bin dira -- channel_tests previous_tests
  - the updater log stays capped :: cargo test -p dirad --lib autoupdate
origin: session
verified: false
---

## Decision

`dirad` keeps the clock for updates. Ten minutes after it starts, then every six hours (±30 min
jitter), it spawns a detached, windowless `dira update --auto`, with output appended to
`logs/update.log`. It does nothing else. The CLI does the work through the same path as a
hand-run `dira update`:

1. resolve on the channel;
2. download and check the sha256;
3. swap by rename (D-0003);
4. probe the new binary;
5. restart the daemon through its supervisor with confirmed exit (D-0019);
6. refresh cloud wiring through the new binary (DIRASH-0038).

`--auto` adds these rules:
- It acts only on a release **strictly newer** than the installed one, and never on
  `update.skip_version`.
- One run at a time, under an `update.lock` beside the store.
- It is a silent no-op when `update.auto = false`, `DIRA_DISABLE_AUTOUPDATE` or `CI` is set, in
  debug builds, and on development installs (D-0004 refuses those).

The channel is `--channel`, else `update.channel`, else stable. In background mode only, an unset
knob falls back to the installed version's channel, so a prerelease install keeps following
prereleases. A hand-run `dira update` keeps the stable default and its downgrade refusal. An
explicit `--channel` is persisted.

After every successful update the replaced binaries are kept as `.dira-update/{name}.prev`.
`dira update --rollback` swaps them back with the same rename-only swap, restarts the daemon, and
adds the version it left to `update.skip_version`. A rollback can itself be rolled back.

## Why

The user asked for Dira to be "almost transparent" and to update in the background like Claude
Code. Before this, an update needed someone to read a notice and run a command. Most fleets drift
for weeks, and every Windows fix in this release would have reached only the users who updated
by hand.

D-0006 rejected "have `dirad` check on its heartbeat". Its concern was a *notice*: that design
needed a protocol field and gave no notice while the daemon was down. Applying updates is a
different job. The daemon is the only always-running process, and keeping the work in the CLI
keeps the daemon crate free of any download/verify/swap logic. D-0006's foreground rule stands:
no user-facing command waits on the network for this, and the passive notice still serves people
who turn auto-update off.

The updater must outlive the daemon it restarts:
- unix: its own process group (launchd kills the job's group);
- systemd: the unit now sets `KillMode=process` (the default `control-group` would kill it the
  moment it restarted the unit);
- windows: it asks for `CREATE_BREAKAWAY_FROM_JOB` and falls back without it.

## Rejected

- **Apply updates inside `dirad`.** It drags download, verify and swap into the daemon, and a
  daemon cannot cleanly replace and restart itself under every supervisor.
- **Update from the hook shim.** The shim is on the agent's hot path and must stay a fast
  fire-and-forget.
- **Auto-update off by default.** That would keep today's drift.
- **Reinstall the same version in background mode.** A plain `dira update` does this on purpose,
  to repair an install. In the background it would restart the daemon every six hours for
  nothing.

## Agent directives

- Never add network I/O to `dirad` for updates. It only spawns `dira update --auto`.
- `--auto` must stay a no-op unless a strictly newer, non-skipped release exists, and must never
  prompt.
- Keep `.dira-update/{name}.prev` after every successful swap. `--rollback` depends on it.
- Not yet observed end to end on a real install (`verified: false`). It needs a released build
  older than the next one.

## Follow-ups

- Sign release artifacts (minisign over `checksums.txt`, public key embedded in `dira`) and
  require a valid signature before an unattended apply. A separate PR, per the user's choice on
  2026-11-09.
