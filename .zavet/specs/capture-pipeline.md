---
title: Zavet capture pipeline
version: 1
origin: session
verified: false
confidence: high
date: 2026-07-16
paths:
  - cli/dirad/src/capture.rs
  - cli/core/src/zavet.rs
  - cli/core/src/project.rs
decisions: [D-0001]
---

## Overview

How dira turns ordinary git commits into zavet knowledge: trailers, decision
records, and living specs, captured as a byproduct of the daemon's existing
commit poll — no filesystem watcher, no extra daemon.

## Behavior

- The event-driven poll (`capture_commits`) resolves the repo's HEAD; an
  unchanged HEAD is a complete no-op. New commits are walked inside one
  `spawn_blocking` under a 10 s budget — an overrun drops the capture and the
  next commit-bearing event retries.
- When the repo is zavet-active (override > `modules.zavet` knob > `.zavet/`
  dir probe), the same walk sweeps knowledge: one batched trailer parse over
  the walked shas, then per commit (oldest first, so first-sight provenance
  points at the introducing commit) the `.zavet/decisions/*.md` and
  `.zavet/specs/*.md` blobs it touched are parsed and upserted.
- Dot-prefixed files (`.template.md`, `.spec-template.md`) and subdirectories
  are never captured; decision files must be flat `D-*.md`, specs flat
  `<slug>.md` (the filename stem is the spec's identity).
- Spec staleness is never materialized: it is computed at query time as
  `git log <last_commit>..HEAD -- :(glob)<path>…` over the spec's declared
  paths, in the repo dir the daemon last observed (else the caller's cwd;
  with neither it reads *unknown*, never guessed).

## Interfaces & data

- Parsing lives in `dira-core::zavet` (pure, no IO): a shared YAML-subset
  frontmatter walker feeds `parse_decision` and `parse_spec`; inline
  `# comments` are allowed on structured lines, never on `title`. Decision
  ids canonicalize to their zero-padded form at every ingestion point —
  which is also why this body avoids example refs: any id mentioned here
  auto-links.
- Storage: `zavet_decisions`/`zavet_guards` (0002), `zavet_specs`/
  `zavet_spec_paths`/`zavet_spec_decisions` (0003), `zavet_trailers` keyed
  `(sha, seq)`. Spec decision links = frontmatter `decisions:` ∪ body D-refs,
  replaced wholesale on each capture; links live on the spec side only.
- Attribution: the unique active session for the repo or NULL — never
  guessed. Unattributed evidence is still counted and reported.

## Invariants

- Decision and spec bodies are LOCAL-ONLY and can never ride the attestation
  wire — the wire contract is content-free by tested invariant (D-0001).
- First-sight fields (`first_commit`, `created_at`, `source_session`) survive
  every later upsert: provenance points at the commit that INTRODUCED the
  record.
- The knowledge sweep shares the walk's blocking budget and never touches the
  accounting hot path.

## Open Questions

- Should huge backfills (>15-commit first sight) sweep older knowledge too,
  or is the bounded window acceptable long-term?
