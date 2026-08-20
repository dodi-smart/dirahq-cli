---
id: DIRASH-0032
title: A record's first-sight triple is repaired as a unit, from recorded facts
status: active
guards:
  - cli/dirad/src/zavet.rs
  - cli/core/src/store.rs
checks:
  - reindex repairs an origin bound to an edit :: cargo test -p dirad --lib reindex_tests::reindex_repairs
  - the repair moves all three fields together :: cargo test -p dira-core --lib store::tests::repairing
origin: session
verified: true
---

## Decision

`dira zavet reindex` repairs `first_commit`, `created_at` and `source_session`
**together**, when it observes a sighting earlier than the one on record.
Attribution is read inside the repair from the `artifacts` row for the new
`first_commit`. It is never passed in, never taken from the ambient session,
and set to NULL when that commit was never captured.

Repairs only ever move an origin earlier, so they are idempotent.

## Why

`zavet_upsert_*` preserves first-sight fields on conflict, which is right for
the ambient path. But it means a record introduced long ago and first *seen* by
the poll on a later edit keeps that edit as its origin permanently, and the
content-hash skip meant reindex never even reached it.

That is not cosmetic. `zavet_sessions_for_decision` and
`zavet_commits_for_decision` both join `artifacts` on `first_commit`, so a wrong
value there bills `dira zavet why`'s cost to whoever made the edit.

**Why attribution is in scope.** Repairing the commit while leaving
`source_session` pointing at a different one produces a row that contradicts
itself. That row then ships to the cloud, where nothing re-derives it. Locally
the read paths recompute attribution from `artifacts` and would have healed
themselves; the denormalized column is precisely the copy that would have stayed
wrong. Resolving it inside the repair statement is what makes "attribution
tracks `first_commit`" structural rather than a convention a caller can forget.

This is not the ambient-session stamping DIRASH-0028 rejects. That would invent
provenance from whatever session happens to be open during a reindex; this reads
a recorded fact keyed by the commit itself. That is the same join the local
read path already trusts.

**Why NULL is allowed here.** DIRASH-0025 says never write NULL where a fallback
exists. The only candidate here is a session that provably belongs to a
different commit, which is not a fallback but a lie.

**Why earliest-wins.** The walk is unbounded over the `.zavet/` pathspec, but a
record whose file moved into it later has history the walk cannot see. Refusing
to move an origin forward keeps the repair monotone and therefore idempotent,
which is what keeps a second reindex a true no-op and stops `touched_seq`
re-pushing the whole knowledge set.

Dates compare as **instants**: `%aI` carries the author's local offset, so
`T01:00+02:00` precedes `T00:30Z` in fact and follows it in ASCII. On a tie,
where git resolves to the second, the walk wins, because it enumerates history
in order and the timestamp has lost that.

## Rejected

- **Repair dates only, leave attribution**: produces a self-contradictory row
  and leaves the cloud's copy wrong, since only the local reader re-derives.
- **Backfill attribution from the reindex's own session**: the invention
  DIRASH-0028 rules out.
- **Widen the content-hash skip's meaning instead**: content currency and
  provenance currency are different questions; folding them together is what
  hid this for so long.
- **Repair during the ambient poll**: it sees a window, not history, so it
  cannot know an earlier sighting exists. This is exactly the explicit,
  path-scoped work DIRASH-0028 gives reindex.

## Agent directives

- Never set `source_session` on a repair from anything but the `artifacts` row
  for the new `first_commit`. Do not add a caller-supplied parameter for it.
- Repairs move an origin earlier only. A rule that can move it forward is not
  idempotent, and a non-idempotent reindex re-pushes the entire knowledge set.
- Compare author dates as parsed instants, never as strings.
- Keep the content skip and the provenance check separate. They answer different
  questions, and `unchanged` must stay about content alone.
- Ordinary reindex writes still pass `source_session: None`. Attribution is set
  by the repair path and nowhere else.

## Verification

`reindex_repairs_an_origin_the_ambient_poll_recorded_from_an_edit` builds the
real shape: two commits with fixed author dates, a baseline that opens the
ambient window after the introducing commit, and an artifact row attributing
that commit to an earlier session. It asserts the fixture actually reproduces
the bug before asserting the repair, then that a second run reports zero and
does not re-trigger knowledge sync.

Store-level: the triple moves together, an uncaptured origin clears stale
attribution rather than keeping it, and a repair re-enters the `touched_seq`
window so the correction reaches the cloud.

Unit: earlier repairs, equal-instant defers to walk order, later never moves an
origin forward, offset-aware comparison, and an undatable sighting never
overwrites a dated record.
