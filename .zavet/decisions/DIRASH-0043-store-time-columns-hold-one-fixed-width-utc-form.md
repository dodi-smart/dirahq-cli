---
id: DIRASH-0043
title: Store time columns hold one fixed-width UTC form, bound only through StoredAt or RawAt
status: active
guards:
  - cli/core/src/store.rs
  - cli/core/src/stored_at.rs
checks:
  - no trimming Rfc3339 value is bound in the store :: sh -c '! grep -nE "bind\([^)]*Rfc3339" cli/core/src/store.rs'
  - text order is time order across fraction widths :: cargo test -p dira-core --lib store::tests::event_at_orders_by_time_across_fraction_widths
  - existing rows are rewritten once :: cargo test -p dira-core --lib store::tests::normalize_stored_at_rewrites_every_time_column_once
  - token and commit times are stored canonical :: cargo test -p dira-core --lib store::tests::token_and_commit_times_are_stored_canonical_and_read_back_rfc3339
origin: recorded
verified: true
---

## Decision

`events.at`, `token_usage.at` and `artifacts.authored_at` hold one form:
UTC with nine fractional digits and a literal `Z`
(`2026-10-16T12:00:05.120000000Z`). Every bind goes through `StoredAt` (an
instant) or `RawAt` (text from a transcript or git). A one-shot data migration
at `Store::open`, gated by meta `stored_at_canonical_v1`, rewrites older rows.

## Why

SQLite compares and orders these TEXT columns as strings. `time`'s `Rfc3339`
formatter trims trailing fractional zeros, so `…:05.12Z` sorted after
`…:05.123Z`, so events from the same second replayed out of order. Git's
`%aI` carries the author's offset, so commit dates did not sort either. With
one width and one offset, text order is time order and the indexes still serve
every range and `ORDER BY`.

## Rejected

- Compare through `julianday()`. It defeats the indexes, rounds to
  milliseconds, and turns an empty `at` into NULL.
- A SQL migration. It would be a second copy of the formatter, with its own
  offset folding and its own guard against SQLite's millisecond rounding.

## Agent directives

- Bind a time column or a bound compared against one only as `StoredAt` or `RawAt`.
- Never bind `x.format(&Rfc3339)` in `store.rs`.
- A new TEXT time column that is compared or ordered joins `TIME_COLUMNS`.
- Read a time column for the wire or the UI through `display_at`, so the wire
  keeps plain RFC 3339.
- Store non-RFC 3339 text from outside verbatim. Never drop the row (D-0018).

## Verification

The grep check fails on an inline `Rfc3339` bind in `store.rs`, not on a
string formatted earlier and bound later; review still covers those. The
tests cover ordering, the data migration and the token/commit writes.

## Open questions

- An older binary run after the migration writes trimmed rows that stay trimmed.
- `artifacts.authored_at` loses the author's offset; the wire carries the UTC
  instant. Whether the cloud used the offset is not established.
