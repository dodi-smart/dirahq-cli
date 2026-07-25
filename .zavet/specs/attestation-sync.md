---
title: Attestation sync and session rollups
version: 1
origin: session
verified: false
confidence: high
date: 2026-07-25
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
  chunk, and advances the cursor per 2xx ack. The final chunk carries
  artifacts and partial rollups; its ack also marks partials sent.
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

## Open Questions

- Sessions that outlive retention lose pre-retention prompts from the
  terminal rollup; `session_rollup_daily` retains the counts locally and
  could backfill them if this ever matters in practice.
