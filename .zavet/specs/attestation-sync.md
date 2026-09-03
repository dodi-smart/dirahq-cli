---
title: Attestation sync and session rollups
version: 5
origin: session
verified: false
confidence: high
date: 2026-08-09
paths:
  - cli/core/src/sync/batch.rs
  - cli/core/src/store.rs
  - cli/dirad/src/sync.rs
  - cli/dirad/src/state.rs
  - cli/dirad/src/writer.rs
  - cli/core/src/tokens.rs
decisions: [D-0001, D-0006, D-0018, D-0020, DIRASH-0025]
---

## Overview

How the daemon turns locally-stored events into signed attestation batches:
the cursor-windowed flush, deterministic chunking, and the two kinds of
`SessionRollup`, terminal (ended) and partial (still-live), including how
rollups keep their `prompts`/`branch`/wall-clock when a session outlives a
single flush window (issue #40).

## Behavior

- A flush reads events in `(META_SYNC_CURSOR, until]`, splits the window into
  chunks only at > idle breaks (never inside a billable interval), POSTs each
  chunk, and advances the cursor per 2xx ack. The final chunk carries the
  partial rollups; its ack marks partials sent. Row cursors do NOT ride that
  flag. Each chunk advances the artifact and token watermarks it carried, on
  its own 2xx (D-0020).
- Token rows ride `META_TOKEN_CURSOR`, a `token_usage.rowid` watermark, on
  exactly the artifact cursor's discipline. They are NOT selected by the
  `at`-span of the window's events, and they participate in the flush gate, so
  a caught-up event log still drains a compute backlog. Both properties are
  load-bearing (D-0018): a turn is discovered by the `Stop` that *follows* it,
  so its transcript timestamp always sorts below the window's own first event,
  and the old `at`-window selection therefore skipped ~97% of captured compute
  permanently. Nothing recorded that a row had never been sent, so no later
  flush reconsidered it. `nuke` and a full `dira device resync` both blank the
  cursor; for `nuke` that is required, not tidy, because emptying the table
  lets SQLite re-issue rowid 1 beneath a surviving watermark.
- Upstream of that cursor, `capture_tokens` reads each transcript from a
  per-file byte watermark (`token_offset:<session>[:<sidecar stem>]`) paired
  with a prologue fingerprint (`token_fp:`, FNV-1a over the first ≤4 KiB).
  The offset says WHERE to resume; the fingerprint says WHICH file it refers
  to, because a length comparison alone cannot see a transcript replaced by a
  different file of equal-or-greater length. An ABSENT fingerprint means an
  install upgrading into the check and never triggers a re-read. `nuke` clears
  both key families.
- The tail is read as BYTES and decoded lossily, and the new watermark is the
  last newline in those raw bytes. `read_to_string` was all-or-nothing on
  UTF-8, so one invalid byte aborted the capture before the watermark advanced
  and that session re-read the same bad tail forever; and once a lossy decode
  is in play, an index into the decoded string is no longer a file offset
  (U+FFFD is 3 bytes per 1–3 replaced). Watermark read/write failures log at
  `warn`. A watermark that silently stops advancing is what made a 97%
  compute loss invisible for weeks.
- Artifacts and token rows are spread across those chunks, at most
  `CHUNK_ARTIFACTS` / `CHUNK_TOKENS` each, with extra artifact-/token-only
  chunks appended when the backlog outlasts the event chunks. Spreading
  rather than capping the store read is what keeps those cursors honest.
  Each chunk advances its stream's watermark only to the high rowid IT
  carried, so the cursor can never move past a row that was not sent, and a
  drain that dies part-way keeps everything the cloud accepted. Gating those
  cursors on the final chunk instead made progress depend on an unbounded
  run of consecutive successes: 48,601 token rows is 49 chunks against the
  cloud's 30/min ingest budget, so the drain was throttled at chunk 31,
  recorded nothing, and restarted at row 1 forever (issue #88, D-0020). A
  long drain now paces its POSTs to stay inside that budget. Artifacts are
  the fat rows (`touched_paths`, `blobs`); unbounded, a `dira device resync`
  built one body out of the whole backlog and was rejected `413` (issue
  #71). The ceiling is the platform's request-body limit, not a cloud
  policy, so construction is the only place it can be respected.
- A session whose entire life is lifecycle events is not a session. The live
  registry marks `had_signal` on the first human signal OR agent activity.
  That is deliberately the union, so an agent-only run is real on its first
  tool call. `SessionRegistry::active` reports only sessions that are
  `!ended`, have shown a signal, and are not stale. Staleness matters
  because `ended` is not a latch and a crashed harness never sends
  `SessionEnd`: without the bound such a session is broadcast as live
  indefinitely. Nothing is evicted, so a session that resumes is reported
  again with `started_at` and its counters intact (issue #74).
- Terminal rollups are emitted by the chunk whose slice contains the
  session's `SessionEnd`/`ManualStop`, but are aggregated over the session's
  FULL retained event history (`Store::events_for_sessions`, bounded by the
  window's `until` id), not just the tail window. Prompts, branch,
  `started_at`, and clamped `agent_wall_seconds` cover the whole
  session. Best-effort: compaction prunes events that are both synced and
  past retention, so a session outliving retention rolls up from what
  remains (still a superset of the tail window).
- "Ended" is decided from the window slice ONLY, never from history: a live
  session's history can carry stale `SessionEnd`s (Claude Code emits one on
  compaction), and a history-derived end would fabricate a false terminal
  rollup. When both a stale and a real end exist, `(at, id)`-sorted merging
  makes the latest end win.
- Partial rollups describe still-live sessions from the daemon's in-memory
  registry: rolling clamped `active_seconds`, a live `prompts` counter,
  and the last-resolved branch (last-wins is the honest answer for a live
  session, deliberately unlike the terminal rollup's start-branch policy,
  whose write supersedes the partial anyway). The registry is a pure
  reduction over events, so daemon-restart hydration replays it back for
  free.

## Interfaces & data

- `dira_core::sync::build_chunked_batches(events, tokens, artifacts,
  partials, device_id, idle, now, seed, history)`. `history` feeds ONLY
  `build_sessions`; intervals and `batch_id_for_chunk` remain pure functions
  of the window, so batch ids are byte-identical with or without history.
- `Store::events_for_sessions(session_ids, until)`. Per-id queries ride
  `idx_events_session_at`; no cross-session ordering guarantee.
- History merging dedups by event id (ULID, the events PK) against the chunk
  slice, which also includes same-window events sitting in other chunks.
- The flush's summary log reads the cloud's ack through
  `dira_core::sync::parse_ingest_response`, the same parser the epoch/watermark
  handshake uses. It is response-only and unsigned, so its fields never touch
  the signing vector. Its `accepted`/`duplicates` are `Option`, and an absent
  count renders `-`. The contract's `IngestAck` is NOT used here: every field
  is `#[serde(default)]`, so it cannot tell "absent" from "zero" and turned
  the live cloud's counter-free 202 into `accepted=0 duplicates=0` on every
  healthy flush (issue #72).
- Every `parse_*_response` is tolerant but never silent. An empty body is
  `Ok(default)` (back-compat with a cloud that acks with no payload); a
  non-empty body that won't parse returns its `serde_json::Error`. Callers on a
  2xx report it via `dira_core::sync::warn_unreadable_body` and continue on
  defaults; callers parsing an *error* body fall back quietly, since a proxy's
  HTML 502 is not contract drift. Before #104 all of these were
  `unwrap_or_default()`, so an unreadable ack dropped `dataEpoch`. A cloud
  that had reset its durable log was indistinguishable from one that never
  mentioned an epoch, and the re-send never fired.

## Invariants

- A token turn's repo comes from that turn's own `cwd`, not from the event that
  triggered the capture pass (DIRASH-0025). Attribution is per turn because one
  unresolved `Stop` would otherwise mark every turn since the last watermark
  repo-less, and repo-less compute is invisible rather than merely unlabelled.
- A row is written with no project only when the turn's cwd, the triggering
  event, and the session's sticky project are all unavailable. Every such
  row is counted and warned, never silently dropped.
- `TokenTurn.cwd` is capture-time provenance only. It never reaches the
  `token_usage` table, the contract, or the wire.
- Nothing content-bearing crosses the wire. Rollups are metadata only
  (D-0001); the flush path performs no foreground network I/O outside the
  sync task (D-0006).
- Cloud merge semantics the rollups rely on: partial UPSERTs are
  latest-wins per `session_id`; the terminal rollup is the authoritative
  last write. Full-history aggregation is what keeps the terminal write
  monotonic (≥ the last partial's rolling counters).
- A retried chunk after compaction may carry slightly different rollup
  content under the same `batch_id`; the cloud's batch-id dedup keeps the
  first accepted version. Every version is a superset of the old
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
