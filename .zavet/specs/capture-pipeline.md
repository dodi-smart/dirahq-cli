---
title: Zavet capture pipeline
version: 2
origin: session
verified: false
confidence: high
date: 2026-07-31
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
- Checks (`checks:`, both record types) bind an invariant to the command that
  proves it, as `label :: command`; an item with no separator IS the command
  and doubles as its own label, and an item with a label but no command
  verifies nothing and drops. dira parses, stores and displays them but NEVER
  executes one — running a command read out of a repo is `zavet verify`'s job
  in the plugin, on an explicit human invocation. No framework is detected,
  inferred or special-cased anywhere: the command is opaque and exit 0 is the
  whole contract.
- Corrections (`corrected-by:`, decisions only) point forward at a later
  record that corrects ONE claim. The corrected record stays `active` and
  keeps its body, so append-only holds; every recall path leads with the
  correction. The pointer canonicalizes like any decision id and may dangle.
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
  `zavet_spec_paths`/`zavet_spec_decisions` (0003), `zavet_checks` +
  `zavet_decisions.corrected_by` (0005), `zavet_trailers` keyed `(sha, seq)`.
  `zavet_checks` keys a check to either subject (`subject_kind` +
  `subject_key`) rather than splitting into two tables — the columns and every
  consumer are identical — and is replaced wholesale on upsert like guards.
  Spec decision links = frontmatter `decisions:` ∪ body D-refs, replaced
  wholesale on each capture; links live on the spec side only.
- Attribution: the unique active session for the repo or NULL — never
  guessed. Unattributed evidence is still counted and reported.

## Invariants

- Decision and spec bodies are LOCAL-ONLY and can never ride the attestation
  wire — the wire contract is content-free by tested invariant (D-0001).
- A check splits across the knowledge channel's tier boundary: the label is
  metadata and always rides, the command is content and rides only at `full`.
  A label names an invariant the way a title names a record; a command is a
  line of the repo's own build configuration and can name internal tooling,
  hosts and paths nobody agreed to publish by enabling sync.
- First-sight fields (`first_commit`, `created_at`, `source_session`) survive
  every later upsert: provenance points at the commit that INTRODUCED the
  record.
- The knowledge sweep shares the walk's blocking budget and never touches the
  accounting hot path.

## Open Questions

- Should huge backfills (>15-commit first sight) sweep older knowledge too,
  or is the bounded window acceptable long-term?
- A dangling `corrected-by` is knowable here (the target may simply not be
  captured yet) but is only reported by the plugin's `zavet check`. Whether
  dira should surface it too — and how it would tell "not captured yet" from
  "never existed" — is open.
- Checks are stored and shown but nothing correlates a `check_failed` guard
  event back to the check that produced it; the event carries only a decision
  id. Whether that link is worth a wire field is open.
