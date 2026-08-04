---
id: D-0018
title: Token usage rides its own rowid cursor, never the event `at` window
status: active
guards:
  - cli/dirad/src/sync.rs
  - cli/core/src/store.rs
checks:
  - a token backlog ships with no new events :: cargo test -p dirad --lib flush_ships_a_token_backlog_with_no_new_events
  - re-captured rows still ship after a nuke :: cargo test -p dirad --lib re_captured_rows_still_ship_after_a_nuke
corrected-by: D-0020
origin: session
verified: false
---

## Decision

`token_usage` rows ship on `META_TOKEN_CURSOR`, a `token_usage.rowid`
watermark advanced on the final chunk's 2xx — the same discipline
`META_ARTIFACTS_CURSOR` follows. They are never selected by the `at`-span of
the flush window's events, and they participate in the flush gate, so a
caught-up event log still drains a compute backlog.

`nuke` and a full `dira device resync` both blank the cursor.

## Why

Token rows were the only synced stream with no durable watermark. They were
read as `WHERE at > <first event of this batch> AND at <= <last event>`, gated
behind `!events.is_empty()`. That loses rows structurally, not occasionally:

- `capture_tokens` runs on `Stop`/`SessionEnd` and stamps each turn with the
  **transcript's** timestamp, so a turn is always dated *before* the event that
  discovered it — below the window's own exclusive lower bound.
- The lower bound is the first event of the new window, not the previous
  window's upper bound, so `(last_n, first_{n+1}]` belonged to no window at all.
- With no cursor, nothing recorded that a row had never been sent, so no later
  flush reconsidered it. A miss was permanent.

Measured on the reporting machine: 3,471 of 3,656 rows — $3,063 of $3,148, or
**97.3%** — were unshippable. The dashboard read `$2` for a week that cost ~$978.

A rowid is the right key precisely **because** `at` is not monotonic with
respect to capture order. Re-captures and back-dated transcript timestamps both
break any `at` watermark, and `nuke` guarantees a full re-import at original
historical timestamps by clearing the `token_offset:%` capture watermarks.

Selecting on rowid also removes two hazards that had nothing to do with the
window: `at` is `TEXT`, compared lexicographically between two independent
clocks whose subsecond formatting differs, and a transcript line without a
`timestamp` yields `at = ""`, which can never satisfy `at > ?1`.

The cloud dedups on `TokenUsage.id`, so over-inclusion is free. Under-inclusion
is the one direction that id cannot protect against — which is why the fix is a
cursor and not a wider window.

## Rejected

- **Widen the `at` window** (inclusive bound, previous window's upper bound as
  the lower one) — leaves the no-cursor and no-catch-up failure modes intact,
  and keeps both the TEXT-comparison and empty-`at` hazards.
- **Initialise the cursor to the current max rowid on upgrade** — correct-looking
  and wrong: it strands exactly the backlog the change exists to recover. An
  absent key reads as "from the beginning", which is the behaviour we want.
- **A separate token flush loop** — a second scheduler, a second backoff and a
  second failure surface, to solve a problem the existing chunking already
  handles.

## Agent directives

- Never select token rows for sync by `at`. `at` is capture-order-independent
  and is display/reporting data only.
- Any new rowid cursor must be blanked in `Store::nuke`. Emptying a table lets
  SQLite re-issue rowid 1, and a surviving watermark then skips every new row —
  silently, and forever.
- A new synced stream needs a durable cursor, a place in the flush gate, and a
  count in the flush log line. Missing the last one is why this defect survived
  weeks in production: a flush shipping zero token rows logged byte-identically
  to one shipping a thousand.
- ~~Never advance the token cursor anywhere but the `is_last` chunk's 2xx.~~
  **Struck — corrected by D-0020.** Advance a stream's watermark on the chunk
  that carried the rows, in that chunk's own 2xx branch, to the high rowid *it*
  carried. Gating on `is_last` made progress depend on an unbounded run of
  consecutive successes, so one 429 or one restart mid-drain cost the whole
  backlog. See D-0020 for the replacement directive; the rest of this record
  stands.

## Verification

Three regression tests in `cli/dirad/src/sync.rs`, all driving the real `flush`
against `MockCloud`: a back-dated token row with **no new events** ships and
advances the cursor (this one was confirmed to fail when token selection is
stubbed out, so it is not vacuous); a 413 leaves the cursor put; and a row
re-captured after `nuke` still reaches a batch.

Not yet verified in the field — the stranded backlog on the reporting machine
should drain on first flush after upgrade, and that has not been observed.
