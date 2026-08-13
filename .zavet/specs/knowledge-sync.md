---
title: Knowledge sync — the consent-gated second channel
version: 1
origin: session
verified: false
confidence: medium
date: 2026-08-11
paths:
  - cli/core/src/sync/knowledge.rs
  - cli/dirad/src/knowledge_sync.rs
decisions: [D-0001, D-0020, DIRASH-0028, DIRASH-0030]
---

## Overview

How captured zavet knowledge reaches the cloud: a channel entirely separate
from attestation sync, with its own endpoint, its own four cursors, and a
double consent gate. It exists because the attestation wire is content-free by
tested invariant (D-0001) — knowledge could not ride it, and coupling knowledge
consent to billing consent was the thing that decision refused.

Written at `confidence: medium` and `verified: false`: this documents an
existing implementation that had no spec, so it is reverse-engineered from the
code rather than recorded as it was built.

## Behavior

- Nothing runs unless `[sync] knowledge` is `metadata` or `full`. The default
  is **`off`**, and the cloud independently enforces the workspace's tier — two
  consents, either of which alone ships nothing.
- **How the tier gets set.** `dira config set sync.knowledge off|metadata|full`,
  or `DIRA_SYNC__KNOWLEDGE`. The knob was absent from `config_cmd`'s `KNOBS`
  table until DIRASH-0030, so the only routes in were hand-editing
  `config.toml` or the env var — which made a consent prompt impossible to
  offer honestly.
- **Where consent is asked.** `dira onboard`'s knowledge step, in a prompt of
  its own that names what `full` sends (record bodies, trailer values, guard
  check commands), defaulting to `full` and declining to `metadata`. It is
  never bundled into device linking or billing consent, and the resulting tier
  is restated in the summary on every path including `--yes`. That prompt is
  the only consent UX this channel has: the tier appears in no status or
  doctor view (DIRASH-0030).
- Same debounce/backstop shape as attestation sync, at a slower cadence:
  3 s debounce, 120 s backstop. Knowledge moves at commit speed, not event
  speed. Triggers are lossy `try_send` nudges; the backstop covers a miss.
- Cursors advance only on a 2xx, per chunk, exactly as attestation sync does
  (D-0020). Guard events chunk at 500 items; decisions, specs, trailers and the
  repo-stats snapshot ride the final chunk, mirroring how artifacts ride the
  last attestation chunk.
- Two channel-specific error paths, neither of which wedges the channel:
  - **`knowledge_disabled` (403)** — the workspace opted out. Back off at the
    ceiling with a distinct health kind; nothing is wrong locally.
  - **`content_not_allowed` (400)** — the batch carried content the workspace
    has not opted into. `KnowledgeBatch::strip_content()` downgrades the same
    window to the metadata tier and retries **once**.
  - A **404** means the cloud predates the endpoint: quietly skipped
    (`endpoint_missing`), not logged as a failure.
- The per-repo coverage snapshot rides a throttle (`knowledge_stats_at:<repo>`)
  so the git pass behind it is not repeated per flush. Its window is
  `STATS_WINDOW_DAYS` (90), clamped down to the period `.zavet/` has actually
  existed in the repo — a rolling window must not span history that predates
  the practice it measures.

## Interfaces & data

- Four cursors, in `meta`, because the four tables move differently:

  | key | tracks | why this shape |
  |---|---|---|
  | `knowledge_cursor_decision_seq` | `zavet_decisions.touched_seq` | upserted, so a rowid would never move on an edit |
  | `knowledge_cursor_spec_seq` | `zavet_specs.touched_seq` | same |
  | `knowledge_cursor_trailer_rowid` | `zavet_trailers.rowid` | insert-only |
  | `knowledge_cursor_guard_event_id` | `zavet_guard_events.id` (ULID) | ULID-keyed, monotonic |

  All four blank together on a `dataEpoch` change, on `nuke`, and on
  `dira device resync` — wipe-and-resync reproduces cloud state because the
  cloud is idempotent by natural key.
- `touched_seq` (migration 0004) is bumped by every decision/spec upsert. That
  makes it the channel's change signal, and it is why any bulk re-ingest has to
  care: a writer that re-stamps unchanged rows re-pushes the entire knowledge
  set. `dira zavet reindex` skips records whose content hash and path both
  match precisely to avoid that (DIRASH-0028).
- The tier boundary is a field-level split, not a per-record one.
  `strip_content()` is the exact definition: it clears decision and spec
  `body_md`, every check's `command`, and every trailer ref's `value`, then
  relabels the batch `metadata`. Everything else — ids, titles, statuses,
  guards, check labels, record shas, counts — is metadata and always rides.
- Derivation (`cli/core/src/sync/knowledge.rs`) is pure and unit-testable; the
  daemon (`cli/dirad/src/knowledge_sync.rs`) owns scheduling, signing and
  transport. The split mirrors `sync/batch.rs` vs `dirad/sync.rs`.

## Invariants

- The default is `off`. Knowledge never leaves the machine because someone
  linked a device — that is the attestation channel's consent, not this one
  (D-0001).
- Content requires BOTH consents. A producer at `full` against a workspace that
  has not opted in ships metadata, never content.
- Consent for content is obtained by an explicit question that names the
  content, never as a side effect of another step. Any change to what `full`
  transmits changes `steps::KNOWLEDGE_DISCLOSURE` in the same commit.
- A tier set on an unlinked machine is reported as pending, never as active:
  the flush is gated on a cloud URL and a linked device, so the setting is
  recorded and inert until then.
- A cursor advances only after the cloud acknowledges the chunk carrying it.
- This channel never touches `AttestationBatch`, and no type under `/contract`
  used by it may carry a content-named field that the wire denylist would
  reject on the billing side.

## Open Questions

- Nothing reconciles the cloud's knowledge against local deletions: the cursors
  are append/advance-only, so a record removed locally is never retracted. The
  same gap as the local index has (see `capture-pipeline`), one level further
  out.
- The `content_not_allowed` retry downgrades the window once. Whether a
  producer stuck at `full` against a metadata-only workspace should also
  self-demote its local knob — rather than re-learning this on every flush — is
  open.
- Coverage stats are computed per repo on a throttle, but the throttle key is
  time-based only. A repo whose `.zavet/` changed heavily inside the throttle
  window reports a stale snapshot; whether that matters has not been measured.
