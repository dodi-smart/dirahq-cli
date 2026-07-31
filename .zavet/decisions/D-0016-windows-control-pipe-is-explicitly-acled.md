---
id: D-0016
title: The windows control pipe carries an explicit user-only ACL and a medium integrity label
status: active
guards:
  - cli/ipc/src/security.rs
  - cli/ipc/src/elevation.rs
  - cli/ipc/src/listener.rs
origin: session
verified: false
confidence: high
---

## Decision

`dira_ipc::Listener::bind` creates the windows named pipe with an explicit
security descriptor — `D:P(A;;GA;;;SY)(A;;GA;;;<user SID>)S:(ML;;NW;;;ME)` — and
passes that same descriptor to **every** per-accept instance, not just the first.

If the descriptor cannot be applied the bind degrades through DACL-only to
unprotected and reports which rung it landed on via `DaemonInfo`; it never fails
the bind. `dirad` warns when it is running elevated and keeps serving. `dira`
maps `ERROR_ACCESS_DENIED` to its own actionable message instead of "daemon not
running".

`verified: false` until the manual runbook below is executed on a real machine —
the elevated↔non-elevated path cannot be exercised by any automated test we can
run.

## Why

The unix arm has always chmod'd the control socket to `0600`, because
`cli/core/src/protocol.rs` states the channel carries `Nuke`/`Shutdown`/
`IngestHook` with no auth of its own and is "permission-gated by the socket
itself". The windows arm never had an equivalent: `CreateNamedPipeW` was called
with a NULL security descriptor, so the pipe silently inherited whatever the
creating token's default DACL happened to be.

For an *elevated* token that default grants `BUILTIN\Administrators` rather than
the interactive user, and the object additionally carries a High mandatory
integrity label. A medium-integrity `dira hook claude` is then refused twice
over. On one user's machine that produced 303 stored events, **all** of them
`harness='manual'`, and zero agent events ever — while `dira daemon status`
reported a healthy daemon, the loopback ingress was listening, commit capture
worked, and cloud sync stayed green. The denial was reproduced outside our CLI
with a raw `NamedPipeClientStream`, so it is the pipe object, not our client.

The user SID is the load-bearing detail: a UAC-split account's filtered (Medium)
and elevated (High) tokens **share** it, so a single ACE spans the elevation
boundary while still excluding every other local user. That is the windows
equivalent of unix `0600` — where all of a user's processes reach the socket
regardless of privilege — not a widening of it.

`GENERIC_ALL` is required rather than a tighter mask: the generic mapping folds
`FILE_CREATE_PIPE_INSTANCE` into `FILE_GENERIC_WRITE`, and the daemon's own
accept loop needs that right against the first instance's DACL to create instance
N+1. A hand-tuned mask breaks the second connection and buys nothing against a
same-user attacker who already owns the database and the binaries.

The fallback ladder exists because this is the daemon's first and most
load-bearing bind (D-0009) and the elevated path cannot be exercised in CI. The
code must be structurally incapable of turning a working windows startup into one
that will not start.

## Rejected

- **A loopback-HTTP fallback for the hook path.** The bearer-authenticated
  `127.0.0.1:8722` ingress exists, was listening throughout the incident, and the
  non-elevated user can read the bearer from the store — so it *would* have
  worked. Rejected anyway: it makes the capture path depend on a surface D-0009
  deliberately made optional and survivable, and widens the
  reachable-from-a-browser footprint, to work around a bug a correct ACL fixes
  outright.
- **Refusing to start when elevated.** Breaks a configuration this change makes
  correct, strands users whose only workable setup is an always-elevated
  terminal, and recreates D-0009's respawn-loop shape with every client
  reporting "down".
- **A `LW` (low) integrity label.** Crosses far more than the elevation boundary
  we need; hands an unauthenticated `Nuke`/`Shutdown` channel to AppContainer and
  sandboxed browser renderers. `ME` is the exact minimum.
- **An Everyone / Authenticated-Users ACE.** A real regression versus even the
  accidental status quo.
- **A daemon-side lost-hook counter.** The daemon cannot count hooks that never
  arrived — which is precisely why this went unnoticed. The breadcrumb has to be
  written client-side, by the shim that failed.

## Agent directives

- Never create a windows control-pipe instance without the explicit descriptor —
  not in `bind`, and **not in the accept loop**. A default-DACL instance #2
  reintroduces the bug for every connection after the first, and the failure is
  invisible in any test that opens only one connection.
- Never widen the pipe DACL beyond the creating token's user SID plus `SY`, and
  never label it below `ME`.
- Never make a descriptor failure fatal to the control-socket bind (D-0009), and
  never let it be silent — surface the rung on `DaemonInfo`.
- Never map `ERROR_ACCESS_DENIED` to "the daemon is not running", and never
  advise a bare `dira daemon start` in response to it: following that advice
  spawns a daemon that dies on `first_pipe_instance` after clobbering the live
  pidfile.
- A hook shim always exits 0 and writes nothing to stdout, but a *transport*
  failure must always leave a durable local breadcrumb. Semantic non-results
  (unknown harness, unaccounted event) stay silent.
- Keep the SDDL builder, the elevation advice, and the connect classifier as pure
  functions that compile and are tested on every platform. They are the only
  parts of this a maintainer without windows can verify.

## Manual runbook (required before flipping `verified`)

Needs two differently-elevated tokens on one machine; GitHub runners provide one.

1. Elevated PowerShell: run `dirad` in the foreground; the elevation warning must
   reach the log file.
2. Normal PowerShell: `dira daemon status` → `up` with a `note:`. Before this
   change, `access denied`.
3. Normal PowerShell: pipe a `SessionStart` payload into `dira hook claude`; exit
   0, and `dira status` shows the event with no hook-health warning.
4. **Negative control:** force the DACL-only rung and repeat step 2. It must
   *fail*, proving the integrity label is load-bearing. If it succeeds, the SACL
   is unnecessary — amend this record and drop it rather than keeping it on
   faith.
5. **Second local user account:** user B must not reach user A's daemon. This is
   what proves the gate was not widened.
6. `dira daemon install` from a normal shell, log off/on, confirm the
   task-registered daemon is non-elevated and reachable.
