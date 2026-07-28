---
id: D-0000
title: Short imperative summary of the decision
status: active
# Guard globs: `*` and `**` both cross directory boundaries (so `src/*.rs`
# also matches nested files); a trailing `/` prefix-matches the directory.
guards:
  - path/or/glob/of/code/this/decision/shapes/**
origin: recorded
verified: true
---

## Decision

One or two sentences: what was decided.

## Why

The constraint or reasoning a future reader could not reconstruct from the code.

## Rejected

- Alternative — why it lost.

## Agent directives

- Concrete do/don't for anyone (human or agent) editing the guarded paths.
