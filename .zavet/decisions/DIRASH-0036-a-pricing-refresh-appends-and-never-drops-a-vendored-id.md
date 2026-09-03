---
id: DIRASH-0036
title: A pricing refresh appends and never drops a vendored id
status: active
guards:
  - cli/core/src/bin/pricing_sync.rs
  - cli/core/pricing/
checks:
  - the table still carries one id per harness family :: cargo test -p dira-core --lib -- pricing tokens
origin: session
verified: true
---

## Decision

`pricing_sync` takes the table it is replacing as an optional positional
argument, and with it the refresh is append-only: a key models.dev still
publishes gets the fresh price, a key models.dev has stopped publishing keeps
its last-known price. The only supported way for an id to leave
`cli/core/pricing/models.json` is an explicit `null` in `overrides.json`.
Both the workflow and `just pricing-sync` pass the argument; omitting it is a
clean regenerate, kept for a from-scratch run.

## Why

models.dev prunes ids as vendors retire them, and a regenerate-from-scratch
sync turns that into silent data loss. The cloud's counterpart re-prices
historical `token_usage` rows against its copy of this table; once a key is
gone the resolve cascade has nothing to fall back to — there is no
`gemini-3` family key under `gemini-3-pro-preview` — so those rows become
permanently unpriceable. The refresh of 2026-09 would have dropped four
Gemini ids this way, and it is what broke the cloud's canary test.

Retention also makes the canaries honest. Pinning literal ids in a test is
only reasonable when a re-sync cannot empty them out; before this, a rotting
canary looked like an upstream rename and the tempting fix was to loosen the
assertion, which is how the test stopped guarding the models dira actually
observes.

The cost is that a genuinely wrong price can no longer be corrected by
waiting for upstream to fix it — the entry persists until someone suppresses
it. That is the right trade: `overrides.json` already exists for exactly this,
and a wrong price is a visible, fixable estimate, while a missing one is
unrecoverable history.

## Rejected

- **Let ids drop and re-point the canaries each time** — the simplest diff,
  but it accepts the cloud's history loss and guarantees the canaries rot
  again on the next vendor rename.
- **Hand-copy dropped ids into `overrides.json`** — uses the documented
  escape hatch, but it is manual work on every upstream prune and nothing
  detects a prune that nobody noticed.
- **Fall back to a clean regenerate when the path argument is missing or
  unreadable** — a typo'd path would drop every retained key with no
  warning, which is the exact failure this decision exists to prevent. A bad
  path is a hard error.

## Agent directives

- Never remove an entry from `cli/core/pricing/models.json` by hand, and
  never "clean up" ids the catalog no longer carries. Suppress with a `null`
  in `overrides.json` instead, and say why.
- The sanity gates in `pricing_sync` (`catalog.len() < 10`, missing
  providers) are load-bearing under retention: a truncated catalog no longer
  shows up as a shrunken table, it shows up as prices that quietly stop
  moving. Do not soften them.
- The canary test asserts table **membership**, not `resolve().is_some()`.
  The cascade's prefix step will answer for a missing id out of a shorter
  sibling and hide the gap.
