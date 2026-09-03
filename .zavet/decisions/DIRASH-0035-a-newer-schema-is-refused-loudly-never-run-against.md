---
id: DIRASH-0035
title: A newer schema is refused loudly, never run against
status: active
guards:
  - cli/core/src/store.rs
checks:
  - the refusal names the cause and the recovery :: cargo test -p dira-core --lib opening_a_newer_schema
origin: session
verified: true
---

## Decision

`Store::open` keeps sqlx's **strict** migration validation: a database whose
`_sqlx_migrations` records a version this binary does not know refuses to
open. What changes is the error — sqlx's bare `VersionMissing(<number>)` is
replaced by `Error::SchemaNewer`, which names the database path, the unknown
migration, the cause ("last written by a newer dira/dirad"), and both
recovery paths (`dira update`, or `dira update --version <the writer's
version>`). `ignore_missing` is never set.

## Why

The downgrade scenario is real: `dira update --version <older>` is a
documented recovery flow, and a daemon that then meets a newer `dira.db`
respawn-loops under launchd with only sqlx's cryptic number in the log.
The tempting fix — `Migrator::set_ignore_missing(true)` — would make the
older binary run anyway, and that is the wrong trade. A newer migration may
add required columns, reshape a table, or change semantics the older
binary's SQL silently violates; on the event log that feeds billing
(engaged-time accounting) and identity (the keychain-fallback secret), a
clean, diagnosable startup refusal is strictly better than undefined
runtime behavior with green process state. The same doctrine already runs
through the codebase: `dira doctor` reports and never repairs
(DIRASH-0022), and a replacement daemon is never started on optimism
(D-0019).

The respawn loop itself is not the bug — launchd restarting a daemon that
refuses to start is supervision working as configured. The bug was that
each attempt left no line a human could act on. The friendly refusal fixes
the actionable half without touching the correctness half.

## Rejected

- **`ignore_missing`** — trades the one failure mode that is safe (refusing
  to start) for the one that is not (running against a schema written by a
  future version).
- **A version-number gate in `meta`** — duplicates what `_sqlx_migrations`
  already records; a second source of truth for schema state would need its
  own repair story.
