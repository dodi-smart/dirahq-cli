---
id: D-0017
title: Every spec counts toward coverage; `verified` is review, not documentation
status: active
guards:
  - cli/dirad/src/knowledge_sync.rs
checks:
  - specs count regardless of verified :: cargo test -p dirad --lib knowledge_sync::tests
origin: recorded
verified: true
---

## Decision

`coverage_globs` returns the coverage set: active decisions' guard globs ∪
**every** spec's paths. Specs are not filtered on `verified`. Decisions are
still filtered on `status`.

## Why

`verified: true` records that a human reviewed whether a spec matches the code.
That is not the same question as whether the code is documented, and coverage
asks the second one.

In practice nobody flips it. Across the two repos using zavet daily, all seven
specs sit at `verified: false`, including one at `confidence: high`, so the old
gate meant specs contributed **exactly zero** to a number labelled coverage. It
read as "guards only" and silently understated the work.

It was also inconsistent. A decision counts whenever it is `active`, whatever
its `origin`/`verified`. An unverified reverse-engineered decision has always
counted. Holding specs to a stricter bar was an accident of implementation, not
a rule anyone stated; the arm arrived in `be07341` with no rationale.

`status` still filters, and that asymmetry is deliberate: supersession is a
statement about the code's current state (a superseded guard enforces
nothing), where `verified` is a statement about the reviewer's queue.

## Rejected

- Keep the gate and get people to flip `verified`: the flag is a human act by
  design (the plugin refuses to set it), so the metric would stay near zero for
  as long as review lags, which is always.
- Report verified and unverified coverage as two numbers: more honest in
  principle, but it needs a new wire field, a contract bump and a cloud change
  to render a distinction that nothing has yet asked for.
- Count only specs whose paths still exist: this conflates coverage with
  staleness, which specs already report separately (`stale_commits`).

## Agent directives

- Do NOT re-add a `verified` filter to the coverage set. It looks like a
  missing honesty check; it is not. Unverified knowledge is still knowledge,
  and `verified` is already shown per record for anyone who wants to weigh it.
- A new record type that joins the coverage set goes through `coverage_globs`.
  Keep the rule in one function so the decision stays greppable.
- `status`-based filtering is a different question from `verified`-based
  filtering. Do not unify them.

## Verification

`coverage_globs` is pure and pinned by three unit tests in
`cli/dirad/src/knowledge_sync.rs`: unverified and unstated specs both count,
an unverified spec contributes exactly what an unverified decision does, and
superseded decisions still drop out.
