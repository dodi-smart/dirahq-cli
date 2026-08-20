---
id: D-0001
title: The attestation wire is content-free; knowledge sync needs its own channel
status: active
guards:
  - contract/**
  - cli/core/src/sync/**
origin: recorded
verified: true
---

## Decision

Nothing content-bearing (prompts, diffs, commit messages, decision bodies,
trailer values) ever rides `AttestationBatch`. Zavet's decision `body_md` and
trailer values are stored local-only; any future cloud sync of knowledge goes
through a separate `KnowledgeEnvelope` with its own cursor and consent gate.

## Why

The billing attestation channel is trusted because it is provably policy-free
and content-free. `wire_contract_carries_no_content_fields` denylists content
tokens on every wire field name. Mixing knowledge prose into it would poison
that guarantee and every privacy claim built on it.

## Rejected

- Additive optional fields on `AttestationBatch`. Breaks the invariant test by
  design, and couples knowledge consent to billing consent.

## Agent directives

- Never add a field named like content (`body`, `text`, `diff`, `prompt`, …) to
  any type in `contract/`.
- New zavet columns/fields must not collide with the wire denylist tokens.
- Cloud sync of zavet data (M2) starts metadata-only; content requires its own
  explicit opt-in knob. Design note: `docs/zavet.md`.
