---
title: Zavet capture pipeline
version: 7
origin: session
verified: false
confidence: high
date: 2026-08-14
paths:
  - cli/dirad/src/capture.rs
  - cli/dirad/src/zavet.rs
  - cli/core/src/zavet.rs
  - cli/core/src/project.rs
decisions: [D-0001, DIRASH-0025, DIRASH-0026, DIRASH-0027, DIRASH-0028, DIRASH-0032]
---

## Overview

How dira turns ordinary git commits into zavet knowledge: trailers, decision
records, and living specs, captured as a byproduct of the daemon's existing
commit poll. No filesystem watcher, no extra daemon.

## Behavior

- The event-driven poll (`capture_commits`) resolves the repo's HEAD; an
  unchanged HEAD is a complete no-op. New commits are walked inside one
  `spawn_blocking` under a 10 s budget. An overrun drops the capture and the
  next commit-bearing event retries.
- When the repo is zavet-active (override > `modules.zavet` knob > `.zavet/`
  dir probe), the same walk sweeps knowledge: a batched trailer parse
  (`commit_trailers`, chunked at 1000 shas per `git log --no-walk` call to
  stay under the OS argv ceiling. Past ~25k shas in one call the spawn fails
  and used to silently empty the whole result) over the walked shas, then per
  commit (oldest first, so first-sight provenance points at the introducing
  commit) the `.zavet/decisions/*.md` and `.zavet/specs/*.md` blobs it touched
  are parsed and upserted.
- Checks (`checks:`, both record types) bind an invariant to the command that
  proves it, as `label :: command`; an item with no separator IS the command
  and doubles as its own label, and an item with a label but no command
  verifies nothing and drops. dira parses, stores and displays them but NEVER
  executes one. Running a command read out of a repo is `zavet verify`'s job
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
  no later sweep revisits it. `zavet sync` is included, since it honors the
  same baseline. `dira zavet reindex` is the explicit recovery: full history
  scoped to a `.zavet/` pathspec for decisions and specs (`records`), a
  separately bounded pass for trailers (`trailer_commits`; `--all-trailers`
  lifts it), driving the same sweep. Because the two walks cover different
  windows, the scoped one's OWN trailer stage (`sweep.trailers`, unbounded) is
  merged with the wide one (`merge_trailer_windows`) before recording. A
  commit old enough to sit past the trailer bound but that also touched
  `.zavet/` would otherwise have its trailers silently dropped every run. It
  skips records whose content hash and path both already match, collapses
  each record to its first and last sighting, attributes no session on the
  ordinary write path, and never writes `repo_baseline`. So a repeat run
  writes nothing and pushes nothing. It also **repairs provenance**, which is
  a separate question from content and is checked separately (DIRASH-0032).
  First-sight fields are excluded from both upserts' `DO UPDATE SET`, so
  whichever row landed first owned `first_commit`/`created_at` forever: on a
  fresh clone the bounded ambient walk stamps any record edited inside its
  window with that recent commit, and a content-only skip meant reindex never
  revisited it. That is not cosmetic. `zavet_sessions_for_decision` and
  `zavet_commits_for_decision` both join `artifacts` on `first_commit`, so a
  wrong origin bills `dira zavet why`'s cost to whoever made the edit. Reindex
  now moves `first_commit`, `created_at` and `source_session` together, and
  only ever earlier; attribution is read inside the repair from the artifacts
  row for the new origin, never carried over and never taken from the session
  running the reindex. Dates compare as parsed instants (`%aI` carries the
  author's offset), and a same-second tie defers to walk order. Because the
  repair is monotone it is idempotent, so a second run still reports zero and
  does not re-push the knowledge set. The trailer half of that claim is
  counted, not assumed: `trailer_commits_recorded` is a before/after
  `zavet_counts` delta on the actual row count (mirroring `sync`'s own
  pattern), not a count of offers to `zavet_record_trailers`. The latter kept
  a repeat run reporting non-zero and re-triggering knowledge sync even though
  every write was a no-op (INSERT OR IGNORE). This is the deeper backfill
  DIRASH-0027 anticipates, not a `force` flag on sync. See DIRASH-0028.
- Spec staleness is never materialized: it is computed at query time as
  `git log <last_commit>..HEAD -- :(glob)<path>…` over the spec's declared
  paths, in the TRUSTED root `query_repo` resolves (see below); with none it
  reads *unknown*, never guessed.

- `dira zavet sync` (`Request::ZavetSync`) runs one sweep on demand and
  registers the repo dir (DIRASH-0027). Both halves matter: the idle ticker only
  visits repos in `repo_dirs`, which is empty after a daemon restart and is
  otherwise filled only by session registration and agent events, so an unswept
  repo had no user-side remedy at all.
  An entry in `repo_dirs` is a standing instruction to run git in that directory
  and record what it finds under that name, so a directory is registered only
  against a repo it demonstrably belongs to. A name *derived from* a cwd is
  evidence about that cwd and registers freely; a name the caller *asserted*
  (`dira start --project <repo>`) is trusted only when `project::resolve(cwd)`
  independently resolves to it. The same check `query_repo` applies, on the
  other entry point that can carry a caller-supplied name. Without it,
  `dira start --project` run from an unrelated checkout enrolled that checkout
  and the ticker captured its commits under the named repo. The verification
  lives in the command, never in `register_repo_dir`, which stays I/O-free
  because the writer loop calls it per event. It reuses `capture_commits`
  rather than forcing a re-read, so an unchanged HEAD stays a no-op here too,
  and it can never pick up an uncommitted record.
- Every `Zavet*` query command (`decisions`, `sync`, `wiki`, `why`, `spec_why`)
  resolves its repo AND working directory together through one ladder,
  `query_repo`: with no explicit `--project`, both come from `cwd`. With an
  explicit `--project <repo>`, the daemon's remembered directory for that repo
  wins if it has one; otherwise `cwd` is trusted as `repo`'s own tree ONLY IF
  `project::resolve(cwd)` independently resolves to that exact repo, never
  assumed. An unrelated `cwd` (or none) yields no root: `sync` returns its
  honest "no working directory known" error rather than sweeping the wrong
  checkout, and every read degrades to *unknown* the same way a missing
  workdir already does (DIRASH-0026): presence `None`, no uncaptured scan,
  branch `None`. Before this, the fallback was unconditional: `--project
  <repo-the-daemon-has-never-seen>` handed `sync` (and every read) the
  CALLER'S checkout, which then got registered and captured under the NAMED
  repo.

## Interfaces & data

- Parsing lives in `dira-core::zavet` (pure, no IO): a shared YAML-subset
  frontmatter walker feeds `parse_decision` and `parse_spec`; inline
  `# comments` are allowed on structured lines, never on `title`. Decision
  ids canonicalize to their zero-padded form at every ingestion point,
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
  `subject_key`) rather than splitting into two tables. The columns and
  every consumer are identical. It is replaced wholesale on upsert like
  guards. Spec decision links = frontmatter `decisions:` ∪ body D-refs,
  replaced wholesale on each capture; links live on the spec side only.
- Attribution: the unique active session for the repo or NULL. It is never
  guessed. Unattributed evidence is still counted and reported. The one path
  that sets attribution without a live session is the reindex provenance
  repair, which reads it from the `artifacts` row for the introducing commit.
  That is a recorded fact keyed by that commit, not an inference
  (DIRASH-0032). It writes NULL when that commit was never captured, because
  the only alternative is a session belonging to a different commit.
- **`repo_dirs` holds only verified pairs.** A directory is registered under a
  repo name only when the name was derived from that directory, or when the
  directory independently resolves to that name. Every write goes through
  `register_repo_dir`, which stays I/O-free.
- Every decision/spec upsert bumps `touched_seq`, which is the change signal
  the knowledge channel reads. What happens to a captured record after this
  point is `knowledge-sync`'s spec, not this one. That covers cursors, tiers
  and the consent gate. The coupling matters in one direction only: a writer
  here that re-stamps unchanged rows silently re-pushes the whole knowledge
  set.

## Invariants

- Decision and spec bodies are LOCAL-ONLY and can never ride the attestation
  wire. The wire contract is content-free by tested invariant (D-0001).
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

- **Nothing reconciles the index against HEAD.** Capture only ever inserts, and
  a full-history walk can re-add a record whose file was later renamed or
  deleted. DIRASH-0026 makes this *visible* (`off-branch`) rather than silent,
  and deliberately never deletes a row, since ids are minted repo-wide. But
  there is still no answer for a record that is genuinely gone rather than
  merely on another branch, and the resurrected row inflates the totals a drift
  figure would be computed from.
- A dangling `corrected-by` is knowable here (the target may simply not be
  captured yet) but is only reported by the plugin's `zavet check`. Whether
  dira should show it too, and how it would tell "not captured yet" from
  "never existed", is open.
- Checks are stored and shown but nothing correlates a `check_failed` guard
  event back to the check that produced it; the event carries only a decision
  id. Whether that link is worth a wire field is open.
