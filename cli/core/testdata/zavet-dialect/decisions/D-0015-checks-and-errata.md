---
id: D-0015
title: Checks bind an invariant to a command
status: active
checks:
  - pg suite forbids module mocks :: run-the-pg-suite
  - run-the-lint-suite
corrected-by: D-7
---

## Decision

A labeled check splits on the first `::`; an item with no separator IS the
command and doubles as its own label. `corrected-by` canonicalizes like every
other decision id, so `D-7` and `D-0007` are the same pointer.
