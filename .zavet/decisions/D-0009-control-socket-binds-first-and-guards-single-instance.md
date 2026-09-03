---
id: D-0009
title: The control socket binds first and is the single-instance guard
status: active
guards:
  - cli/dirad/src/lib.rs
origin: recorded
verified: true
---

## Decision

`run()` binds the UDS control socket before the HTTP ingress port.
`bind_control_socket` refuses to start when a live daemon already holds the
path, and only unlinks a socket nothing answers on. An unavailable ingress
port is no longer fatal: the daemon runs degraded, reports why over
`DaemonInfo`, and retries the bind in the background.

## Why

The old order bound the TCP port first and returned `Err` on conflict, so a
duplicate daemon died *before* binding the control socket. Every client and
every supervision probe therefore saw "down" while a healthy daemon was
running, and launchd's `KeepAlive` respawned the loser every 10s indefinitely.

The reorder alone would have been worse than the bug. The old code unlinked
the socket path unconditionally, and the TCP bind was the *accidental* mutual
exclusion that stopped a second daemon from ever reaching that unlink. Binding
the UDS first without a real guard means a duplicate steals the path from a
healthy daemon, whose listener then survives on an unlinked inode: alive,
holding the DB, reachable by nobody, and silent about it. The explicit guard
is the prerequisite that makes the ordering safe, not a nicety alongside it.

`connect` is the liveness probe rather than a protocol `Ping`: a stale file
gives `ECONNREFUSED`, and dirad has no control client of its own.

Degraded-with-retry (not exit) is what actually ends the respawn loop. There
is nothing left for a supervisor to restart, and the daemon reclaims the port
by itself when the squatter goes away.

## Rejected

- **Reorder only**. Silently breaks a healthy daemon. See above.
- **Exit with a nicer message**. Still a `KeepAlive` loop, and still leaves
  every client unable to tell a port conflict from a dead daemon.
- **A pidfile lock**. A second source of truth that goes stale on SIGKILL;
  the socket already answers the exact question being asked.

## Amendment: the reclaim is serialized by an flock (2026-07-22)

The probe→unlink→bind sequence is not atomic on its own. Two daemons racing
onto a *stale* socket file can both observe "nothing answers" before either
unlinks; the loser's unlink then steals the path from the winner's freshly
bound listener. The same orphaned-listener failure through a second door.
The original incident was masked from this race because launchd serializes
its own spawns, but a manual start racing a `KeepAlive` respawn is enough.

`bind_control_socket` therefore takes an exclusive non-blocking `flock(2)` on
`<sock>.lock` *before* the probe and holds it for the daemon's lifetime
(`SocketLock`, kept alive in `run()`). A second daemon fails the lock and
exits with the same "already running" message.

This does not reopen the rejected pidfile-lock option: a pidfile goes stale
on SIGKILL because its *content* is the source of truth, while an `flock` is
kernel-held and released on any exit, SIGKILL included. It cannot go stale.
The `connect` probe stays, both because a pre-lock daemon holds the socket
without holding any lock, and because the socket still answers the "is it
alive" question the lock cannot.

## Agent directives

- Never bind the ingress port before the control socket, and never make an
  ingress bind failure fatal.
- Never unlink the control socket path without first proving nothing answers
  on it, and never probe/unlink/bind outside the `<sock>.lock` flock: the
  probe detects a live daemon, the lock makes the reclaim race-free.
- Never release `SocketLock` while the daemon still serves the socket, and
  never turn the flock into a pidfile (its content must stay meaningless).
- Any new startup step that can fail must either bind after the control
  socket or be survivable; the control socket must stay the first thing up
  and the last thing lost.
- When adding a degradation, expose it on `DaemonInfo`. A daemon that
  cannot do its job must never report as plainly healthy.
