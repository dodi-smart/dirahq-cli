---
id: DIRASH-0025
title: Token attribution resolves per turn, and never writes NULL when a fallback exists
status: active
guards:
  - cli/dirad/src/writer.rs
  - cli/core/src/tokens.rs
origin: recorded
verified: false
---

## Decision

A token turn's repo is resolved from **that turn's own `cwd`**, carried on the
transcript line, not from the event that triggered the capture pass. The chain
is, in order:

1. the turn's own `cwd`, resolved through the writer's `ProjectCache`;
2. the triggering event's project;
3. the session's sticky last-known-good project from `SessionRegistry`.

A row is written with `project = NULL` only when all three are unavailable, and
every such row is counted and warned about.

`TokenTurn.cwd` is capture-time provenance only: it never reaches the
`token_usage` table, the contract, or the wire.

## Why

`capture_tokens` used to be handed the project of the single event that
triggered it and stamped that onto every turn in the pass. One `Stop` whose cwd
failed to resolve therefore marked every turn since the last watermark
repo-less — potentially hundreds of turns — while the `Stop`s either side
resolved the same repo perfectly well.

The cloud's D-0026 makes that expensive rather than cosmetic: repo-less token
usage is neither counted nor shown, so an unattributed turn is not merely
unattributed, it is **invisible**. It is also unrecoverable in place — rows are
written `ON CONFLICT(id) DO NOTHING` and the byte watermark advances
immediately after, so no later, better-attributed pass can repair them.

**Turn-first, not event-first.** Sessions do change directory mid-flight
(`EventKind::CwdChanged` exists for that reason). Preferring the event would
preserve the same class of bug whenever the event resolves but to the *wrong*
repo.

**Fall back rather than write NULL.** `project::resolve` returns `None` both for
"this genuinely has no origin remote" and for "git transiently failed", and
cannot distinguish them. Given D-0026, the cost of those two errors is wildly
asymmetric: a wrongly-NULL row silently deletes real compute from the ledger,
while a repo-less turn inheriting its own session's repo is a small, bounded
mis-attribution. So attribution is favoured.

This also depends on the `ProjectCache` never caching a failure permanently. A
sticky negative verdict poisons every later event *and* every later turn for
that cwd until the daemon restarts, which would reduce per-turn resolution to
theatre. Successes are cached for the writer's lifetime — a repo's origin does
not move under a running daemon — while failures expire and are retried.

Per-turn resolution is free in the steady state precisely because of that cache:
a pass over hundreds of turns sharing one cwd performs a single git shell-out.

## Rejected

- **Carry forward the last known-good project only** — cheaper, but silently
  wrong for any session that changes repo mid-flight, which is a real case.
- **Persist `cwd` on `token_usage` / the wire** — a filesystem path is content,
  it is not needed after resolution, and D-0001 keeps the attestation wire
  content-free.
- **Repair existing NULL rows with an `UPDATE`** — token rows ride their own
  rowid cursor (D-0018) and an `UPDATE` does not move a rowid, so a repaired row
  below the cursor would never re-send and the device and cloud would then
  disagree. That is worse than a known, bounded gap. The source `cwd` is gone in
  any case: the watermark passed those lines and it was never stored.
- **Decode grok's cwd from its session path** — grok encodes cwd in
  `~/.grok/sessions/<encoded-cwd>/`, but the encoding is unverified. Guessing it
  would produce confidently wrong attribution, which is worse than the honest
  fallback. Grok keeps `cwd: None` and rides the chain.

## Agent directives

- Never attribute a batch of turns from a single event's project. If you add a
  harness, give its turns their own `cwd` or accept the fallback chain.
- Never write `project = NULL` where a fallback rung is available, and never
  drop the unattributed counter/warn — an invisible turn must stay visible to
  the operator even when it cannot be attributed.
- Do not make `ProjectCache` cache failures indefinitely, and do not move
  `branch` into it: branch is volatile and is re-resolved live per event.
- Do not add `cwd` to `token_usage`, the contract, or any synced payload.
