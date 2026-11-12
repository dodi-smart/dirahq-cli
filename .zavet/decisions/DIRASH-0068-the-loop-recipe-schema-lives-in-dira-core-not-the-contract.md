---
id: DIRASH-0068
title: The loop recipe schema lives in dira-core, not the contract
status: active
guards:
  - cli/core/src/loop_recipe/**
  - examples/loops/**
checks:
  - the contract crate holds no loop recipe type :: sh -c "! grep -rqiE 'loop_?recipe|LoopRecipe' contract/"
  - the recipe schema rejects unknown keys :: cargo test -p dira-core -- loop_recipe::tests::an_unknown_field_fails_with_a_line
origin: recorded
verified: true
---

## Decision

The loop recipe (`.dira/loop.yml`, v0) is defined as typed serde structs in
`dira-core` under `cli/core/src/loop_recipe/`. Every struct rejects unknown
keys, and those structs are the schema. `serde_norway` parses the YAML. The
recipe has no type, no JSON Schema and no fixture in `/contract`.

## Why

`/contract` is the attestation wire. Its types are signed, drift-gated in CI
and vendored by the cloud repo, so every change there costs a cross-repo
release order. The recipe is local configuration. `dira loop lint` reads it
from disk and nothing sends it anywhere, so none of that cost buys anything.

The recipe also holds text a team writes about its own process: gate rules,
tier conditions, override reasons. Keeping it off the wire keeps D-0001 easy
to hold. Content-free does not have to be argued field by field for a type
that never travels.

`serde_norway` is the maintained fork of `serde_yaml`, under MIT or Apache-2.0,
which the cargo-deny license gate allows. Typed structs give a schema failure
a line and column for free.

## Rejected

- A recipe type in `/contract` with a generated JSON Schema. It would put a
  local file under the wire's release discipline for no consumer.
- A hand-written JSON Schema plus a validator crate. It is two sources of
  truth for one shape, and an extra dependency.
- TOML, to reuse `toml_edit`. The vision spec and every CI tool a team already
  writes use YAML for loop descriptions.

## Agent directives

- Do not move recipe types into `/contract`. If the cloud ever needs to read a
  recipe, that is a new decision and a new versioned wire record, reviewed
  against D-0001. Do not reuse the local structs as the wire shape.
- Keep `deny_unknown_fields` on every recipe struct. A typo must fail with
  its line, never be ignored.
- Bump `version` for a breaking change. v0 accepts exactly `0`.

## Verification

The two checks above. Whether nothing sends the recipe off the machine is not
mechanically checked; review any new reader of `loop_recipe` for a network
path.
