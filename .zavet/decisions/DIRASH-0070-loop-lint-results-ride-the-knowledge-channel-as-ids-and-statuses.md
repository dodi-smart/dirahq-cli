---
id: DIRASH-0070
title: Loop lint results ride the knowledge channel as ids and statuses
status: active
guards:
  - cli/core/src/loop_recipe/wire.rs
  - cli/core/src/bin/loop_lint_vector.rs
  - contract/testdata/loop-lint-vector.json
checks:
  - only loop check ids and statuses cross :: cargo test -p dira-core --lib -- loop_recipe::wire::tests
  - the conformance fixture is current and exercises every offline check :: cargo test -p dira-core --test loop_lint_vector
origin: recorded
verified: true
---

## Decision

`dira loop lint` results may leave the machine, as metadata only. The daemon
sends one `loopLints[]` entry per repo that holds `.dira/loop.yml`, on the
knowledge channel (contract 2.2.0). An entry holds the repo, the recipe path,
the recipe's git blob oid, the catalogue version, and one result per loop
check: its id (`L1`..`L15`) and its status (`pass`, `warn`, `fail`, `skip`).
A `warn` or `fail` also carries its severity after overrides (`required`,
`advisory`). When a recipe override changed a check, the result carries the
decision id the override cites as `overriddenBy`, an `off` that reads `pass`
included. Summaries, remedies, override reasons, `recipe.*` results and the
recipe text stay local. A lint that stops early sends empty `results`.

`contract/testdata/loop-lint-vector.json` pins the `L1`..`L10` statuses
(`{id, status}`) for a set of recipes, so the cloud's TypeScript port of those
checks tests against the Rust rules. `just loop-vector` generates it, and CI
drift-gates it.

## Why

proba's loop assay (cloud D-0098) needs the lint result, and only the device
sees every repo's working tree. A summary can quote recipe text, so only ids
and enums cross (D-0001). A severity is an enum and a decision id is an id.
The cloud needs them to show which decision turned a check off. The cloud has
titles and remedies from the catalogue. The git blob oid names the recipe
without sending it, and equals the `sha` GitHub reports for the same file.

This amends DIRASH-0068 and cloud spec `loop-assay` §1 on the lint result.
The recipe itself still never leaves the machine.

## Rejected

- Sending summaries, override reasons or the proba policy projection.
- A new endpoint. The knowledge channel already signs, gates and dedups.

## Agent directives

- Never add a text field to `KnowledgeLoopLint` or `LoopLintResult`. An
  override's `reason` never crosses; only its `record` id does.
- Change a check rule, then run `just contract` and commit the new fixture.
- Keep the fixture's recipes synthetic: it is a release asset.

## Verification

The two checks above, plus the contract's no-content tests over `loopLints[]`.
