---
id: D-0008
title: The control socket is anchored to a fixed per-user path, never $TMPDIR
status: active
guards:
  - cli/core/src/config.rs
origin: recorded
verified: true
---

## Decision

`default_socket_path` resolves `$XDG_RUNTIME_DIR/dira.sock`, else
`<data_dir>/dira.sock` beside the database. `std::env::temp_dir()` survives
only as a last resort when no project dirs resolve at all.

## Why

The daemon and every client must agree on this path with zero coordination.
Whoever binds it and whoever dials it are separate processes started by
different parents. `$TMPDIR` cannot carry that contract: on macOS launchd
hands an agent the per-user Darwin temp dir, a login shell inherits the same,
but an agent sandbox, a cron job or a container each get a different one. The
observed failure was a healthy launchd-supervised daemon that `dira` reported
as `down`, because the client's `$TMPDIR` was `/private/tmp` while the daemon
had bound under `/var/folders/…/T/`. Nothing logged an error. Both sides were
behaving correctly against different paths.

`$XDG_RUNTIME_DIR` stays first because on Linux it is the conventional home
for per-user sockets and is already per-user and session-scoped. The data dir
is the fallback precisely because it is the one location both sides already
agree on. The database lives there.

## Rejected

- **Keep `$TMPDIR`, document it**. The failure is silent and presents as
  "daemon down", which is the single most misleading thing it could say.
- **Have clients fall back to the legacy path when the real one is quiet**.
  Makes `$TMPDIR` load-bearing again forever and reintroduces the ambiguity.
  The legacy path is probed *only* to print a "restart it" hint.
- **A hardcoded `/var/run` or `/tmp/dira-$UID.sock`**. Needs root or invents
  a private convention where an XDG one already exists.

## Agent directives

- Never resolve `socket_path` (or any other rendezvous path) through
  `std::env::temp_dir()`, `$TMPDIR`, or the cwd.
- Keep `default_socket_path`'s inputs injected. The branches are only
  testable from one machine because it does not read the environment itself.
- Keep the resolved path under ~104 bytes (`sun_path`), and keep the daemon's
  `create_dir_all` before `UnixListener::bind`. The data dir may not exist
  on a first run.
- `legacy_socket_path` is transitional. Delete it once no pre-D-0008 daemon
  can still be alive. It must never carry capture traffic or serve as a
  client fallback; lifecycle commands (`daemon status`, `daemon restart`,
  `dira version`) may probe it to detect a pre-D-0008 daemon and to locate
  its pid so the old process can be terminated and replaced. Migration,
  never rendezvous.

## Amendment (2026-07-22)

The last directive originally read "never use it to actually talk to a
daemon". That wording blocked the fix for a hole this record did not
foresee: a *bare* pre-D-0008 daemon is invisible to `detect_supervision`
(its pidfile and socket both live under the old `$TMPDIR` anchor), so the
"restart it" hint this record prescribes invoked a command that was a
no-op, `dira update` printed a false "daemon restarted", and a later
`daemon start` could put two live daemons on one database. Lifecycle
commands may now send control requests (`Ping`/`DaemonInfo`) over the
legacy path solely to find and kill the old daemon. The rejection above is
unchanged: the legacy path never carries hook capture and is never a
client fallback. `dira daemon status` exits 0 when a daemon answers on
either path. The exit code answers "will a restart accomplish something",
which is what install.sh asks of it.
