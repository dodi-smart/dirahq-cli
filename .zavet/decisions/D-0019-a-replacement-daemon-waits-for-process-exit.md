---
id: D-0019
title: A replacement daemon is never started until the previous process is confirmed exited
status: active
guards:
  - cli/dira/src/daemon.rs
  - cli/dirad/src/lib.rs
checks:
  - restart refuses while the old pid lives :: cargo test -p dira --bin dira restart_refuses_to_spawn_a_replacement_while_the_old_pid_lives
  - schtasks never launches beside a live daemon :: cargo test -p dira --bin dira scheduled_task_restart_never_launches_beside_a_live_daemon
  - escalation keys on the process :: cargo test -p dira --bin dira escalation_keys_on_the_process_not_the_control_channel
origin: session
verified: false
---

## Decision

Every windows restart path — bare (`restart_bare`) and scheduled-task
(`restart_scheduled_task`) alike — sequences as:

```
send Shutdown → wait for PROCESS EXIT → if alive: taskkill /F
              → wait for exit again → only then start the replacement
```

Exit is decided by the process probe (`tasklist`), never by whether the control
channel answers. Failure to confirm exit is a hard error naming the surviving
pid and the manual command; it never falls through to starting another daemon.

`run()` additionally binds the control channel **before** `Store::open`, so a
duplicate that gets started anyway is refused before it can touch the database.

## Why

The single-instance guard is real and works — a duplicate is refused in 0.13 s
while the pipe is held. The restart path started the replacement in the one
window the guard cannot cover.

The field report blamed the old daemon dropping its listener early. The opposite
is true, and the difference matters: `serve_control` spawns a detached accept
loop that nothing cancels, and `handle_conn` fires the shutdown notify only
*after* writing its response. The pipe therefore answers `Ping` for the whole of
teardown. Meanwhile worst-case teardown is ~8 s (3 s offline beat + 5 s WAL
checkpoint) against a 5 s grace window — so `still_up_after` returned `true` and
`taskkill /F` fired into a daemon that was shutting down exactly as designed,
mid `wal_checkpoint_truncate`, after which the replacement was spawned into a
pipe whose handles had not been released. Outcome: frequently *zero* daemons and
a truncated checkpoint, not two.

The reported *two* daemons come from `restart_scheduled_task`, which was worse
and not a race: it sent `Shutdown`, waited, **discarded the result**, and ran
`schtasks /Run` unconditionally with no force-kill on that path at all. It is
the branch every user who ran `dira daemon install` takes, and
`detect_supervision` returns `ScheduledTask` on mere *registration*, returning
before the pidfile probe — so it structurally never learned the pid and could
not have force-killed anything.

Underneath both: "the control channel stopped answering" and "the process
exited" are different questions, and only the second one licenses starting a
replacement. Nothing in the tree asked the second one — a repo-wide search for
`OpenProcess`/`WaitForSingleObject` finds only unrelated elevation checks.

Unix is not exposed to the same failure because `bind_control_socket` holds a
kernel `flock` that is released on any exit, SIGKILL included (D-0009). Windows
has no counterpart: `SocketLock` is empty there and the guard *is* the bind, so
the guard is only as good as the ordering that precedes it.

## Rejected

- **Cancel the accept loop at the start of teardown** so the channel stops
  answering and the existing probe becomes accurate. Tempting and smaller, but
  it makes a shutting-down daemon indistinguishable from a crashed one for every
  *other* client too, and still answers the wrong question — a process can
  outlive its listener for reasons other than an orderly shutdown.
- **Just raise the grace window.** Necessary (5 s → 15 s) but not sufficient: it
  makes the force-kill rarer without ever establishing that the process is gone,
  and an unbounded teardown still races.
- **Keep `answers()` and add a sleep before `start`.** A guess dressed as a fix.

## Agent directives

- Never treat `is_up`/`answers` as evidence a process exited. They report the
  control channel, which outlives the decision to shut down.
- Never start, relaunch or `schtasks /Run` a daemon on a path where the previous
  pid was not confirmed gone. If the pid cannot be determined and something is
  still answering, refuse with manual instructions.
- Keep the stop grace budget above `dirad`'s worst-case teardown. If a teardown
  step gains a timeout, raise the budget in the same change.
- Route restart decisions through the tri-state `reach`, not the boolean
  `answers` — collapsing `ERROR_ACCESS_DENIED` to "down" is the mapping D-0016
  forbids, and these are the functions that then kill and respawn.
- Do not move `Store::open` back above `bind_control_socket`.

## Verification

Five `FakeRunner` unit tests in `cli/dira/src/daemon.rs`, none needing a live
daemon: escalation fires on a still-listed process even though nothing answers;
a kill that works reports stopped; a kill that does not report stopped;
`restart_bare` errors instead of spawning; and `restart_scheduled_task` never
reaches `schtasks /Run` while the pid is listed. The refusal test was confirmed
to fail when the guard is stubbed out.

`restart_bare`/`restart_scheduled_task` gained injectable grace budgets purely
so these run in milliseconds — the same seam, for the same reason, as
`graceful_then_force`'s existing parameters.

Since 2026-08-14 that is **one** function taking `os`, not a per-platform pair:
the sequence above is the invariant, only the ask (`Request::Shutdown` vs
SIGTERM) and the force (`taskkill /F` vs SIGKILL) differ, and two hand-synced
copies of an invariant are not a guarantee. Unix now runs it too — see the
amendment in DIRASH-0029 for why the flock made that optional but not
unnecessary.

**Not verified on a real Windows host.** The teardown-overlap behaviour, the
`tasklist` output shape, and the bind-before-store reorder under a contended
database all need a manual pass before this record flips to `verified: true`.
Local cross-compilation could not stand in: `ring` needs MSVC headers this
machine does not have. No new `cfg(windows)` code was added — every branch
routes through the existing `Os`/`Runner` seam, so all of it is compiled and
exercised on macOS.
