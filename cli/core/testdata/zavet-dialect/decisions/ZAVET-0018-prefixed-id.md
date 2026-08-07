---
id: ZAVET-0018
title: A record minted under a non-default prefix
status: active
guards:
  - cli/core/src/zavet.rs
checks:
  - prefix set round-trips :: run-the-dialect-suite
---

## Decision

An id carries a per-repo prefix. Everything downstream of the id — guards,
checks, status defaults, the meta row — behaves exactly as it does for a `D-`
record; only the namespace changes. The retired `D` prefix in this corpus's
`config` keeps every older fixture resolvable alongside this one.
