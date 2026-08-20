---
id: DIRASH-0026
title: Branch presence is a display state on the zavet list views, never a deletion
status: active
guards:
  - cli/dirad/src/zavet.rs
  - cli/dira/src/render.rs
  - cli/core/src/protocol.rs
checks:
  - off-branch and uncommitted records are both reported :: cargo test -p dirad --test zavet_presence
  - presence is unknown without a working directory :: cargo test -p dirad --test zavet_presence presence_is_unknown_without_a_working_directory
  - a long-guard decision is still one row :: cargo test -p dira --bin dira render::tests
origin: recorded
verified: true
---

## Decision

`dira zavet decisions` and `dira zavet wiki` report, per record, whether its
file is in `HEAD`'s tree (`on-branch` / `off-branch`) and list the records
found on disk that the store has never captured. All of it is computed per
query from one `git ls-tree` plus one directory read, and **no store row is
ever removed, tombstoned, or restyled as inactive**.

Presence is `Option<ZavetPresence>`. `None` means *unknown*, no working
directory to ask git in, and renders as nothing at all: the groups collapse to
one plain list rather than marking every record as belonging elsewhere.

Uncaptured records split by remedy: `uncommitted` (absent from `HEAD` too) and
`awaiting sweep` (in `HEAD`, not yet walked by the daemon).

## Why

The store keys knowledge by repo alone, with `WHERE repo = ?1` and no ref
predicate, and capture only ever inserts. A field report caught what that
produces: on branch `1881-…`, `dira zavet decisions` printed D-0001..D-0007
while the tree held D-0005..D-0011. Four records were from a branch the user
had left four days earlier, printing confident guard globs for files not in
that checkout; four freshly-written ones were absent because capture reads git
objects, not the working tree. The two sets overlapped in three records, and
neither half was visible as a discrepancy. The user noticed only the missing
end of the range.

The pooling itself is correct and has to stay. Ids are minted against the
repo-wide captured set, not the branch's files, which is why the first decision
authored on `1881-…` was numbered D-0005 on a branch containing no
D-0001..D-0004. Make the list branch-local and two branches both mint D-0012.
The append-only model says the same thing from the other direction: "this no
longer applies" is `status: superseded`, expressed in the record, never a
deleted row.

So the defect is not that the rows exist. It is that the UX says "the captured
decisions **for this repo**", with `.zavet/ present` printed above the count,
and invites the reader to hear "what governs the code in front of me" while
the list actually answers "everything this device ever saw anywhere in this
repo". Showing the difference costs one git call and closes the gap without
touching the model.

`unknown` is a third state rather than a default because the two ways to be
absent are not distinguishable from outside a working tree, and reporting every
record as off-branch when we simply could not look would be a worse lie than
saying nothing.

## Rejected

- **Add a `ref`/branch column to `zavet_decisions`**: the only version that
  makes the list and `.zavet/INDEX.md` agree by construction, but it makes a
  branch part of a record's identity, which the append-only model does not
  believe, and it needs a migration plus a capture-path rewrite to answer a
  question `ls-tree` already answers for free.
- **Handle `'D'` in `changed_paths` and delete rows for removed files**: makes
  a `git rm` on one branch silently erase knowledge for every branch, and
  strands the id allocator. If deletion semantics are ever wanted they need
  their own decision, not a side-effect of a filter written for a different
  purpose.
- **Hide off-branch records by default**: cleanest list, but a record that
  vanishes with no trace is the same class of failure as the one being fixed.
  `--branch` opts into the narrow view and still prints how many it set aside.
- **Read the working tree during capture so uncommitted records are
  captured**: makes the store's contents depend on unsaved editor state, and
  would ingest a half-written record as fact. Reporting the file as uncaptured
  is honest and needs no new ingest path.

## Agent directives

- Never delete, tombstone, or downgrade a `zavet_decisions` row because its
  file is absent from a tree. Presence is computed per query and lives only in
  the view.
- Never default an unknown presence to `off-branch`. `None` renders as
  nothing. That is the same rule `stale_commits` follows.
- Any new presence probe runs in ONE `spawn_blocking` for the whole batch, and
  decisions and specs go through the SAME probe: two `ls-tree` calls can
  straddle a checkout and report two different trees in one view. The repo
  toplevel is resolved once and passed in. Every probe that resolves its own
  both pays for a subprocess and reopens that window.
- An unknown `HEAD` makes *presence* unknown. It does NOT make the on-disk scan
  unknown. The files are still there. Never gate the uncaptured report on
  reading the tree, or a freshly-`git init`ed repo reports nothing uncaptured,
  which is the first thing a new user does and the case this record exists for.
- Match uncaptured records by id/slug, never by path. A record renamed in the
  working tree is the same record.
- If id allocation ever moves to a branch-local source, this record is wrong
  and must be superseded. The two halves are load-bearing together.

## Verification

`cli/dirad/tests/zavet_presence.rs` builds a real git repo and reproduces the
report: a record committed on `other` is marked `off-branch` while still
present in the list, an uncommitted one is reported `uncommitted`, a committed
but unswept one is reported `awaiting sweep`, a `--project` query with no cwd
reports `None` for every row, a record renamed since capture is not reported as
uncaptured, and a repo with no commits at all still reports its on-disk records.

Both regression tests were confirmed to fail against the code they guard.
Reintroducing the early bail-out on an unknown `HEAD` turns the unborn-branch
case red, rather than the test passing vacuously.
