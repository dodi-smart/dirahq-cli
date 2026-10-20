---
id: DIRASH-0042
title: A session that spawns from its own repo keeps the repo, and 1.7 rollups can retract a verdict
status: active
guards:
  - cli/core/src/orchestration.rs
  - cli/core/src/runs.rs
  - contract/src/lib.rs
checks:
  - a spawner keeps its repo :: cargo test -p dira-core --lib orchestration::tests::a_session_that_spawns_from_its_repo_keeps_the_repo
  - a spawner heads its run with its own repos :: cargo test -p dira-core --lib runs::tests::a_spawner_heads_its_run_with_its_own_repo_and_reach
  - a worktree spawner keeps its repo end to end :: cargo test -p dirad --test orchestration a_worktree_session_that_spawns_a_child_keeps_its_repo
corrected-by: DIRASH-0044
origin: recorded
verified: true
---

## Decision

A session is an orchestrator when it worked from a scratch workspace or spawned a child, and never resolved a repo from a cwd outside every scratch workspace.
A session that spawns from its own repo is a spawner. It keeps its sticky project and `repoCanonical`, has no `role`, and lists the repos it touched in `repos`.
It heads a run because its children name it as their parent.
From contract 1.7, a rollup's `role`, `repos` and `workspaceRef` are authoritative: an absent field means "not set", so a newer rollup retracts an older verdict.

## Why

DIRASH-0041 made every spawning session an orchestrator, and an orchestrator never gets a single repo.
In prod that stripped the repo from six worktree sessions that only called `start_session`, so the cloud could not group them under their repo.
Run heads were already derived from children on both sides, so the role was never needed to head a run.
The cloud merged the lineage fields as "latest non-null wins", and the wire says "not an orchestrator" by omission. A stored `orchestrator` therefore could never be cleared, not even by a correct later rollup.

## Rejected

- A new `SessionRole` value for a run head. The parent link already says it, and every ordinary rollup would have to carry the field.
- Keep spawners as orchestrators and fix the grouping in the cloud only. The repo is lost on the producer side and the cloud cannot recover it.
- Retract by sending an explicit "none" role. Omission already means none for every other optional field.

## Agent directives

- Never mark a session that resolved a repo from a cwd outside a scratch workspace as an orchestrator, whatever it spawned.
- Count a non-orchestrator run member's `project` and `repos` together in a run's repos, in `runs.rs` and in the cloud's `planRuns`.
- Never treat an absent `role`, `repos` or `workspaceRef` on a 1.7+ rollup as unknown.

## Verification

The `checks:` entries cover the lineage rule, the run repo union and the end-to-end live registry and rollup.
The cloud's authoritative merge for 1.7 envelopes is tested in dirahq-cloud's ingest tests, not here.

## Open questions

- A session in a non-repo, non-scratch cwd that names a repo only through tool evidence (`gh -R`) counts as having resolved a repo outside a scratch workspace. If it also spawns, it becomes a spawner pinned to that evidence repo. Nothing in prod shows this yet.
