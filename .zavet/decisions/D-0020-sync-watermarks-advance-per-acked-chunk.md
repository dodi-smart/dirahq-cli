---
id: D-0020
title: Sync watermarks advance per acked chunk, and a long drain paces itself
status: active
guards:
  - cli/dirad/src/sync.rs
  - cli/core/src/sync/batch.rs
checks:
  - a throttled drain keeps acked progress :: cargo test -p dirad --lib a_throttled_token_drain_keeps_the_chunks_the_cloud_already_acked
  - a resumed drain ships only the remainder :: cargo test -p dirad --lib a_resumed_token_drain_ships_only_the_rows_left
  - a long drain is paced under the budget :: cargo test -p dirad --lib pacing
corrects: D-0018
origin: recorded
verified: true
---

## Decision

`META_TOKEN_CURSOR` and `META_ARTIFACTS_CURSOR` advance on **each chunk's own
2xx**, to the high rowid that chunk carried, not on the final chunk's ack.
A flush that dies part-way keeps everything the cloud already accepted.

Within one flush, consecutive ingest POSTs are spaced once a drain is long
enough to threaten the cloud's per-device budget.

This corrects D-0018's "advanced on the final chunk's 2xx" and its directive
"never advance the token cursor anywhere but the `is_last` chunk's 2xx".
Everything else in D-0018 stands: rowid not `at`, blank on `nuke`, a place in
the flush gate, a count in the log line.

## Why

D-0018 gave tokens their own cursor and their own 1000-row chunking, which made
a drain's chunk count independent of the event backlog. The cloud's ingest
budget (30/min, fixed window) had been sized on the opposite assumption. Its
own comment says "a drain must NEVER be throttled mid-flight, so the budget
covers a full large drain". Nothing re-derived that sizing when the coupling
was cut.

Issue #88 is where the two met: 48,601 rows ⇒ 49 chunks against a 30-request
budget. Chunks 1–30 were accepted, the 31st got a 429, and because the
watermark was all-or-nothing the flush recorded **nothing** and restarted at row
1 on the next attempt. Observed in the field: 47 consecutive 429s on a ~62s
cycle with the cursor blank throughout, and the wasted retries burned the same
budget ordinary event sync needed.

An all-or-nothing watermark makes progress depend on an unbounded number of
*consecutive* successes. That is not a rate-limit bug. A network blip or a
daemon restart mid-drain costs the whole backlog just as completely. Per-chunk
advance makes progress monotonic, so a drain converges over as many windows as
it needs.

Pacing is the second half, and only the second half: it stops the 429s
happening, while per-chunk advance is what makes them survivable. Pacing alone
would leave the restart hazard intact, which is why both ship together.

## Rejected

- **Raise the cloud's ingest budget**: moves the cliff to the next backlog size
  and leaves every non-429 interruption just as expensive.
- **Pace only, keep the `is_last` watermark**: a drain still loses everything to
  one blip, and the pacing constant becomes load-bearing for correctness rather
  than for politeness.
- **A dedicated token flush loop**: rejected in D-0018 for the same reasons, and
  per-chunk advance removes the motivation for it.

## Agent directives

- Advance a stream's watermark on the chunk that carried the rows, in that
  chunk's own 2xx branch. Never gate a watermark on `is_last`.
- A chunk's watermark is the high rowid **it** carried, never the flush-wide
  snapshot bound. Writing the snapshot bound from a non-final chunk claims rows
  that never left the machine.
- Partial rollups stay on `is_last`: they are a live-registry snapshot, not a
  cursor over stored rows.
- When adding a synced stream, size its chunking against the cloud's per-device
  ingest budget, and treat exceeding it as expected, not exceptional.

## Verification

The three `checks:` above drive the real `flush` against `MockCloud`: a 429 on
the second of two token chunks must leave the cursor on the first chunk's high
rowid (fails on the old `is_last` code, which left it blank); the follow-up
flush must send one chunk, not the whole backlog; and the pacing policy tests
pin that a single-chunk flush is never delayed while a long drain is.

Not verified in the field yet. The stranded backlog on the reporting machine
is the test case, and it has not been re-run against this change.

## Open questions

- The 30/min budget is read from the cloud repo's `rate-limit.ts` comment, not
  from the deployed config; the observed 429 timing matches it but the deployed
  value was not confirmed. The pacing constant carries headroom for that.
