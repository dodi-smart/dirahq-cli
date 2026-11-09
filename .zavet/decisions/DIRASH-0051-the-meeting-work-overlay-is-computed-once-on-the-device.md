---
id: DIRASH-0051
title: The meeting/work overlay is computed once, on the device; precedence is per user and a meeting wins across workspaces
status: proposed
guards:
  - cli/core/src/accounting.rs
  - cli/core/src/report.rs
  - cli/core/src/store.rs
  - cli/core/src/sync/batch.rs
origin: session
verified: false
---

## Decision

Meetings enter accounting as **spans**, not as synthetic `ManualTick` events. A pure function,
`accounting::overlay`, joins them with the unchanged `counted_gaps`. Every wall-clock second gets
exactly one slot: `Work(project)` or `Meeting(project?)`. Seconds where both overlap are counted
separately as `engaged_during_meeting_s`.

**Precedence:**

- Within one workspace, the workspace's `overlapPrecedence` decides between its meeting and its
  work. The default is meeting wins.
- Across workspaces, **the meeting wins**.

**Other rules:**

- A span's edges act as human signals (bridge minutes).
- Inputs are snapped to milliseconds, and cut gaps keep their whole seconds.
- The overlay runs on unclipped input padded by ±idle, then splits at midnight.

The overlay is **materialized once, on the device**:

- Wire intervals are its slices.
- A version change re-emits the slices for `[span−idle, span+idle]`, together with a span
  tombstone that forces the cloud's rebuild-delete across workspaces.
- Days with meetings that are not yet final are not compacted.

Agent time is untouched.

## Why

The `accounting-reviewer` pass on the design showed what goes wrong otherwise:

- **Overlay per workspace** bills one minute in two workspaces when their precedences differ (H1).
- **Overlay at read time in the cloud** needs about 24 read sites that sum
  `intervals.humanSeconds` to change. Each one missed double counts (H2).
- **Synthetic ticks** cannot express precedence or retraction.

Materializing on the device keeps `Σ intervals = dira report` (`batch.rs:5-7`) true by
construction.

## Rejected

- **Synthetic `ManualTick` events.** Overlaps split by signal order, a retraction leaves stale
  intervals, and cross-workspace overlaps double count.
- **Unclipped intervals plus a cloud-side overlay with a cross-language vector.** The bug would sit
  in the callers, not the function, so parity tests would not catch it.
- **Precedence per workspace only.** It double counts across workspaces.

## Agent directives

- With no spans, every report, interval and rollup must be byte-identical to the result without
  the overlay.
- Never compute human time for a billing or reporting surface from raw gaps when meeting spans
  exist. The live registry and presence stay raw and are labelled as such.
- Never put unattributed meeting time into the unresolved or orchestration buckets. It is
  `Meeting(None)`.
- Changes here are reviewed by the `accounting-reviewer` agent and keep the overlay proptests
  green.
