---
title: Zavet capture pipeline
version: 4
origin: session
verified: false
confidence: high
date: 2026-08-10
paths:
  - cli/dirad/src/capture.rs
  - cli/dirad/src/zavet.rs
  - cli/core/src/zavet.rs
  - cli/core/src/project.rs
decisions: [D-0001, DIRASH-0027, DIRASH-0028]
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
  are never captured; decision files must be flat `<PREFIX>-<digits>[-slug].md`
  with the prefix UPPERCASE, specs flat `<slug>.md` (the filename stem is the
  spec's identity). Uppercase is load-bearing rather than cosmetic: the old
  `D-` literal excluded stray files for free, and a case-insensitive shape
  would let `notes-2024.md` read as a record.
- That first-sight window is bounded AND followed by a baseline write, so a
  clone whose `.zavet/` history predates the window indexes almost nothing and
  no later sweep revisits it — `zavet sync` included, since it honors the same
  baseline. `dira zavet reindex` is the explicit recovery: full history scoped
  to a `.zavet/` pathspec for decisions and specs, a separately bounded pass
  for trailers (`--all-trailers` lifts it), driving the same sweep. It skips
  records whose content hash and path both already match, collapses each record
  to its first and last sighting, attributes to no session, and never writes
  `repo_baseline` — so a repeat run writes nothing and pushes nothing. This is
  the deeper backfill DIRASH-0027 anticipates, not a `force` flag on sync.
  See DIRASH-0028.
- Spec staleness is never materialized: it is computed at query time as
  `git log <last_commit>..HEAD -- :(glob)<path>…` over the spec's declared
  paths, in the repo dir the daemon last observed (else the caller's cwd;
  with neither it reads *unknown*, never guessed).

- `dira zavet sync` (`Request::ZavetSync`) runs one sweep on demand and
  registers the repo dir (DIRASH-0027). Both halves matter: the idle ticker only
  visits repos in `repo_dirs`, which is empty after a daemon restart and is
  otherwise filled only by session registration and agent events, so an unswept
  repo had no user-side remedy at all. It reuses `capture_commits` rather than
  forcing a re-read, so an unchanged HEAD stays a no-op here too, and it can
  never pick up an uncommitted record.

## Interfaces & data

- Parsing lives in `dira-core::zavet` (pure, no IO): a shared YAML-subset
  frontmatter walker feeds `parse_decision` and `parse_spec`; inline
  `# comments` are allowed on structured lines, never on `title`. Decision
  ids canonicalize to their zero-padded form at every ingestion point —
  which is also why this body avoids example refs: any id mentioned here
  auto-links.
- Ids carry a PER-REPO prefix and padding width, read from `.zavet/config`
  into a `ZavetConfig` once per sweep (`capture.rs`) or per resolved repo
  (`dirad::zavet`). No config means `D` at width 4, so a repo scaffolded
  before prefixes behaves byte-identically and needs no migration. Retired
  prefixes live in `prefix-aliases` and stay resolvable forever, because
  records are append-only and an id keeps the prefix it was minted under.
- The prefix set restricts FREE-TEXT scanning only (`scan_all_decision_refs`,
  which feeds trailer refs and spec body auto-linking). Filename and
  frontmatter grammar are shape-only: the decisions directory is closed, so a
  generic prefix is safe there, while prose is full of `UTF-8`, `SHA-256`,
  `RFC-2119` and `CVE-2024` that a generic scanner would read as references.
- A guard event is normalized but NOT padded by `parse_guard_event`: it is
  parsed before its `cwd` is resolved to a repo, so the width is unknown, and
  padding at the wrong width would key the event away from the record captured
  from that same repo. `dirad::zavet::ingest` canonicalizes once it can read
  the config.
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
  accounting hot path. The reindex path is the one exception and earns it by
  being user-initiated and off the poll: it takes no capture timeout, and it
  still drives the same sweep rather than a second parser (DIRASH-0028).

## Open Questions

- A dangling `corrected-by` is knowable here (the target may simply not be
  captured yet) but is only reported by the plugin's `zavet check`. Whether
  dira should surface it too — and how it would tell "not captured yet" from
  "never existed" — is open.
- Checks are stored and shown but nothing correlates a `check_failed` guard
  event back to the check that produced it; the event carries only a decision
  id. Whether that link is worth a wire field is open.
