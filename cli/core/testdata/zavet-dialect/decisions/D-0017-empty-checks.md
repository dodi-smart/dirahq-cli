---
id: D-0017
title: An empty checks list is not a check
status: active
checks: []
corrected-by: D-9999
---

## Decision

`checks: []` yields no rows, mirroring `guards: []`. A `corrected-by` pointing
at a record that does not exist still parses — danglingness is a store-side
finding, not a parse error, so a typo'd pointer never costs the record.
