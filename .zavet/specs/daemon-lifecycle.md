---
title: Daemon startup and ingress lifecycle
version: 6
origin: session
verified: false
confidence: high
date: 2026-08-09
paths:
  - cli/dirad/src/lib.rs
  - cli/dirad/src/control.rs
  - cli/dira/src/daemon.rs
  - cli/core/src/config.rs
  - cli/ipc/**
decisions: [D-0008, D-0009, D-0010, D-0016, D-0019]
---

## Overview

How `dirad` comes up: which entry points it binds, in what order, what
happens when one of them cannot bind, and how it refuses to run twice. The
ordering is not incidental. It decides whether a failing daemon can still be
diagnosed.

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
the interactive user entirely. That was a silent, total capture outage.

## Behavior

- Startup reports the store it opened. `DaemonInfo` carries `db_path` plus a
  `storage_warning` when `project_dirs()` did not resolve and the store fell
  through to `$TMPDIR`. That is a whole capture history somewhere the OS may
  clear on reboot. `dira` compares the reported path against its OWN
  resolution (`store_divergence_line`) because the elevated / service-account
  case is invisible to either process alone: `project_dirs()` succeeds on
  both sides and simply lands in two profiles. Neither condition is fatal.
  The log sink resolves through the same `project_dirs()` and windows nulls
  stdio, so a bail is invisible exactly where it would fire, and an exiting
  daemon respawn-loops (D-0009). Both fields are `#[serde(default)]` for skew.
- `run()` loads config, then binds the control channel **before opening the
  store**, then opens the store, builds state, and binds the loopback HTTP hook
  ingress. Both binds precede hydration, so `Ping`/`Status` answer immediately
  and a status during warm-up reports `hydrating: true`.
- Control-channel-before-store is load-bearing (D-0019). `Store::open` runs
  `sqlx::migrate!` and can both fail and block, and it used to run *before*
  the single-instance guard. So a duplicate daemon opened and migrated the
  database, and contended for it, before the guard could refuse. That is the
  window in which two processes genuinely hold one database. Binding first
  makes the refusal the earliest, cheapest and only side-effect-free thing a
  duplicate hits, and satisfies D-0009's directive that any startup step
  that can fail must bind after the control socket.
- Nothing accepts on the listener until `serve_control`; that gap already
  existed (the HTTP bind and the hydrate spawn sit inside it) and is only
  extended by the store open.
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
- **No replacement daemon is started until the previous PROCESS is confirmed
  exited** (D-0019), on every windows restart path, bare and scheduled-task
  alike. The sequence is `Shutdown` → wait for process exit → `taskkill /F` →
  wait again → only then start. Failure to confirm exit is a hard error naming
  the surviving pid and the manual command, never a silent extra daemon.
- Exit is decided by the process probe (`tasklist`), never by whether the
  control channel answers. `serve_control` is a detached accept loop nothing
  cancels and the shutdown notify fires only after the response is written, so
  the pipe answers `Ping` for the whole of teardown. "Stopped answering" and
  "exited" are different questions, and the grace budget (15 s) must exceed the
  worst-case teardown (3 s offline beat + 5 s WAL checkpoint) or an orderly
  shutdown gets force-killed mid-checkpoint.
- Teardown logs a final `stopped` line with its total, offline-beat and
  WAL-checkpoint durations. Its absence in a log is itself the signal: the
  daemon was killed before finishing, or is still going.
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
- `Listener::security_degradation() -> Option<String>` reports which level of
  the descriptor fallback the windows bind landed on (labeled → DACL-only →
  default); always `None` on unix. `run()` combines it with the
  elevation advisory into
  `Response::DaemonInfo.control_channel_warning: Option<String>`. `DEGRADED`
  stays reserved for "captures nothing". An elevated but reachable daemon is
  an advisory `note:`, not a degradation.
- `Response::DaemonInfo.http_ingress_error: Option<String>` carries the
  degradation to clients. It is `#[serde(default)]`, so a newer `dira` reading
  an older daemon's reply during a partial update sees `None` instead of a
  parse error. This is local IPC (`cli/core/src/protocol.rs`), not the cloud
  contract. It needs no schema regeneration.

## Invariants

- `resync --from <id>` rewinds ONLY the event cursor; the artifact and token
  rowid cursors deliberately stay put (D-0018/D-0020), and the response says so.
  A rewind that under-reports what it moved is the same class of defect as one
  that moves the wrong thing.
- `ResyncQueued` reports the event backlog and the token backlog as separate
  numbers. They are never combined into one counter. A single counter
  spanning two streams is worse than the under-count it would replace.
- A failed backlog count is logged, never collapsed to a bare `0`: silently
  reporting "nothing pending" right after a user asked for a rewind is
  indistinguishable from success.
- The control socket binds first and an ingress failure never costs it
  (D-0009). Every client and supervision probe depends on it.
- The control socket path is never unlinked without first proving nothing
  answers on it, and never outside the `<sock>.lock` flock (D-0009). The
  probe detects a live daemon, the lock serializes the reclaim, and unlinking
  a live path orphans the owner silently.
- The socket path itself is a fixed per-user location, never `$TMPDIR`
  (D-0008), so client and daemon agree without coordination.
- A daemon that cannot do its job never reports as plainly healthy.
- The control channel is permission-gated on every platform, and the gate is
  explicit, never inherited from a process token (D-0016).
- A descriptor failure degrades and is reported; it never fails the bind, and
  it is never silent.

## Open Questions

- The launchd plist still sets `KeepAlive: true`, so a daemon stopped
  deliberately via `dira daemon stop` is respawned. `SuccessfulExit: false`
  would respect an orderly shutdown, but changes restart semantics for every
  other exit path and was left alone here.
- The ingress retry runs forever. A daemon whose configured port is
  permanently owned by something else stays degraded indefinitely, visible
  only via `dira daemon status`. There is no escalation beyond the log line.
