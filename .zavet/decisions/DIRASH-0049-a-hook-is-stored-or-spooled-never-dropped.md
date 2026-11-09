---
id: DIRASH-0049
title: A hook event is stored or spooled, never dropped, and the spool holds normalized metadata only
status: active
guards:
  - cli/sources/src/spool.rs
  - cli/dirad/src/spool.rs
  - cli/dirad/src/quarantine.rs
  - cli/dira/src/autostart.rs
checks:
  - spool records round-trip in order, carry no raw payload, and trim oldest-first :: cargo test -p dira-sources spool
  - spooled hooks replay with their original time and an old shim still lands :: cargo test -p dirad --test spool
  - a chunk is stepped past only after five identical content rejections :: cargo test -p dirad --lib quarantine
origin: session
verified: true
---

## Decision

Every hook either reaches the store or waits in the **spool**: one JSON file per event under
`spool/` beside the database.

- **Shim.** When `dira hook` cannot deliver (the daemon is down, busy or slow), it writes the
  event there and exits 0 as before. If nothing is listening, it asks for the daemon to be
  started through its supervisor: `schtasks /Run`, `launchctl kickstart` or
  `systemctl --user start`, else a bare windowless spawn. At most once per 30 s, never when
  elevated, never in a debug build.
- **Daemon.** It spills to the same spool when its ingest queue is full, where it used to
  answer `Ok` and discard the event. It replays the spool at startup, the moment it spills, every 2 s while
  records remain and on a 5-minute backstop, only while the queue has room, keeping each event's original `observed_at`. The shim now sends
  `observed_at` with every hook.
- **Content.** A spool record holds only the normalized event (`dira_sources::Normalized`),
  never the raw harness payload.
- **Sync.** A chunk the cloud rejects for content (413 or a non-auth 4xx) five times in a row
  is quarantined. The cursors step past it, and its bounds and reason go to the
  `sync_quarantine` meta list that `dira doctor` reports.

Alongside this:
- event ids come from one monotonic ULID generator;
- the writer reads the branch from `HEAD` instead of spawning `git`;
- the Claude subagent sweep reads the current `<session>/subagents/` layout and only recent
  legacy siblings;
- the control channel accepts before the background setup runs.

## Why

Windows users reported a large backlog of events that were never handled. Every path that lost
an event did so silently:
- a hook that missed the daemon left only a breadcrumb;
- a full queue answered `Ok`;
- hooks during daemon startup waited on a single pipe instance and timed out;
- one rejected chunk retried forever and stopped the event cursor.

The queue filled because the writer spawned `git` on every event (50–200 ms each on Windows)
and re-read every legacy subagent transcript for each new session. Non-monotonic ids could also
put an event behind the sync cursor for good.

Per-event files rather than one append log: hook processes are many, concurrent and short-lived.
A tmp-file-plus-rename needs no lock, and a reader never sees a torn record.

## Rejected

- **Send hooks over HTTP when the pipe fails.** D-0016 rejected an HTTP fallback for hooks, and
  it does not help when the daemon is down, which is the main case.
- **Spool the raw payload.** It would put prompt text and tool input on disk. Normalizing in
  the shim costs microseconds.
- **Block the shim until the daemon is back.** A hook must never stall the agent loop.
- **Bisect a rejected chunk down to the bad row.** It needs a restructured flush. Stepping past
  the whole chunk after five identical rejections bounds the loss to one chunk and is
  recorded. A finer bisect can come later.

## Agent directives

- Never answer an ingest with `Ok` without the event being queued or spooled. Every ingress goes
  through `dirad::spool::enqueue`.
- Never write a raw harness payload to the spool or anywhere else on disk.
- Never autostart from an elevated token or a debug build, and always prefer the supervisor over
  a bare spawn (D-0009: a bare daemon beside a supervisor respawn-loops it).
- Only content rejections (413, non-auth 4xx) count toward quarantine. Auth, schema skew, 429,
  5xx and network errors never do.

## Deviations from the 2026-11-09 plan (Phase 2)

- ~~The local pipe still carries the raw payload.~~ Resolved: the shim normalizes once and sends
  `Request::IngestRecord`, the same record the spool holds. Raw content no longer crosses the
  pipe either, and an older daemon's `bad request: unknown variant` makes the shim spool the
  record.
- The event sync cursor stays on `id`. The monotonic generator closes the skip for every id the
  writer mints, which is every hook. A rowid cursor is planned with contract 2.0 (Phase 5).
- Writer insert batching and a separate token-sweep task are not done. The two spawns that made
  the writer slow are gone instead.
