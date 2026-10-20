---
id: DIRASH-0045
title: A lineage rule change re-sends the ended rollups once, sessions only
status: active
guards:
  - cli/core/src/orchestration.rs
  - cli/core/src/sync/batch.rs
  - cli/core/src/store.rs
  - cli/dirad/src/sync.rs
checks:
  - the rebuilt rollup equals the normal ended rollup :: cargo test -p dira-core --lib sync::batch::tests::a_lineage_resend_rollup_equals_the_normal_ended_rollup_over_the_same_history
  - a session whose last event is not an end is not re-sent :: cargo test -p dira-core --lib sync::batch::tests::a_session_whose_last_event_is_not_an_end_is_not_resent
  - re-send batches carry only sessions :: cargo test -p dira-core --lib sync::batch::tests::lineage_resend_batches_carry_only_sessions
  - the store lists only definitely-ended sessions :: cargo test -p dira-core --lib store::tests::ended_sessions_since_lists_only_sessions_whose_last_event_is_an_end
  - a re-send after a normal sync double counts nothing and moves no cursor :: cargo test -p dirad --lib sync::tests::a_lineage_resend_after_a_normal_sync_double_counts_nothing_and_moves_no_cursor
  - the marker gates the pass :: cargo test -p dirad --lib sync::tests::the_lineage_resend_runs_when_the_marker_is_missing_or_older_and_not_when_current
  - a failed pass writes no marker :: cargo test -p dirad --lib sync::tests::a_failed_lineage_resend_leaves_the_marker_unwritten_and_resumes
  - an unlinked device is a silent no-op :: cargo test -p dirad --lib sync::tests::the_lineage_resend_is_a_silent_no_op_while_unlinked_or_unconfigured
origin: recorded
verified: true
---

## Decision

`dira_core::orchestration::LINEAGE_RULES_VERSION` (an integer, 2 today) names the
generation of the lineage and branch rules. The `meta` key
`lineage_rules_version` records the generation whose corrected rollups were last
re-sent. Missing reads as 0.

When the build's version is newer than the marker, the sync task, after a healthy
regular flush and never on a foreground path, rebuilds the final `SessionRollup`
of every session that ended in the last 30 days from the local store and POSTs
them once. The marker is written only after every chunk was acked.

- Ended only. A session is re-sent only when its latest event is a `session_end`.
  Nothing is ever sent as a partial from here.
- Sessions only. Batches carry empty `intervals`, `token_usage` and `artifacts`
  and move no sync cursor.
- Same rollup. Rebuilt by `build_sessions` over the full history, so
  `started_at`, `ended_at`, `agent_wall_seconds` and `prompts` match the first
  send.
- Same transport. Signed envelope, the same POST and error classification as
  `flush` (`ingest_error`), `chunk_pace_delay` between chunks (D-0020) and
  `dira_core::sync::Backoff` on failure (DIRASH-0031). A failure backs off on
  its own clock and never sleeps the loop. An unlinked device or missing cloud
  URL is a silent no-op that writes no marker.

## Why

An ended rollup ships once, on the flush holding its `SessionEnd`. Sessions that
synced under older daemons keep stale lineage in the cloud: `role=orchestrator`
with no repo, or a wrong branch. DIRASH-0044 and `SessionBranch` just changed
those rules. A 1.7 cloud treats `role`, `repos` and `workspaceRef` as
authoritative (absent means retract) when the incoming `agentWallSeconds` is not
lower than the stored value, so a corrected rollup heals the row. Only the
device holds the events needed to recompute it.

## Rejected

- **Cloud-side recompute**: the cloud has no evidence (events, cwds, spawn
  records) to derive lineage from.
- **Manual-only `dira sync --resend-sessions`**: the fleet stays stale until
  each user finds and runs it, and the next rule change repeats the problem.
- **Re-sending partials for ended sessions**: the cloud reopens a row when a
  partial arrives (cloud#253), so ended sessions would be shown as live again.
- **Re-sending intervals**: they are the billable truth. Anything that ships
  them twice risks counting time twice, and the rollup fixes need none.

## Agent directives

- Bump `LINEAGE_RULES_VERSION` whenever a change to `SessionLineage`,
  `SessionBranch` or the role/repo/workspace derivation can change a rollup for
  events already stored.
- Never send a partial, an interval, a token row or an artifact from the
  re-send, and never advance a cursor in it.
- Write the marker last, only after every chunk was acked. Do not write it on a
  skip.
- Keep the re-send off foreground CLI paths and out of the regular flush's
  failure handling.
- Keep the Store reads on the probe-filtering path (DIRASH-0023) and bind times
  through `StoredAt` (DIRASH-0043).

## Verification

The `checks:` entries cover the rebuilt rollup against the normal path, the
ended-only rule at both the store and the builder, the sessions-only batches,
the no-double-count and unmoved-cursor regression against `MockCloud`, and the
marker (runs when missing or older, silent when current, unwritten on failure or
when unlinked).

Not verified against the live cloud yet.

## Open questions

- A session older than retention may rebuild with less history than its first
  send. Its lower `agent_wall_seconds` is refused by the cloud's guard, which is
  the safe outcome.
- A session with no agent time is skipped: it has no lineage to correct.
