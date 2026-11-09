---
id: DIRASH-0052
title: "Contract 2.0: chained batches, day rollups and count-only session stats"
status: active
guards:
  - contract/src/lib.rs
  - cli/dirad/src/chain.rs
checks:
  - the wire still carries no content fields, 2.0 fields included :: cargo test -p dira-contract
  - the chain advances only on ack and restarts an epoch :: cargo test -p dirad --lib chain
origin: session
verified: false
---

## Decision

`SCHEMA_VERSION` is `2.0.0`. Every `AttestationBatch` carries a per-device hash chain:

- `seq`: the next position in the chain;
- `prevBatchHash`: lowercase hex SHA-256 of the JCS payload of the previous **acknowledged**
  batch, `""` at seq 0;
- `chainEpoch`.

The chain is stamped at send time, before signing, and advances only on a 2xx. A retry therefore
re-sends a byte-identical payload, and a quarantined chunk never advances the chain. A local
wipe (`nuke`) or a cloud data-epoch reset starts a new epoch at seq 0. The state lives in `meta`
(`batch_seq`, `batch_prev_hash`, `chain_epoch`). The lineage re-send rides the same chain.

Additive fields, all optional:
- on the batch: `client` (version, channel, os, arch), `captureHealth`, `dayRollups[]`;
- on `SessionRollup`: `stats` (tool calls by category, files touched, agent lines +/−,
  subagents, compactions, permission prompts, approval wait, test runs by outcome) and
  `workspaceHint`;
- on `ArtifactRef`: `linesAdded` / `linesRemoved`;
- on `ServerMeta`: `acceptEncodings`, an empty list when absent.

A field the producer does not measure is omitted, never zero-filled.

## Why

The cloud is moving to a new schema with a `facts_daily` read model, and the user asked for more
data, more statistics and good security. The chain makes each device's history tamper-evident:
a dropped, altered or reordered batch breaks a hash the cloud recomputes. Day rollups let the
cloud reconcile its daily facts against what the device holds. The stats answer what users ask
of the dashboard, still as counts only (D-0001).

The chain requires `seq` and `prevBatchHash` on every batch, which is a breaking change, hence
the major bump. The cloud accepts 1.x through adapters during a transition window. Release
order: CLI develop → cloud develop → cloud main (accepting 1.x and 2.0) → CLI main.

## Rejected

Agreed with the cloud-redesign session on 2026-11-09:
- **A session-level model mix.** `tokenUsage[].model` already carries it per turn.
- **An interrupt count.** No hook signals interrupts; only transcript-text scraping could.
- **A "human lines" field.** It would claim more precision than the data has. The cloud derives
  commit lines minus agent lines and labels it an estimate.
- **An untyped `meetings` slot.** A typed `meetings[]` lands later as an additive 2.x minor.
- **A chain over every channel.** Presence, knowledge and billing are not attestations and stay
  unchained.

## Agent directives

- Never advance the chain except on an acknowledged attestation batch. Stamp before signing.
- Every new 2.x field is optional and omitted when unmeasured. Populate it in
  `wire_contract_carries_no_content_fields` and in the signing vector.
- Producer status (`verified: false`):
  - **Filled now:** the chain, `client`, `ServerMeta.acceptEncodings`.
  - **Not filled yet:** `stats`, `dayRollups`, `captureHealth`, `linesAdded`/`linesRemoved`,
    `workspaceHint`, and zstd request bodies.
