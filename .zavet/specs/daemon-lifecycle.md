---
title: Daemon startup and ingress lifecycle
version: 3
origin: session
verified: false
confidence: high
date: 2026-07-31
paths:
  - cli/dirad/src/lib.rs
  - cli/dirad/src/control.rs
  - cli/dira/src/daemon.rs
  - cli/ipc/**
decisions: [D-0008, D-0009, D-0010, D-0016]
---

## Overview

How `dirad` comes up: which surfaces it binds, in what order, what happens when
one of them cannot bind, and how it refuses to run twice. The ordering is not
incidental — it decides whether a failing daemon can still be diagnosed.

The control channel is platform-split behind `dira-ipc`: a Unix domain socket
on unix, a named pipe on windows (D-0010). Everything below about socket
files, `flock`, and stale-path reclaim is the unix arm; on windows the pipe's
`first_pipe_instance` bind is atomically the single-instance guard, and no
filesystem state exists to go stale.

Both arms are **explicitly** permission-gated, and the gate is never inherited:
unix chmods the socket to `0600`, windows creates every pipe instance with a
user-only DACL plus a medium integrity label (D-0016). The channel carries
`Nuke`/`Shutdown`/`IngestHook` with no auth of its own, so that gate is the
whole of its access control. Windows previously passed a NULL descriptor and
inherited the creating token's default, which for an elevated daemon excluded
the interactive user entirely — a silent, total capture outage.

## Behavior

- `run()` loads config, opens the store, builds state, then binds — in order —
  the control channel, then the loopback HTTP hook ingress. Both bind before
  hydration, so `Ping`/`Status` answer immediately and a status during warm-up
  reports `hydrating: true`.
- `bind_control_socket` creates the parent directory, takes an exclusive
  non-blocking `flock(2)` on `<sock>.lock` (held for the daemon's lifetime via
  `SocketLock`), then probes an existing socket file by `connect`. A held lock
  or a live listener means another daemon owns it: the start is refused and
  the file is left alone. `ECONNREFUSED` means the file is a leftover from a
  dead daemon and is reclaimed. The flock is what makes the reclaim race-free:
  without it, two daemons racing onto a stale file could both pass the probe
  and the loser's unlink would orphan the winner's fresh listener.
- An ingress port that cannot be bound is **not** fatal. The reason is recorded
  in `AppState::http_ingress_error`, logged at ERROR, and retried in the
  background with exponential backoff (`HTTP_RETRY_BASE` → `HTTP_RETRY_MAX`)
  until the port frees, at which point the ingress starts and the field clears.
- While degraded the daemon answers every control request but captures nothing
  over HTTP hooks. `dira daemon status` renders `up, DEGRADED` plus the reason;
  `dira version` prints a warning line.
- `dira daemon restart` (and the restart inside `dira update`) detects a bare
  pre-D-0008 daemon on the legacy `$TMPDIR` socket, kills it, removes its
  socket and pidfile, and starts a fresh daemon on the fixed path. If its pid
  cannot be determined, restart errors with manual instructions instead of
  starting a second daemon beside it.
- `dira daemon status` exits 0 when any daemon is running (healthy, degraded,
  or legacy) and 1 when none is; install.sh keys its restart-after-upgrade
  decision off this. `dira version` and `dira daemon status` both point at a
  legacy daemon when the fixed socket is silent.

## Interfaces & data

- `bind_control_socket(&Path) -> Result<(dira_ipc::Listener, SocketLock)>` and
  `serve_http_ingress(AppState, String, Duration)` are both public so the
  integration tests in `cli/dirad/tests/startup_binding.rs` drive the real
  code paths rather than a copy. Callers hold the `SocketLock` as long as the
  listener serves; `run()` keeps it as a local until shutdown.
- `Listener::security_degradation() -> Option<String>` reports which rung of
  the descriptor ladder the windows bind landed on (labeled → DACL-only →
  default); always `None` on unix. `run()` folds it together with the
  elevation advisory into
  `Response::DaemonInfo.control_channel_warning: Option<String>`. `DEGRADED`
  stays reserved for "captures nothing" — an elevated but reachable daemon is
  an advisory `note:`, not a degradation.
- `Response::DaemonInfo.http_ingress_error: Option<String>` carries the
  degradation to clients. It is `#[serde(default)]`, so a newer `dira` reading
  an older daemon's reply during a partial update sees `None` instead of a
  parse error. This is local IPC (`cli/core/src/protocol.rs`), not the cloud
  contract — it needs no schema regeneration.

## Invariants

- The control socket binds first and an ingress failure never costs it
  (D-0009). It is the surface every client and supervision probe depends on.
- The control socket path is never unlinked without first proving nothing
  answers on it, and never outside the `<sock>.lock` flock (D-0009) — the
  probe detects a live daemon, the lock serializes the reclaim, and unlinking
  a live path orphans the owner silently.
- The socket path itself is a fixed per-user location, never `$TMPDIR`
  (D-0008), so client and daemon agree without coordination.
- A daemon that cannot do its job never reports as plainly healthy.
- The control channel is permission-gated on every platform, and the gate is
  explicit, never inherited from a process token (D-0016).
- A descriptor failure degrades and is surfaced; it never fails the bind, and
  it is never silent.

## Open Questions

- The launchd plist still sets `KeepAlive: true`, so a daemon stopped
  deliberately via `dira daemon stop` is respawned. `SuccessfulExit: false`
  would respect an orderly shutdown, but changes restart semantics for every
  other exit path and was left alone here.
- The ingress retry runs forever. A daemon whose configured port is
  permanently owned by something else stays degraded indefinitely, visible
  only via `dira daemon status` — there is no escalation beyond the log line.
