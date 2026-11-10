---
id: DIRASH-0062
title: Meetings are their own versioned wire record; meeting time rides intervals with a meetingId
status: proposed
guards:
  - contract/src/lib.rs
  - cli/core/src/sync/batch.rs
origin: session
verified: false
---

## Decision

Meeting metadata crosses the wire as `AttestationBatch.meetings[]`, a `Meeting` record. Its fields:

- an id the daemon mints;
- a monotonic `version`;
- `state`: recorded, confirmed or retracted (a tombstone);
- `source`, `confidence`, `providerClass` and `evidence`, all closed enums;
- optional `CalendarFacts`;
- optional `Attribution`.

Meeting **time** rides ordinary `Interval`s that carry `meetingId` and `activity: "meeting"`.

The cloud rejects a lower version for an id it already holds. `(id, version)` is folded into
`batch_id_for_chunk`, and the flush gate fires on pending meeting changes, tracked by an
`updated_seq` cursor.

The record is native to contract 2.0. If it ships before 2.0, the same shape goes out as an
additive optional array in 1.9.0. `SessionKind` and `Harness` are not extended.

## Why

A meeting is confirmed, reassigned or retracted after it was first sent. That is an update, so an
event rowid cursor never sees it. Today's batch id hashes only events, artifacts and intervals, so
a retry would be acked as a duplicate and the confirmation lost. A versioned record with
tombstones makes every change idempotent and orderable.

Keeping the time in intervals means the cloud's read sites keep summing `intervals.humanSeconds`
correctly (DIRASH-0063).

## Rejected

- **An `Interval.meeting {…}` object**, from the October design. It has no place for versioning,
  retraction or attribution, and it duplicates metadata on every slice.
- **A new `SessionKind`.** Older cloud validators would reject it, and a meeting is not a session.
- **Latest-write-wins upserts without versions.** A delayed retry could undo a confirmation.

## Agent directives

- Every new field stays a closed enum, a timestamp, a count bucket, a boolean, a device-minted id,
  a workspace entity reference or a keyed hash. The denylist test's fixture populates them all.
- Never let a lower version overwrite a higher one, on either side.
- Run `just contract` after any change. Never hand-edit the generated artifacts.
