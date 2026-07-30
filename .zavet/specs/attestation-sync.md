---
title: Attestation sync and session rollups
version: 2
origin: session
verified: false
confidence: high
date: 2026-07-31
paths:
  - cli/core/src/sync/batch.rs
  - cli/core/src/store.rs
  - cli/dirad/src/sync.rs
  - cli/dirad/src/state.rs
decisions: [D-0001, D-0006]
---

## Overview

How the daemon turns locally-stored events into signed attestation batches:
the cursor-windowed flush, deterministic chunking, and the two kinds of
`SessionRollup` — terminal (ended) and partial (still-live) — including how
rollups keep their `prompts`/`branch`/wall-clock when a session outlives a
single flush window (issue #40).

## Behavior

- A flush reads events in `(META_SYNC_CURSOR, until]`, splits the window into
  chunks only at > idle breaks (never inside a billable interval), POSTs each
  chunk, and advances the cursor per 2xx ack. The final chunk carries the
  token rows and partial rollups; its ack also marks partials sent and
  advances the artifact cursor.
- Artifacts are spread across those chunks, at most `CHUNK_ARTIFACTS` each,
  with extra artifact-only chunks appended when the backlog outlasts the
  event chunks. Spreading rather than capping the store read is what keeps
  the artifact cursor honest — it still advances exactly once, on the true
  final chunk, to the same snapshot bound the caller read, so the cursor can
  never move past a row that was not sent. Artifacts are the fat rows
  (`touched_paths`, `blobs`); unbounded, a `dira device resync` built one
  body out of the whole backlog and was rejected `413` (issue #71). The
  ceiling is the platform's request-body limit, not a cloud policy, so
  construction is the only place it can be respected.
- A session whose entire life is lifecycle events is not a session. The live
  registry marks `had_signal` on the first human signal OR agent activity —
  deliberately the union, so an agent-only run is real on its first tool
  call — and `SessionRegistry::active` reports only sessions that are
  `!ended`, have shown a signal, and are not stale. Staleness matters
  because `ended` is not a latch and a crashed harness never sends
  `SessionEnd`: without the bound such a session is broadcast as live
  indefinitely. Nothing is evicted, so a session that resumes is reported
  again with `started_at` and its counters intact (issue #74).
- Terminal rollups are emitted by the chunk whose slice contains the
  session's `SessionEnd`/`ManualStop`, but are aggregated over the session's
  FULL retained event history (`Store::events_for_sessions`, bounded by the
  window's `until` id), not just the tail window — prompts, branch,
  `started_at`, and idle-trimmed `agent_wall_seconds` cover the whole
  session. Best-effort: compaction prunes events that are both synced and
  past retention, so a session outliving retention rolls up from what
  remains (still a superset of the tail window).
- "Ended" is decided from the window slice ONLY, never from history: a live
  session's history can carry stale `SessionEnd`s (Claude Code emits one on
  compaction), and a history-derived end would fabricate a false terminal
  rollup. When both a stale and a real end exist, `(at, id)`-sorted merging
  makes the latest end win.
- Partial rollups describe still-live sessions from the daemon's in-memory
  registry: rolling idle-trimmed `active_seconds`, a live `prompts` counter,
  and the last-resolved branch (last-wins — the honest answer for a live
  session, deliberately unlike the terminal rollup's start-branch policy,
  whose write supersedes the partial anyway). The registry is a pure fold
  over events, so daemon-restart hydration replays it back for free.

## Interfaces & data

- `dira_core::sync::build_chunked_batches(events, tokens, artifacts,
  partials, device_id, idle, now, seed, history)` — `history` feeds ONLY
  `build_sessions`; intervals and `batch_id_for_chunk` remain pure functions
  of the window, so batch ids are byte-identical with or without history.
- `Store::events_for_sessions(session_ids, until)` — per-id queries riding
  `idx_events_session_at`; no cross-session ordering guarantee.
- History merging dedups by event id (ULID, the events PK) against the chunk
  slice, which also folds in same-window events sitting in other chunks.
- The flush's summary log reads the cloud's ack through
  `dira_core::sync::parse_ingest_response`, the same parser the epoch/watermark
  handshake uses — response-only and unsigned, so its fields never touch the
  signing vector. Its `accepted`/`duplicates` are `Option`, and an absent
  count renders `-`. The contract's `IngestAck` is NOT used here: every field
  is `#[serde(default)]`, so it cannot tell "absent" from "zero" and turned
  the live cloud's counter-free 202 into `accepted=0 duplicates=0` on every
  healthy flush (issue #72).

## Invariants

- Nothing content-bearing crosses the wire — rollups are metadata only
  (D-0001); the flush path performs no foreground network I/O outside the
  sync task (D-0006).
- Cloud merge semantics the rollups rely on: partial UPSERTs are
  latest-wins per `session_id`; the terminal rollup is the authoritative
  last write. Full-history aggregation is what keeps the terminal write
  monotonic (≥ the last partial's rolling counters).
- A retried chunk after compaction may carry slightly different rollup
  content under the same `batch_id`; the cloud's batch-id dedup keeps the
  first accepted version — every version is a superset of the old
  tail-only rollup.
- Exactly one chunk per flush carries `is_last`, and it is the final one.
  Both the artifact cursor and the partial-sent watermark hang off it, so a
  second `is_last` would advance them mid-drain.
- No batch may exceed the request-body ceiling by construction. A `413` is
  therefore evidence of one pathological record, not of backlog size, and
  gets its own `SyncError` + `payload_too_large` health kind so it is never
  mistaken for a retryable failure.

## Open Questions

- Sessions that outlive retention lose pre-retention prompts from the
  terminal rollup; `session_rollup_daily` retains the counts locally and
  could backfill them if this ever matters in practice.
