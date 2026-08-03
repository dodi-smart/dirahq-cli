---
id: D-0018
title: The offline timeline is a port of the cloud's grouping, pinned by a shared fixture
status: active
guards:
  - cli/core/src/timeline.rs
  - contract/testdata/session-grouping-vector.json
origin: session
verified: false
---

## Decision

`cli/core/src/timeline.rs` is a **deliberate port** of the cloud's
`assembleSessionGroups` + `pageFromFacts`, not an independent implementation of
"group sessions sensibly". The grouping key, the cluster split, the head-selection
rule, the group sort and the page-boundary filter all mirror
`cloud/src/lib/data/sessions.ts`, constants included.

`contract/testdata/session-grouping-vector.json` pins the agreement. Both
languages assert the same fixture — Rust in `timeline::tests`, TypeScript in the
cloud's `session-grouping-vector.test.ts` (vendored by `just contract-pull`).

## Why

The desktop app reads the daemon; the dashboard reads the cloud. Both show "your
week". If the two cluster sessions differently, the same week reports different
totals in the two surfaces — and nothing tells the user which one is wrong. Time
is what this product bills on, so "roughly the same" is not a tolerable answer.

The rules are not self-evident from either implementation, which is what makes an
independent re-derivation almost certain to drift:

- the cluster gap is measured between **consecutive members**, not from the
  cluster head, so a chain of 3h-apart sessions is one unit however long it runs;
- the comparison is **inclusive** (`<=`), so a gap of exactly 4h does not split;
- a unit's position on the timeline is its **newest** member's start, not its
  oldest;
- the fetch window is padded on **both** ends by `SESSION_LOOKBACK`, and a unit is
  emitted only when `floor <= head < ceiling`. Pad one end only and a straddling
  unit is assembled twice, each time from half its sessions. The cloud's D-0019
  records the empirical proof: a 2-session unit summed to 150% of its real
  engaged time under the one-sided version.

Each of those is one character or one clause away from a plausible wrong answer,
and none of them fails loudly — they produce numbers that look fine.

## Rejected

- **Re-derive the grouping in Rust from the description.** This is the default
  and it is the trap: the description reads unambiguous and is not. A perturbation
  check confirmed the fixture's value — flipping the gap comparison from `<=` to
  `<` fails three tests with clear messages, and would otherwise have shipped as a
  silent off-by-one on every exactly-4h boundary.
- **Have the daemon call the cloud for grouping.** Defeats the entire point:
  offline is the free tier, and it must work with no network and no account.
- **Keep the fixture next to the Rust tests.** Only the Rust side would read it.
  `contract/testdata/` is the one directory already vendored into the cloud
  (`just contract-pull`), so it is the only location where both sides can assert
  the same bytes without inventing a second sync channel.

## Agent directives

- Never change the grouping rules on one side alone. A change means: both
  implementations **and** the fixture, in the same change set.
- The fixture carries only metadata that already rides the attestation wire
  (`repoCanonical`, `branch`, `identityEmail`, `prompts`, timestamps). Never add
  content-bearing fields to it — D-0001 governs `contract/**`.
- Session summaries are rebuilt from the **event log**, never from the daemon's
  in-memory session registry: the registry holds only live and recent sessions, so
  reading it would silently truncate the timeline to the current daemon run.
- `Response::Timeline.cursor` is the only stop signal a walker may trust. Do not
  make a client stop on an empty `units` — a quiet week is still a page, and
  stopping there truncates history at the first gap.
