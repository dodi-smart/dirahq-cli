---
id: DIRASH-0044
title: A scratch session is an orchestrator only when it coordinates
status: active
guards:
  - cli/core/src/orchestration.rs
  - cli/core/src/report.rs
  - cli/dirad/src/state.rs
checks:
  - a repo-less scratch session is ordinary :: cargo test -p dira-core --lib orchestration::tests::a_scratch_only_session_with_no_spawn_and_no_repo_is_ordinary
  - first repo evidence flips a scratch session on :: cargo test -p dira-core --lib orchestration::tests::a_scratch_session_that_later_resolves_a_repo_flips_to_orchestrator
  - a scratch session that spawns is an orchestrator :: cargo test -p dira-core --lib orchestration::tests::spawning_from_scratch_or_from_no_repo_marks_an_orchestrator
  - a repo-less scratch session has no orchestration report line :: cargo test -p dira-core --lib report::tests::a_repo_less_scratch_session_is_unresolved_not_orchestration
  - a repo-less scratch session ships no lineage :: cargo test -p dirad --lib state::tests::a_repo_less_scratch_session_ships_no_orchestration_lineage
  - the verdict flip still ships one partial :: cargo test -p dirad --lib state::tests::lineage_changes_ship_one_partial_each_whatever_the_age
corrects: DIRASH-0042
origin: recorded
verified: true
---

## Decision

A session is an orchestrator only when it coordinates: it spawned a child, or it worked from a scratch workspace and resolved at least one repo.
In both cases it never resolved a repo from a cwd outside every scratch workspace (DIRASH-0042, unchanged).
A scratch-rooted session that spawned nothing and resolved no repo is an ordinary session. It ships no `role`, no `workspaceRef` and no `repos`, and its repo-less time is ordinary unresolved time, not the orchestration report line.
The verdict can flip on later, when the first repo evidence arrives, so the live registry ships a partial for that flip like any other lineage change.
This corrects the scratch clause of DIRASH-0041 and DIRASH-0042, which made scratch alone sufficient.

## Why

Any Claude Desktop session whose cwd is a scratch workspace became an orchestrator, even one that spawned nothing and touched no repo.
One hourly Claude Desktop scheduled task (dodi-smart-triage-refresh, workspace_ref 311a693093f0ade1) refreshes Artifact and `gh` dashboards from a scratch dir.
It accounted for 39 of 51 "orchestrators" in local data and 39 of 57 in prod.
Each shipped `role=orchestrator` with a workspaceRef, showed up as an orchestration run, and its time sat in the orchestration bucket, where the cloud keeps it permanently unverified.

## Rejected

- A display-only relabel (issue cli#181). The role drives cloud runs, billing and assurance, so a label fix leaves the wrong data on the wire.
- Requiring `spawned` alone. A scratch session that clones and works `repos/x` is coordination even if it spawns no one.

## Agent directives

- Never derive `orchestrator` from scratch alone. It needs a spawn, or a resolved repo.
- Keep `workspace_ref`, `role` and `repos_touched` gated on the verdict, so a repo-less scratch session stays byte-identical to an ordinary one on the wire.
- Count the verdict flip as a lineage change: `observe` compares the shape, which includes `orchestrator`.

## Verification

The `checks:` entries cover the rule, the late flip, the spawn path, the report line and the live registry's wire.
A 1.7 rollup's absent `role` is authoritative, but only live sessions ship another rollup (a partial on lineage change).
An ended session's rollup ships once, so prod rows that ended as orchestrators stay wrong until the one-shot re-send of
ended rollups lands (https://github.com/dodi-smart/dirahq-cli/issues/180).

## Open questions

- A scheduled task that runs `gh -R` against a repo from its scratch dir now counts as an orchestrator, because the repo evidence resolves under scratch.
