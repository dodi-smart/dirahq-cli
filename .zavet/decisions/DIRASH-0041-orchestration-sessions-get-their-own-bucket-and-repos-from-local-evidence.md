---
id: DIRASH-0041
title: Orchestration sessions keep repo-less time in their own bucket and take repos only from local tool evidence
status: active
guards:
  - cli/core/src/orchestration.rs
  - cli/core/src/runs.rs
  - cli/sources/src/claude_code.rs
  - cli/dirad/src/writer.rs
  - cli/dira/src/session_titles.rs
checks:
  - tool arguments never survive normalization :: cargo test -p dira-sources tool_input_yields_evidence_but_never_survives_normalization
  - several repos in one command yield no evidence :: cargo test -p dira-core --lib orchestration::tests::several_distinct_repos_in_one_command_are_ambiguous
  - orchestrator attribution and links end to end :: cargo test -p dirad --test orchestration
  - repo-less orchestrator time is its own report line :: cargo test -p dira-core --lib report::tests::orchestrator_repo_less_time_is_its_own_line_not_unresolved
corrected-by: DIRASH-0042
origin: recorded
verified: true
---

## Decision

A session rooted in a Claude Desktop scratch workspace, or one that spawned another session, is an orchestrator.
Its events take a repo from the cwd first, then from the tool call's own input (`cd`, `git -C`, `gh -R`, file paths), and only when every piece names the same repo.
Its repo-less time is reported as an orchestration bucket, never as unresolved and never moved onto a repo.
A child links to its orchestrator by host session id (`local_<uuid>`), matched against the id the orchestrator's `start_session` call returned.

## Why

Orchestrators never `cd` in a way the hook cwd shows, so cwd-only resolution left them unresolved or pinned to the first repo they touched.
Tool input and output are content (D-0001). The daemon may read them, but only to derive a canonical repo ref or a child id, and it drops them right away.
Coordination time is real work that no single repo describes. Putting it on a repo would bill one client for another's fan-out.
Session titles can paraphrase a prompt, so they are content too.

## Rejected

- Split coordination time across the children's repos. That invents an attribution policy the accounting core does not have.
- Parse transcripts for `pr-link` records. The hooks already carry the evidence, and dira does not tail logs.
- Pick the first repo when a command names several. That is how work lands on the wrong client.

## Agent directives

- Never store or send a tool argument, tool output or session title. Store only the derived ref, digest or id.
- Never register a scratch workspace dir in `repo_dirs`. Register only the directory the repo resolved from.
- Never give an orchestrator a single `repo_canonical`. Use `repos`.
- Keep runs a display lens. Never change per-session or per-repo totals to build one.
- Keep `cli/core/src/runs.rs` and the cloud's `assembleRuns` on the same rules.

## Verification

The `checks:` entries cover arguments dropped at normalization, ambiguity giving no evidence, the end-to-end link and attribution, and the separate report line.
That titles never reach the wire is not mechanically checked. A reviewer must confirm `session_titles` is only read by the renderer.
