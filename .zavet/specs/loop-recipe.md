---
title: Loop recipe v0 and dira loop lint
version: 3
origin: session
verified: false
confidence: medium
date: 2026-11-12
paths:
  - cli/core/src/loop_recipe/**
  - cli/dira/src/loop_cmd.rs
  - examples/loops/**
  - contract/src/catalogue.rs
  - contract/testdata/loop-lint-vector.json
decisions: [D-0001, DIRASH-0022, DIRASH-0068, DIRASH-0069, DIRASH-0070]
checks:
  - the DODI factory example lints clean :: cargo test -p dira-core -- loop_recipe::tests::the_dodi_factory_example_lints_clean
  - an unknown field is a schema failure with its line :: cargo test -p dira-core -- loop_recipe::tests::an_unknown_field_fails_with_a_line
  - a version other than 0 is refused :: cargo test -p dira-core -- loop_recipe::tests::a_version_other_than_0_is_refused
  - dangling references fail recipe.refs :: cargo test -p dira-core -- loop_recipe::tests::dangling_references_fail_refs
  - a same-family required verdict fires L1 :: cargo test -p dira-core -- loop_recipe::checks::tests::a_same_family_required_verdict_fires_l1
  - an override lowers severity and keeps the finding visible :: cargo test -p dira-core -- loop_recipe::checks::tests::an_override_keeps_the_finding_and_never_raises_the_exit
  - a check without inputs skips :: cargo test -p dira-core -- loop_recipe::checks::tests::missing_inputs_skip_never_fail
  - an agent lane with production write reach is critical :: cargo test -p dira-core -- loop_recipe::checks::tests::production_write_reach_is_critical
  - a proba-backed gate is observed and an unknown id is a finding :: cargo test -p dira-core -- loop_recipe::checks::tests::a_proba_backed_gate_is_observed_and_an_unknown_id_is_a_finding
  - a required gate makes its proba check required :: cargo test -p dira-core -- loop_recipe::policy::tests::a_required_gate_makes_its_check_required_and_the_stricter_gate_wins
  - the dial is stated or derived from the rung :: cargo test -p dira-core -- loop_recipe::policy::tests::the_dial_is_stated_or_derived_from_the_rung
  - only loop check ids and statuses cross :: cargo test -p dira-core --lib -- loop_recipe::wire::tests
  - the loop-lint conformance fixture is current :: cargo test -p dira-core --test loop_lint_vector
  - exit codes follow doctor :: cargo test -p dira --bin dira -- loop_cmd::tests::exit_codes_follow_doctor
---

## Overview

Letopis Phase 1 ("the loop, observed") starts with a way to write a team's
delivery loop down as data and check it. The vision spec
(dirahq-cloud `docs/superpowers/specs/2026-11-11-letopis-autonomy-cockpit-vision.md`,
§6) calls this the **loop recipe**: one file, `.dira/loop.yml`, that names
the stages work moves through, who or what holds each stage, the gates
before a stage or a merge, the merge tiers, the holdouts, the round caps and
the kill switches. `dira loop lint [path]` parses it, validates it, and runs
the §6.5 review checks that can be decided from the recipe alone.

v0 is deliberately small. It holds what `dodi-smart/factory@1` (DODI's
GitHub factory, `examples/loops/dodi-factory.yml`) needs to be described
truthfully, plus the fields the offline §6.5 checks read. Nothing else: no
dispatch, no mandates, no adapters that act. The recipe describes a loop
that exists; it does not run one.

The recipe is local configuration, read from disk and never sent anywhere.
It lives in `dira-core` (`cli/core/src/loop_recipe/`), not in `/contract`:
see DIRASH-0068.

The lint result does leave the machine, as metadata only (DIRASH-0070). When
knowledge sync is on, the daemon lints each repo's `.dira/loop.yml` and sends
a `loopLints[]` entry on the knowledge channel: the recipe path, its git blob
oid, the catalogue version, and each loop check's id and status (`pass`,
`warn`, `fail`, `skip`), a finding's severity after overrides, and the
decision id an override cites (`overriddenBy`). Summaries, remedies, override reasons and the
`recipe.*` results stay local. `loop_recipe::wire` builds the entry.
`contract/testdata/loop-lint-vector.json` pins the `L1`..`L10` statuses for a
set of recipes, so the cloud's TypeScript port of those checks can test
against the Rust rules. `just loop-vector` regenerates it.

**Loop checks and proba checks.** The two judge different things. A loop
check judges the loop's design, offline, from the recipe. A proba check judges
one merged change, from evidence, and proba seals the result into a hallmark.
The recipe says what the loop promises; proba records whether each change kept
the promise. Both kinds of id live in one check catalogue that ships in
`/contract` (DIRASH-0069).

`dira loop lint` is the open, offline entry point to the loop checks. proba
runs the same ids in the cloud as a **loop assay**, with the scope's observed
history, and keeps the result as its own record. A loop assay has findings
and a readiness record, and no grade. It never moves a hallmark's fineness
rung, because loops differ between teams and a rung must mean the same thing
everywhere. That layer, the recipe's proba policy and the `loop.*` hallmark
annotations are the cloud spec `loop-assay` (dodi-smart/dirahq-cloud).

## The file

YAML, one document. Every mapping rejects unknown keys, so a typo is a
schema failure with its line, never a silently ignored field.

| Key | Required | Meaning |
|---|---|---|
| `version` | yes | Recipe schema version. v0 accepts exactly `0`. |
| `recipe` | yes | `owner/name@N`, e.g. `dodi-smart/factory@1`. A discovered loop carries its owner's name. |
| `tracker.adapter` | yes | `github`, `gitlab`, `jira`, `linear` or `azure-boards`. Declarative in v0. |
| `state` | yes | Map of state key (`stage`, `risk`, `effort`, `mode`, `priority`, …) to its source: `field:<name>`, `label:<prefix>`, `column:<name>` or `status:<context>`. A list of sources is allowed only so L10 can report it. |
| `lanes` | yes | Map of lane name to lane (below). |
| `gates` | no | Map of gate name to `{ rule, enforced_by, required }`. |
| `stages` | yes | Ordered list of stages (below). |
| `holdout` | no | `{ paths, frozen, hidden, enforced_by }`. |
| `budgets` | no | `{ per_item_usd, per_month_usd, on_limit }`. |
| `autonomy` | no | `{ rung: A0..A4, scope, dial }`. The rung belongs to a scope (§5). `dial` is proba's enforcement dial (`observe`, `signal`, `enforce`); absent, it follows the rung: A0 and A1 observe, A2 signals, A3 and A4 enforce. |
| `kill_switches` | no | List of `{ name, kind: variable\|label, scope: org\|repo\|item, stops, off_when }`. `stops` lists stage ids or `all`. |
| `review.overrides` | no | List of `{ check: L1..L15, severity: off\|advisory\|required, reason, record }`. |

**Lane**: `runtime` (free text: `claude-code-action`, `grok-bot`, …), `pool`
(`seat`, `coworker`, `metered`), `family` (the model family: `anthropic`,
`openai`, `xai`, `google`, …), optional `model` (`{ default, rules: [{ when,
use }] }`, informational), `max_parallel`, `reach` (list of `repo`, `org`,
`production-read`, `production-write`, `deploy`), `pricing` (`table`,
`reported`, `none`).

**Stage**: `id` (the loop's own name), `kind` (one of `intake`, `triage`,
`plan`, `implement`, `fix`, `verify`, `review`, `judge`, `merge`, `release`,
`dispatch`, `other`, the canonical vocabulary of §6.1, so a recipe can rename,
skip or add), `by` (`lane:<name>`, `person`, `host` or `script`), `request`
(what starts it, e.g. `label:agent:implement`), `needs` (earlier stage ids
the item must have passed), `gates` (gate names), `done_when`, `rounds`
(`{ max, on_limit }`), `verdict` (`{ status, required }`, a machine-readable
review result on a head commit), `limits` (`{ wip, review_wait_ceiling_hours }`),
`tiers` (merge stages only: ordered `{ name, when, action: merge|ready|draft }`;
exactly one tier has no `when`, and it is last).

**Gates as code.** A gate is a deterministic predicate. `rule` states it in
the loop's terms. `enforced_by` is one entry or a list, and each entry is one
of two kinds:

- `proba:<check id>` names a change check from the catalogue. proba observes
  it on every merge and seals the answer, so the gate is **observed**.
- Anything else (a workflow or action path, a required check name) is
  **declared**: the recipe says this code enforces the rule, and nothing
  checks that it does.

A gate can name both, the code that blocks and the proba check that watches:
`enforced_by: [actions/merge-gate, "proba:host.ci"]`. A gate with no
`enforced_by` is a rule that lives only in a prompt or a person's head. L4
reports that, and reports a `proba:` id that is not in this dira's catalogue
or that names a loop check. Rules are text in v0: the linter does not
evaluate them.

`required` (default `true`) is the gate's weight. It feeds proba policy:
a required gate sets each proba check it names to `required` for the repo,
and `required: false` sets them to `advisory`. When two gates name one check,
the stricter wins. `loop_recipe::policy::proba_policy` computes this
projection (check ids, severities, rung and dial, and no rule text). This PR
computes it locally only. Sending it to the cloud is a separate decision
(DIRASH-0068).

## Lint

`dira loop lint [path]`: `path` is a recipe file or a directory holding
`.dira/loop.yml`; it defaults to `.dira/loop.yml` in the working directory.
Flags: `--json` (one JSON object, the `dira doctor` report shape, with
`command: "dira loop lint"`) and `--verbose` (also show skipped checks). It
reads no config, daemon or store, so a broken `config.toml` does not stop it.

It reports, it never edits, and its exit codes are `dira doctor`'s contract
(DIRASH-0022): 0 all clear, 1 any warning, 2 any failure. A file that cannot
be read or parsed is a failure (2).

Results, in order:

1. `recipe.parse`: the file reads and matches the schema (unknown keys, wrong
   types, bad enum values fail with their line and column). If it fails,
   nothing else runs, unless the file declares another version: then
   `recipe.parse` skips and `recipe.version` names it, so a newer recipe's
   new keys never hide the real cause.
2. `recipe.version`: `version: 0`.
3. `recipe.refs`: every `lane:` and `needs` and gate and kill-switch target
   exists; `needs` points backward; stage ids and kill-switch names are unique; tiers are on a merge
   stage with exactly one trailing default; overrides name a real check, once,
   with a reason. Dangling references fail.
4. `recipe.unused`: lanes or gates nothing references. A warning.
5. The catalogue's loop checks, `L1`..`L15`, in catalogue order. Each finding
   takes its title, default severity and remedy (the typical improvement)
   from the catalogue; this module holds only the rules.

### Severity to level

A finding carries the catalogue's default severity, in proba's words
(DIRASH-0069). `required` is `fail` and `advisory` is `warn`. The catalogue
derives a loop check's default from its §6.5 impact: critical and high are
`required`, medium and low are `advisory`. `review.overrides` can change a
check's severity. Set to `off`, the finding is still printed, at `ok`, with
the override's reason and record, so an acknowledged gap never disappears and
never moves the exit code. In `--json` the override is structured
(`from`, `to`, `reason`, `record`). An override with no reason, an unknown
check, or a `record` that is not a decision id (`D-0042`, `DODI-00065`, parsed
by zavet's own id reader) is a `recipe.refs` failure.

### Which §6.5 checks run offline in v0

A check whose inputs are not in the recipe reports `skip`, never `fail`.

| # | v0 offline rule | Skips when |
|---|---|---|
| L1 | Some stage with a `required` verdict, and no required verdict comes from a lane whose `family` differs from every authoring lane (stages of kind `implement` or `fix`). | no authoring lane, no required verdict, or a family is missing |
| L2 | Agent lanes author code, and there is no `holdout` that is `frozen` and has `enforced_by`. | no agent authoring stage |
| L3 | An agent lane's `reach` includes `production-write` or `deploy` (required). `org` reach is an advisory finding under the same check. `production-read` is fine: it is the observe lane. | no agent lane declares `reach` |
| L4 | A gate has no `enforced_by`; a `proba:` id is not in the catalogue ("update dira" when the cloud is newer) or names a loop check. A pass counts gates observed by proba and gates only declared. | no gates |
| L5 | An agent `implement` stage does not `need` a `plan` or `triage` stage. | no agent implement stage |
| L6 | A `dispatch` stage has no `limits.wip`, or no `limits.review_wait_ceiling_hours`. | no dispatch stage |
| L7 | A merge tier with action `merge` while `autonomy.rung` is below A3, or a `dispatch` stage below A3. Outcome evidence is not checked offline; the summary says so. | no `autonomy.rung` |
| L8 | A `merge`-action tier or a `dispatch` stage has no `org` or `repo` scoped kill switch stopping it; a stage with `rounds` has no `max`; no kill switch at all. | never |
| L9 | An agent lane's `pricing` is `none`. | no agent lane declares `pricing` |
| L10 | A state key has more than one source. | never |
| L11–L15 | Need observed data. The cloud answers them from proba's hallmark history and tracker data, under the same ids. | always offline |

Impact grades, from the catalogue: L3 critical; L1, L2, L6, L7, L8, L15
high; L4, L5, L9, L11, L12, L14 medium; L10, L13 low.

## Non-goals

- Running, dispatching or gating anything. `dira loop` gains no verbs but
  `lint` in v0.
- Evaluating rule expressions, or fetching anything from a tracker or host.
- Sending the recipe to the cloud. If a later phase needs the cloud to read a
  recipe, that is a new decision and a new wire record (DIRASH-0068). Lint
  results are not the recipe: their ids and statuses cross (DIRASH-0070).
- Budgets enforcement or a readiness record. `budgets` is read only so L8 can
  credit it.

## Behavior

- `examples/loops/dodi-factory.yml` describes DODI's factory as it stands in
  dodi-smart/.github: triage, implement (holdout first, draft PR), pr-iterate
  (3 fix rounds), pr-review (`review / verdict`, required), judge (another
  family, advisory), dispatch (WIP cap 2, review-wait ceiling 24 h, off unless
  `FACTORY_DISPATCH`), and the merge tiers from `factory.sh tier`. It lints
  with exit 0. Its known L1 finding (the required reviewer shares the author's
  family; the other-family judge is advisory) is overridden to `off` with
  the reason and is printed. Its `checks-green` gate is enforced by
  merge-gate and observed by `proba:host.ci`; the other gates are declared.
