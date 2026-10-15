---
title: Orchestration sessions and runs
version: 1
origin: session
verified: false
confidence: high
date: 2026-10-15
paths:
  - cli/core/src/orchestration.rs
  - cli/core/src/runs.rs
  - cli/core/migrations/0007_orchestration.sql
  - cli/sources/src/claude_code.rs
  - cli/dirad/src/writer.rs
  - cli/dira/src/session_titles.rs
decisions: [D-0001, DIRASH-0027, DIRASH-0041]
---

## Overview

Claude Desktop runs orchestration sessions. Their cwd is a scratch workspace
(`…/Claude/scratch-workspaces/<acct>/<org>/scratch-<date>-<id>/`), not a repo.
They reach repos through tool calls and spawn child sessions with
`mcp__ccd_session__start_session`. Dira attributes their repo work per event,
links their children, and shows the whole thing as one run.

## Behavior

- A session is an orchestrator when an event's cwd sits in a scratch workspace
  and is not itself a repo, or when it spawned a child. A clone inside the
  workspace is ordinary repo work.
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
