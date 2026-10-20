---
id: DIRASH-0046
title: A scheduled-task slug may cross the wire as triggerName, only when its directory exists
status: active
guards:
  - cli/core/src/tokens.rs
  - cli/dirad/src/writer.rs
  - cli/core/src/orchestration.rs
  - contract/src/lib.rs
checks:
  - an interactive transcript has no trigger :: cargo test -p dira-core --lib tokens::tests::an_interactive_transcript_has_no_trigger
  - a slug that is not exactly a slug is rejected :: cargo test -p dira-core --lib tokens::tests::a_slug_that_is_not_exactly_a_slug_is_rejected
  - a slug survives only when its directory exists :: cargo test -p dira-core --lib tokens::tests::a_slug_survives_only_when_its_directory_exists
  - prompt text never surfaces in the parsed trigger :: cargo test -p dira-core --lib tokens::tests::the_prompt_text_never_surfaces_in_the_parsed_trigger
  - the writer stamps the verified slug and drops the prompt :: cargo test -p dirad --lib writer::tests::a_scheduled_transcript_stamps_the_trigger_and_the_verified_slug
  - a missing task dir keeps only the trigger :: cargo test -p dirad --lib writer::tests::a_scheduled_transcript_whose_task_dir_is_missing_keeps_only_the_trigger
  - sidecars and interactive transcripts set nothing :: cargo test -p dirad --lib writer::tests::an_interactive_transcript_and_a_sidecar_never_set_a_trigger
  - a trigger rolls up first-non-null and is never retracted :: cargo test -p dira-core --lib sync::batch::tests::a_trigger_found_on_any_event_rolls_up_and_is_never_retracted
  - a newly seen trigger ships one partial :: cargo test -p dirad --lib state::tests::a_newly_seen_trigger_is_offered_as_a_partial_once
  - the wire carries no content tokens :: cargo test -p dira-contract wire_contract_carries_no_content_fields
origin: recorded
verified: true
---

## Decision

The slug of a Claude Desktop scheduled task may cross the attestation wire as `SessionRollup.triggerName`, next to `trigger = "scheduled"` (contract 1.8).
This is an exception to D-0001 scoped to that one verified identifier, in the same class as a branch name or a repo name, which already cross.

- Capture reads the transcript's `user` records in the pass that already reads it for tokens. `turnOrigin == "scheduled"` or `origin.subkind == "scheduled-trigger"` marks the session `scheduled`. Those are metadata fields.
- The slug is matched in that one record's message text as `scheduled-tasks/<slug>`, with `<slug>` exactly `[a-z0-9][a-z0-9_-]{0,63}`. The text is read only to find the slug, then dropped, like tool input in DIRASH-0041.
- The slug is kept only if `~/.claude/scheduled-tasks/<slug>/` is an existing directory. Otherwise the session is still `scheduled`, with no name.
- A trigger is stamped on the event the transcript pass was prompted by, rolls up first-non-null, and is never retracted.

## Why

Scheduled runs reached the cloud as anonymous sessions: 38 in 10 days from one hourly task, with no repo and no label. Nobody could tell what they were. The user approved sending the name (#201).
The existence check is what makes a free-text prompt unable to become a value: only a name the user gave a task directory on their own disk can pass.

## Rejected

- **An opaque hash of the slug.** It groups runs but cannot name them, which is the complaint.
- **Matching the cwd against a task.** The task store records no cwd, and one scratch dir hosts several tasks.
- **A cloud-side heuristic** (same workspace, regular cadence, no repo). It cannot name a task and misfires on other repetitive work.

## Agent directives

- Never widen the slug pattern or accept a lookalike by trimming. A run that is not exactly a slug yields nothing.
- Always require the task directory to exist before keeping a slug. A candidate from the transcript is never a value.
- Never send, store or log the prompt, the task's `SKILL.md`, its description or its schedule.
- Read the trigger only from the session's own transcript, never a subagent sidecar.
- A new trigger kind needs its own evidence and its own decision. Do not add one to the enum on a hunch.

## Verification

The `checks:` entries cover the parser, the slug pattern, the directory check, the writer path and the rollup. That `triggerName` is the only transcript-derived string on the wire is not mechanically checked; a reviewer must confirm it.
Sessions already past their transcript offset when the daemon upgrades are not re-scanned, so they stay anonymous.

## Open questions

- `/loop` runs have no known marker locally, so there is no `loop` trigger yet.
