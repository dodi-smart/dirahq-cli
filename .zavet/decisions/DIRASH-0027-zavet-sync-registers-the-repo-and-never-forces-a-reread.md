---
id: DIRASH-0027
title: zavet sync registers the repo and reuses the ordinary sweep, never forcing a re-read
status: active
guards:
  - cli/dirad/src/zavet.rs
  - cli/dirad/src/control.rs
checks:
  - an unseen repo is swept and registered :: cargo test -p dirad --test zavet_sync
  - unchanged HEAD stays a no-op :: cargo test -p dirad --test zavet_sync a_second_sync_with_unchanged_head_captures_nothing
origin: recorded
verified: true
---

## Decision

`dira zavet sync` runs one capture pass for the current repo and records its
toplevel in `repo_dirs`. It calls the same `capture_commits` the idle ticker
does: no `force`, no bypass of the unchanged-HEAD short circuit. So it can
only ever pick up what is already committed.

`state.repo_dirs` gets a single writer, `control::register_repo_dir`, which
returns whether the repo was ABSENT before.

## Why

The ticker sweeps exactly the repos in `repo_dirs`, and that map is populated
only by manual session start and by agent events carrying a cwd. It is empty on
every daemon restart. So a repo nobody has opened a session in is never swept
at all. Its decisions stay uncaptured indefinitely, and there was no command
to force one. That hole had no user-side remedy; registering here is the fix,
not a side effect of it.

The latency it also closes (30 s active, ~5 min deep-idle) is the smaller half.

**No `force`.** Re-reading regardless of HEAD cannot help: it would re-ingest
blobs already stored, and still could not see an uncommitted file, because
capture reads git objects and never the working tree (DIRASH-0026). It would
also break the "unchanged HEAD is a complete no-op" invariant that
`zavet_capture.rs` pins for the ticker. Uncommitted records are REPORTED by the
same `working_tree_probe` the list views use, and never ingested.

**"Was absent", not "changed".** The writers store different shapes for one
checkout: a session's raw cwd, possibly a subdirectory, versus the toplevel
sync resolves. So a "did the value change" answer flips on every alternation
between them and would tell a human "repo registered" on every single run.

## Rejected

- **Sweep every repo the store knows about**: `repo_dirs` is deliberately a
  map of directories the daemon has OBSERVED, not of repos it has rows for. A
  path that is stale, unmounted, or belongs to a deleted checkout is not
  something to shell out to git in on a timer.
- **Register inside `capture_commits`**: it is called from the writer's
  detached task on the hot path; resolving a toplevel there would add a git
  spawn per event. The registration belongs to the human-initiated command.
- **Have `register_repo_dir` resolve the toplevel for every caller.** Same
  problem: the agent-event writer calls it per event, and `toplevel()` shells
  out to git. Readers resolve the root instead, which is what its doc says.

## Agent directives

- Never add a `force`/`--refresh` flag that bypasses the baseline check in
  `git_walk`. If a deeper backfill is ever wanted, that is a change to the
  capture window with its own decision, not a flag on this command.
- All writes to `state.repo_dirs` go through `register_repo_dir`. Do not
  re-introduce a bare `insert`. The map decides what gets captured at all.
- Keep `register_repo_dir` free of I/O. It is called from the writer loop.
- A reader that needs a repo ROOT resolves it (`repo_root`); the map's value
  may be a subdirectory.

## Verification

`cli/dirad/tests/zavet_sync.rs` drives the real `control::dispatch`: an unseen
repo is swept and registered with the toplevel; a second sync captures nothing;
an uncommitted record is reported rather than ingested and is captured only
once committed; and a `--project` with no known directory errors instead of
reporting a successful zero-capture sync.

Not mechanically checked: that no future caller adds a bare `repo_dirs.insert`.
The guard glob on `control.rs` is what puts this record in front of a reviewer.
