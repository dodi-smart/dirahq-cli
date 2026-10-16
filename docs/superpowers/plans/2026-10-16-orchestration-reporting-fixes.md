# Orchestration reporting fixes: plan

Date: 2026-10-16. Status: proposed, awaiting approval. Nothing is implemented.

Scope: why Dira reports orchestration sessions wrongly, in the CLI (this repo) and in
dirahq-cloud. The live "Right now" card's grouping UI belongs to the cloud-live-runs
session. This plan covers data, classification and sync only.

Evidence comes from the local store (read-only) and the prod cloud DB (Neon
`sparkling-mouse-90926813`, read-only). Installed dira is 0.9.0-develop.3. The repo is at
develop.4.

## Root causes, verified in code

| # | Symptom | Cause |
|---|---|---|
| 1 | Repo-rooted sessions that spawn a child lose their repo | `SessionLineage::observe` sets `orchestrator = spawned \|\| (scratch && !repo_outside_scratch)` (`cli/core/src/orchestration.rs:136`). `repo_canonical()` then returns `None` for it. The live registry clears its sticky project the same way (`cli/dirad/src/state.rs:604`). |
| 2a | A wrong verdict can never be corrected in the cloud | The cloud upsert merges `role`, `repos` and `workspaceRef` as "latest non-null wins" (`cloud/src/lib/ingest/unpack.ts:854-857`). The wire says "not an orchestrator" by omitting `role`, so once a row says `orchestrator` no later rollup can clear it. `workspace_id` is set on insert only, so a first rollup with no repo pins the session to the device's home workspace. |
| 2b | Sessions closed under the old daemon keep the old verdict | An ended rollup is built only when its `SessionEnd` falls in the sync window (`build_sessions`, `cli/core/src/sync/batch.rs:701`), so it ships once and never again. |
| 3 | Young live sessions have no `sessions` row | `partial_rollup_candidates` only offers sessions older than `partial_rollup_after` (default 3600s, `cli/dirad/src/state.rs:794`). |
| 4 | A resumed session keeps a stale end | The cloud merge never reopens: an incoming null `endedAt` keeps the stored end (`unpack.ts:822`). 2c6cba49 ended at 21:43:24Z and started again at 21:45:11Z. |
| 5 | Hourly scratch sessions read as orchestrators | They are scratch-rooted, so the rule marks them, even with no children and no repos. |
| 6 | `dira sessions` run rows lose their children | `runs::group` sees only the sessions in the live window. A run root whose children fell out keeps only its `direct` repos. |
| 7 | One host id maps to several session ids | The extra ids for `local_684b6117` (74ba7448, and the one whose id ends in 06bdcc134572) are single `session_end` events with no parent link. No wrong binding was observed. |

## Fixes, ordered by impact

Each fix is one issue. "Contract" says whether `/contract` changes (the contract-sync skill
applies when it does).

### A. A session that spawns from a repo keeps its repo (CLI + cloud mirror). Highest.

- **CLI.** Split the verdict. A *scratch-rooted orchestrator* is `scratch &&
  !repo_outside_scratch`. A *repo-rooted spawner* is `spawned && !orchestrator`. The
  spawner keeps its sticky project and `repoCanonical`, carries `repos` (every repo it
  touched), and has no `role`. It heads a run by being some child's parent. `runs::group`
  already supports that (`runs.rs:79-83`).
- `runs::build_run`: a non-orchestrator member contributes `project` plus its `repos`, so
  a spawner's own extra repos count.
- Live registry (`state.rs:604`) and `report.rs` follow the lineage, so they need no change
  beyond the rule. The spawner's repo-less time (rare, since its cwd resolves) stays
  unresolved, not orchestration.
- **Decision.** Record a new DIRASH via `/zavet:decide` that supersedes DIRASH-0041's
  "or one that spawned another session" clause and its directive "never give an
  orchestrator a single `repo_canonical`" (the directive stays true, since a spawner is no
  longer an orchestrator). Update `.zavet/specs/orchestration-sessions.md`.
- **Contract.** No new `SessionRole` value. "Heads a run" is derived from children on both
  sides. The doc text on `role` and `repos` changes ("`repos` is also set for a session that
  spawned others"), and that regenerates the schema. This ships in contract **1.7.0**
  together with fix B's semantics.
- **Cloud.** `planRuns` (`cloud/src/lib/data/runs.ts`) mirrors the `build_run` change.
  Nothing else in the cloud branches on "spawned".
- Tests: replace `spawning_a_child_always_marks_an_orchestrator` with a spawner test, and
  add a dirad e2e test that a worktree session which calls `start_session` keeps its repo.

### B. Lineage verdicts are retractable in the cloud (cloud + contract). Prerequisite for C and D.

- **Contract 1.7.0.** From 1.7, a rollup's `role`, `repos` and `workspaceRef` are
  authoritative: an absent field means "not set", not "unknown". `parentSession` stays
  latest-non-null, since a link is never retracted.
- **Cloud.** For envelopes at 1.7 or later, take incoming `role`/`repos`/`workspaceRef` as
  they are (including null) when the incoming `agentWallSeconds` is at least the stored
  value. That guard keeps an older, out-of-order partial from overwriting a newer one.
  Pre-1.7 envelopes keep today's merge.
- **Cloud.** When `repo_id` first resolves on an existing row, also re-route `workspace_id`
  to that repo's workspace.
- Without B, fix D makes things worse: a child's scratch preamble would ship an
  `orchestrator` partial early, and the row would keep it for good.

### C. Re-roll stale verdicts (CLI). Depends on A and B in prod.

- The cloud cannot recompute the verdict, because it never receives scratch-event counts.
  The re-roll has to come from the daemon.
- On first start of a build with the new lineage rules, the daemon re-sends session
  rollups, ended ones included, for every session with events in the last 30 days. It is
  gated by a `lineage_rules_version` marker in `meta` and runs once. It reuses the paced
  drain (D-0020).
- It must wait until contract 1.7 is on prod (main). Until then the cloud answers a 1.7
  envelope with `signature_rejected`.
- Fixes 2d556502 and the six repo-rooted spawners in finding 1.

### D. Ship a lineage partial on first sight and on lineage change (CLI). Depends on B.

- `partial_rollup_candidates` also offers any non-ended session with activity whose
  lineage fingerprint changed since its last partial: first sight, spawned, parent linked,
  first repo, verdict flip. The age threshold still governs the growth-only re-sends.
- Track `last_partial_lineage` beside `last_partial_active_seconds`.
- Gives b4d83885, 029aa72e, ff3e11a1, 7c0c6999 and 62016e23 a row with role, parent and
  repos within one sync.

### E. A resumed session reopens in the cloud (cloud).

- A partial (`endedAt` null) reopens the row (`ended_at = NULL`) when its envelope was
  signed after the stored `ended_at`. A replayed old envelope carries an old signing time,
  so it cannot reopen a row.
- The daemon's end was not premature. Claude Code really ended the session, and `--resume`
  reused the id two minutes later. No CLI change.

### F. A scratch session with no children reads as a scratch session (CLI + cloud UI). Low.

- Display-only: an orchestrator with no children and no repos renders as "scratch session",
  not "orchestration". No contract change, no accounting change.
- The cloud's label belongs to the cloud-live-runs session. The CLI's goes in `render.rs`
  and `format.rs`.

### G. `dira sessions` keeps a run's ended children (CLI). Low.

- When the list includes a run root, the daemon also returns that root's children from the
  store (by `parent_session`) for the same window, even when they have ended.

### H. Lone `SessionEnd` ids never join a run (CLI). Low.

- Add tests: a host id that maps to several session ids links each child to the session
  that spawned it, and a one-event `session_end` session becomes neither parent nor child,
  and ships no rollup.

## Order of work

1. B (cloud + contract 1.7) and A (CLI + cloud `planRuns`) together. The contract bump
   lands in the CLI first, then the cloud re-vendors it.
2. D and E.
3. C, after develop is promoted to main on both repos.
4. F, G, H.

## Coordination

- cloud-live-runs: D gives the "Right now" card a row for young sessions, and A makes
  repo-rooted spawners carry `repoCanonical`. Its grouping should key on "has children" and
  not on `role` alone.
- Tests that exec the real daemon use isolated `DIRA_SOCKET_PATH`, `DIRA_DB_PATH` and
  `DIRA_HTTP_PORT` (D-0021).

## Issues

| Fix | Issue | Type |
|---|---|---|
| A (CLI) | https://github.com/dodi-smart/dirahq-cli/issues/178 | Bug |
| A (cloud mirror) | https://github.com/dodi-smart/dirahq-cloud/issues/252 | Bug |
| B (cloud + contract 1.7) | https://github.com/dodi-smart/dirahq-cloud/issues/251 | Bug |
| C (CLI re-roll) | https://github.com/dodi-smart/dirahq-cli/issues/180 | Task |
| D (CLI early partials) | https://github.com/dodi-smart/dirahq-cli/issues/179 | Improvement |
| E (cloud reopen) | https://github.com/dodi-smart/dirahq-cloud/issues/253 | Bug |
| F (scratch label) | https://github.com/dodi-smart/dirahq-cli/issues/181 | Improvement |
| G (`dira sessions` children) | https://github.com/dodi-smart/dirahq-cli/issues/182 | Bug |
| H (host id tests) | https://github.com/dodi-smart/dirahq-cli/issues/183 | Task |
