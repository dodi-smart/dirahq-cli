# Loop recipe v0 and `dira loop lint` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Parse and validate `.dira/loop.yml` (recipe v0), run the offline §6.5 checks, and expose it as `dira loop lint [path]` with `dira doctor`'s 0/1/2 exit contract.

**Architecture:** `dira-core::loop_recipe` owns the schema (serde structs, `deny_unknown_fields`), reference validation and the L1–L15 checks, returning plain `Finding`s. The `dira` binary's `loop_cmd.rs` maps findings onto doctor's `Check`/`Level` and reuses `doctor::render` and `doctor::exit_code`.

**Tech Stack:** Rust 2021, serde, `serde_norway` (YAML), clap.

**Spec:** `.zavet/specs/loop-recipe.md`; decision `DIRASH-0068`.

## Global Constraints

- v0 accepts exactly `version: 0`.
- Every recipe struct has `#[serde(deny_unknown_fields)]`.
- Exit codes: 0 all clear, 1 any warning, 2 any failure (DIRASH-0022). Skip and overridden-to-info never raise it.
- Severity → level: critical/high → fail, medium/low → warn, info → ok.
- Nothing in `/contract` changes.
- Commits: `feat(cli): …`, DCO `-s`, `Refs: DIRASH-0068` trailer.

## Review Focus

- An empty file or a YAML list at the top level: must be a `recipe.parse` failure, not a panic. Test `an_empty_file_fails_parse`.
- A `path` that is a directory: resolves `<dir>/.dira/loop.yml`. Test `a_directory_resolves_the_default_file` (bin).
- A missing file: exit 2 with the path in the message. Test `a_missing_file_is_a_failure` (bin).
- Two tiers without `when`, or a default tier not last: `recipe.refs` failure. Test `tiers_need_one_trailing_default`.
- An override with an empty reason: `recipe.refs` failure. Test inside `dangling_references_fail_refs`.

---

### Task 1: Schema and parse (`cli/core/src/loop_recipe/{mod,schema}.rs`)

**Produces:** `pub fn parse(src: &str) -> Result<Recipe, ParseError>`; `ParseError { message: String, line: Option<usize>, column: Option<usize> }`; the schema types named in the spec (`Recipe`, `Lane`, `Stage`, `StageKind`, `By`, `Gate`, `Tier`, `TierAction`, `Holdout`, `Budgets`, `Autonomy`, `Rung`, `KillSwitch`, `Override`, `Severity`, `OneOrMany<T>`).

- [ ] Tests: `an_unknown_field_fails_with_a_line` (line is Some), `an_empty_file_fails_parse`, `a_version_other_than_0_is_refused` (via `lint`).
- [ ] Add `serde_norway` to workspace deps and `dira-core`.
- [ ] Implement, run `cargo test -p dira-core -- loop_recipe`.

### Task 2: References and checks (`loop_recipe/{refs,checks}.rs`)

**Produces:** `pub fn lint(src: &str) -> Vec<Finding>`; `Finding { id: &'static str, level: FindingLevel (Ok|Skip|Warn|Fail), summary: String, remedy: Option<String>, severity: Option<Severity>, overridden: Option<String> }`; `pub const CHECK_IDS: &[&str]` = `recipe.parse, recipe.version, recipe.refs, recipe.unused, L1..L15`.

- [ ] Tests (checks.rs): `a_same_family_required_verdict_fires_l1`, `an_override_keeps_the_finding_and_never_raises_the_exit`, `missing_inputs_skip_never_fail`, `production_write_reach_is_critical`, `a_dispatch_stage_without_wip_fires_l6`, `a_merge_tier_without_a_class_kill_switch_fires_l8`, `an_implement_stage_without_a_plan_fires_l5`, `a_gate_without_enforcer_fires_l4`.
- [ ] Tests (mod.rs): `dangling_references_fail_refs`, `tiers_need_one_trailing_default`, `the_dodi_factory_example_lints_clean` (reads `examples/loops/dodi-factory.yml` via `include_str!`; no finding is Warn or Fail; L1 present with `overridden`).

### Task 3: CLI (`cli/dira/src/loop_cmd.rs`, `main.rs`)

**Consumes:** `dira_core::loop_recipe::lint`, `doctor::{Check, Level, exit_code, render}`.

- [ ] `Command::Loop { command: LoopCommand::Lint { path: Option<PathBuf>, json: bool, verbose: bool } }`; dispatched before the daemon path like `Doctor`; `std::process::exit(code)`.
- [ ] Tests: `exit_codes_follow_doctor`, `a_directory_resolves_the_default_file`, `a_missing_file_is_a_failure`.

### Task 4: Example and docs

- [ ] `examples/loops/dodi-factory.yml` from dodi-smart/.github as of 1.48.0; `just ci`; PR into `develop`.
