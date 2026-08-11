---
id: DIRASH-0028
title: Knowledge reindex is an explicit path-scoped command, never the ambient poll
status: active
guards:
  - cli/dirad/src/capture.rs
  - cli/dirad/src/zavet.rs
checks:
  - the collapse and skip rules hold :: cargo test -p dirad --lib zavet::
  - the pathspec walk stays scoped and bounded :: cargo test -p dira-core --lib project::tests::log_commit_refs
origin: recorded
verified: false
---

## Decision

The ambient git poll keeps its bounded first-sight walk
(`COMMIT_BACKFILL_LIMIT`). Recovering the knowledge history that bound skips is
a separate, user-initiated command, `dira zavet reindex`, which:

1. walks **full history scoped to a `.zavet/` pathspec** for decisions and specs
   — unbounded, because the pathspec is what keeps it cheap;
2. walks commit trailers under a **default bound** (`REINDEX_TRAILER_LIMIT`),
   liftable with `--all-trailers`, because trailers ride arbitrary commits and
   that walk is whole-history by nature;
3. **skips any record whose `content_hash` already matches** the stored row, and
   collapses each record's history to its first and last sighting;
4. **never writes `repo_baseline`**, and never runs on the ambient path.

The record walk drives the same `zavet_sweep` as the poll. The trailer walk
drives `zavet_trailers` — that function *is* `zavet_sweep`'s own first stage,
extracted and called by both, not a copy of it. There is one parser for
trailers and one for records, and the record parser is never invoked over the
trailer window (see Why).

Discovering the gap is NOT this command's job. `dira zavet decisions` / `wiki`
already report uncaptured on-disk records, per record and split by remedy
(DIRASH-0026). What this record changes there is one word: an `awaiting sweep`
record names `reindex`, not `sync`, whenever sync cannot reach it.

## Why

The mirror in SQLite is populated as a side effect of commit capture, which
bounds a repo's first walk and then sets the baseline — so history older than
that window is never revisited. On a fresh clone that is not an edge case but
the normal outcome: this repo carries 21 decisions and 5 specs across 161
commits, of which a 15-commit first-sight walk indexes almost none. Every query
path (`why`, `wiki`, `decisions`, `status`) reads the index, not the working
tree, so a new team member gets thin answers with the records sitting on disk
and no indication anything is missing. Sharing `.zavet/` through git is the
whole distribution model (D-0001), and it was silently only half-working.

The bound stays on the ambient path because it is load-bearing there: that walk
runs inside `CAPTURE_TIMEOUT` on a path that must never stall the writer. A
user-initiated command has no such constraint — it is off the hot path, the
accept loop spawns per connection, and it may take seconds.

The pathspec is what makes "full history" affordable rather than reckless: git
returns only commits that touched `.zavet/decisions` or `.zavet/specs` — 30 of
161 in this repo — so depth costs almost nothing. Trailers have no such scope,
which is why they are the one bounded half, and why the renderer says so
out loud instead of letting a partial scan read as exhaustive.

That asymmetry is also why the two walks call different entry points. Record
parsing costs one `git diff-tree` per commit plus a `git show` and a `git
rev-parse` per touched record; trailers cost one batched `git log --no-walk`
for the entire set. Measured on this repo, running the record parser across a
189-commit trailer window spends 2.4s and 189 subprocesses to produce results
that are then discarded, against 0.05s and one subprocess for the trailers
actually wanted — and that gap grows linearly with `--all-trailers`. Sharing
one function across both windows would mean paying the record parser's price
on the window that by design never needs it.

The content-hash skip is not an optimization. Every upsert bumps `touched_seq`,
which is the knowledge-sync cursor (D-0001's separate channel), so a reindex
that re-stamps every row re-pushes the entire knowledge set on each run.
Collapsing to first-and-last sighting is the same argument: the store preserves
first-sight fields on conflict and overwrites the living ones, so replaying a
record's middle revisions cannot change the final row — it can only churn the
cursor, and a middle revision's hash never matches the stored latest one, so
without collapsing, *every* run would re-push.

## Rejected

- **Raise or drop `COMMIT_BACKFILL_LIMIT`** — makes every first sight of every
  repo pay full-history cost inside the capture timeout, on the path that must
  not stall the writer. It also cannot be scoped: commit capture genuinely wants
  recent commits, knowledge wants all of `.zavet/`.
- **Reindex automatically on first sight** — same timeout problem, and it turns
  an explicit recovery into ambient behavior that fires on every new repo.
- **Read the working tree instead of git history** — cheap, but it has no
  `first_commit`, no `created_at`, and no trailers. Provenance is most of what
  `why` answers with; an index that knows *what* was decided but not *when* or
  *in which commit* is not the same index.
- **Let `--project` name a repo to reindex** — the walk reads git history off a
  working tree, so a repo the caller is not standing in has nothing to walk.
  The other `Zavet*` queries take `--project`; this one deliberately does not.
- **Attribute reindexed records to the current session** — a reindex replays
  other people's history. Stamping it with whichever session happens to be open
  would invent provenance the ambient path earns honestly.
- **Have `dira zavet status` reindex when it detects drift** — reporting and
  repairing stay separate (DIRASH-0022).
- **Count `*.md` on disk against indexed rows for a drift figure on `status`**
  — built first, then dropped. Two counts subtracted from each other must share
  one definition of "a record", or the difference is noise: a `README.md`, a
  dot-prefixed template, or a record whose frontmatter does not parse all read
  as permanently missing, and no reindex can clear them. DIRASH-0026's
  per-record probe answers this off `git ls-tree` and splits by remedy, which
  is strictly better; two overlapping signals on one subject is worse than one.
- **A `--force` flag on `dira zavet sync`** — forbidden by DIRASH-0027, and it
  would not work regardless: forcing a re-read still walks `baseline..HEAD`.
  Reaching behind the baseline needs a different walk, which is this command.
  DIRASH-0027 anticipates exactly this ("a deeper backfill ... with its own
  decision, not a flag on this command").

## Agent directives

- Never add a second implementation of decision/spec/trailer parsing. The poll
  and the reindex both reach it through `capture::zavet_sweep` /
  `capture::zavet_trailers`, and the latter is the former's own stage — extract
  and share, never copy. A reindex that disagrees with the ambient poll is
  worse than the under-indexing it fixes.
- Never run the record parser over the trailer window. It costs a `diff-tree`
  per commit plus a `show` and a `rev-parse` per touched record, and over that
  (deliberately wide) window every result is discarded.
- Never make the reindex ambient, and never give it `CAPTURE_TIMEOUT`. The
  bound on the poll and the absence of one here are the same decision.
- Never drop the `content_hash` skip or the first-and-last collapse: together
  they are what make a repeat run write nothing and push nothing.
- Never let the reindex write `repo_baseline` — commit capture owns that
  watermark and its own bound.
- If a walk is bounded, say so in the output. A silent bound is the bug this
  record exists for.
