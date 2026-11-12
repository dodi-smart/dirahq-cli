---
id: DIRASH-0069
title: The check catalogue ships in the contract, the recipe still does not
status: active
guards:
  - contract/src/catalogue.rs
  - contract/check-catalogue.json
  - cli/core/src/loop_recipe/checks.rs
checks:
  - catalogue ids are unique and scoped :: cargo test -p dira-contract -- catalogue::tests
  - the emitted catalogue matches its source :: sh -c "just contract-schema >/dev/null 2>&1 && git diff --exit-code contract/check-catalogue.json"
origin: recorded
verified: true
---

## Decision

Loop checks (`L1`..`L15`) and proba's change checks (`host.*`, `dira.*`,
`zavet.*`) share one catalogue: id, scope (`loop` or `change`), default
severity (`off`, `advisory`, `required`), a title and a basis. Loop checks
add an impact grade and their typical improvement; change checks add their
element and rung. The fields stay flat, each present only for its scope,
because a tagged shape would nest the JSON keys the cloud vendors. The source is
`contract/src/catalogue.rs`. `just contract` emits
`contract/check-catalogue.json`, which is drift-gated and attached to every
release. The cloud vendors that file and pins proba's registry to it. Every
change bumps `CATALOGUE_VERSION`, and ids are never removed or reused.

## Why

`dira loop lint` must check a `proba:<id>` gate offline, and the CLI is open
source with no route to the private cloud. So the CLI has to carry the ids.
The contract crate already has what a shared vocabulary needs: one Rust
source, an emitted artifact, a CI drift gate, a release asset and a cloud
pull. A second mechanism for one more shared list would drift.

This does not revisit DIRASH-0068. That record keeps the recipe off the
contract because the recipe is local text. The catalogue is not the recipe.
It is a list of ids the cloud already uses in its own records, with no
content in it (D-0001).

The severity words are proba's (D-0041) for both scopes. A recipe override,
a gate and a proba policy row then all say the same thing. Loop severities
come from the §6.5 impact: critical and high are `required`, medium and low
are `advisory`.

## Rejected

- The cloud owns the catalogue and the CLI vendors it. That would make the
  open repo depend on a private one, and it reverses the existing direction.
- The CLI hard-codes only the ids it needs. That is a second, partial list
  with nothing to keep it in step with proba's registry.
- A separate repo or package for the catalogue. That adds a third release
  train for one list.

## Agent directives

- Add a change check here first, release, then add it to proba's registry.
  An older dira reports a newer id as an L4 warning, never a failure.
- Never remove, rename or reuse an id. Retire one by keeping it listed.
- Keep evidence logic out of this file: sources, qualifiers and thresholds
  stay in the cloud registry.
- Bump `CATALOGUE_VERSION` with every edit to `CATALOGUE`.

## Verification

The two checks above. Whether every edit bumps the version is not checked
mechanically; review it.
