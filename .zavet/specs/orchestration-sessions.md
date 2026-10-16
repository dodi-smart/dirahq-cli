---
title: Orchestration sessions and runs
version: 4
origin: session
verified: false
confidence: high
date: 2026-10-16
paths:
  - cli/core/src/orchestration.rs
  - cli/core/src/runs.rs
  - cli/core/src/report.rs
  - cli/core/migrations/0007_orchestration.sql
  - cli/sources/src/claude_code.rs
  - cli/dirad/src/writer.rs
  - cli/dira/src/session_titles.rs
  - cli/dirad/src/state.rs
decisions: [D-0001, DIRASH-0027, DIRASH-0041, DIRASH-0042]
---

## Overview

Claude Desktop runs orchestration sessions. Their cwd is a scratch workspace
(`…/Claude/scratch-workspaces/<acct>/<org>/scratch-<date>-<id>/`), not a repo.
They reach repos through tool calls and spawn child sessions with
`mcp__ccd_session__start_session`. Dira attributes their repo work per event,
links their children, and shows the whole thing as one run.

## Behavior

- A session is an orchestrator when it spawned a child or an event's cwd sits
  in a scratch workspace and is not itself a repo, unless the session also
  resolved a repo from a cwd outside every scratch workspace. A clone inside
  the workspace (`scratch-…/repos/x`) does not cancel the role.
- A session that spawns from its own repo is a spawner (DIRASH-0042). It keeps
  its sticky project and `repoCanonical`, has no role, and lists the repos it
  touched in `repos`. It heads a run because its children name it as parent.
- A spawned child often runs its first events in the parent's scratch dir, then
  moves to its own worktree of a real repo. It is an ordinary session: it keeps
  its sticky project and its rollup's `repoCanonical`. Its scratch events still
  carry the `workspace` digest; only the session-level verdict changes.
- `SessionLineage` folds the verdict from flags that only ever turn on, so the
  rollup, the live registry and reports agree whatever the event order. While a
  child's scratch preamble is all the registry has seen, it reads as an
  orchestrator; the first event in its worktree flips it for good. Any change
  to what the rollup would carry (role, parent, repos, workspace, repo) ships
  a partial rollup on the next flush (see attestation-sync), so the cloud's live view follows
  the flip. That early partial is only safe from contract 1.7, where the
  later rollup's absent `role` retracts the preamble's.
- The writer resolves an event's repo from the cwd first. When the cwd names no
  repo, it uses the tool input's evidence: a `cd` chain, `git -C`, `gh -R`,
  `gh api repos/…`, or an `Edit`/`Write`/`Read` path. Evidence naming two
  repos gives nothing.
- Only a directory that resolved enters `repo_dirs` (DIRASH-0027). A repo named
  outright (`gh -R`) registers nothing.
- The shim forwards `CLAUDE_CODE_HOST_SESSION_ID`. A child finds its parent
  from the event whose `spawned` holds its host id. Spawn records are exempt
  from coalescing, since they carry the only copy of the link.
- An orchestrator never gets a sticky project. Its repos are listed instead.
  A spawner keeps its project and also lists its repos.
- Reports show an `orchestration` line for an orchestrator's repo-less time,
  separate from `(unresolved)`. The split never changes a total.
- `dira sessions` groups runs into a tree. A run over at most one repo is
  folded and shows that repo. `--flat` gives the old one-row view. Titles come
  from `~/.claude/sessions/*.json` at render time and never leave the machine.

## Interfaces & data

- Events gain `host_session`, `workspace` (16-hex FNV-1a digest of the path
  below the scratch root), `spawned` and `parent_session` (migration 0007).
- `SessionView` gains `orchestrator`, `parent_session` and `repos`, all
  defaulted so an older daemon degrades to the flat view.
- Contract 1.6 `SessionRollup` gains `role`, `parentSession`, `repos` and
  `workspaceRef`. See attestation-sync.
- From contract 1.7, `role`, `repos` and `workspaceRef` are authoritative on
  every rollup. An absent field retracts what an earlier rollup sent, so a
  child whose scratch preamble read as an orchestrator heals in the cloud.
- A run's repos are every member's `project` plus `repos`. Only an
  orchestrator's repos can be `direct`.
- `runs::group` is the CLI lens. The cloud's `assembleRuns` follows the same
  rules and heads a run with the one cloud project covering all its repos.

## Invariants

- No tool argument, tool output or title is stored or sent.
- A run is a lens. Every session appears once, and no total changes.
- A parent cycle puts nobody in a run.

## Open Questions

- A child whose events all arrive before the parent's spawn record stays
  unlinked. Links resolve per event, so this needs a child that ends within
  one hook of starting.
