---
id: DIRASH-0067
title: Meeting restatements sync when the cloud advertises restatement windows, and the day ledger clips like the cloud
status: active
guards:
  - cli/dirad/src/meeting_sync.rs
  - cli/core/src/sync/meetings.rs
checks:
  - the clip matches the cloud's cases and never keeps time inside the region :: cargo test -p dira-core --lib sync::meetings
  - a restatement clips a straddling day-ledger row like the cloud :: cargo test -p dira-core --lib a_restatement_clips_a_straddling_row_like_the_cloud
  - an older cloud's meta advertises no capability :: cargo test -p dira-contract server_meta_roundtrips_camel_case
origin: recorded
verified: true
---

## Decision

The `DIRA_MEETINGS_SYNC` env gate is gone. Meeting sync (flush clipping and restatements) runs when the store holds meetings and the cloud's `GET /api/v1/meta` lists `restatement-windows` in `ServerMeta.capabilities`. While the capability is missing, the daemon re-reads the meta at most hourly.

When a batch with `windows` is acknowledged, the `day_intervals` ledger clips every row overlapping the restated region (the windows plus the batch's intervals) with `clip_outside`. That function mirrors the cloud's `clipClaim` (cloud D-0087).

A change whose padded read reaches a compacted day, or a day older than the 40-day ledger, sends its records alone, with no intervals and no window. Compaction never prunes the padded range of a change still waiting to be sent. A meeting slice with no event around it takes the identity of the latest event before the read's end.

## Why

The cloud applies `windows` to the device's claims in every workspace only from dodi-smart/dirahq-cloud#310. An older cloud reports the same `schemaVersion` (2.1.0) and would bill a restated minute twice (accounting review H1). A capability is the only signal the device can read. It also turns on with the cloud deploy itself, on every release channel, and serves as a kill switch.

The cloud compares each signed day rollup with the device's claims by ids and seconds. If the ledger dropped whole rows while the cloud clipped them, every restatement would raise `day_rollup_mismatch`.

A window over compacted days would rebuild no work there and delete claims the cloud rightly holds (review M3).

## Rejected

- Gate on `modules.meetings`. After the module is turned off, a flush would stop clipping around a meeting the cloud still bills.
- Rely on the release order alone. Every merge to `develop` ships a prerelease, and prerelease devices talk to the production cloud.
- Settle sooner than `2 × idle_seconds` after a meeting ends. The overlay reads events ±idle around the meeting, and they may still land until that read has passed.
- Split restatement slices at UTC midnight. Flush intervals are not split either, and both sides file a row on its start day.

## Agent directives

- Keep `clip_outside` byte-for-byte equal to the cloud's `clipClaim`. Change both, with their shared cases, together.
- Never declare a window over a range whose raw events may be compacted or whose ledger days were pruned.
- Never send a restatement unless the cloud lists `restatement-windows`.

## Verification

The `checks:` entries cover the shared clip cases, a 5000-case property, the ledger test built on the cloud's two-workspace fixture, and the meta default. The accounting review also ran a 200,000-case differential run of `clip_outside` against `clipClaim` (0 mismatches); that harness is not checked in.

## Open questions

- If someone retracts a meeting after its days were compacted, the change goes out records-only, and the meeting's slices stay billed on the cloud. Either the cloud stops billing claims of a retracted meeting, or the device sends a window over exactly that meeting's span. Neither is decided.
