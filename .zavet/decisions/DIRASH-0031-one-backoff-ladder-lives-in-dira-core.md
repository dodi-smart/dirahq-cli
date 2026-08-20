---
id: DIRASH-0031
title: One backoff ladder lives in dira_core; callers own their attempt budget
status: active
guards:
  - cli/core/src/sync/ratelimit.rs
  - cli/dira/src/update/retry.rs
  - cli/dirad/src/sync.rs
checks:
  - the shared ladder caps both arms :: cargo test -p dira-core --lib sync::ratelimit
  - the updater keeps its own seed and cap :: cargo test -p dira --bin dira update::retry
origin: session
verified: true
---

## Decision

The seed/double/cap wait lives once, as `dira_core::sync::Backoff { seed, max }`.
`dirad::sync` and `dira update` each construct one with their own numbers.
The *attempt budget* does not move: the daemon retries the cloud indefinitely,
the updater gives up after four tries, and only the caller knows which it is.

This reverses `update/retry.rs`'s recorded micro-decision to keep a deliberate
local mirror.

## Why

The mirror's reasoning was that `dira` does not depend on `dirad` and the two
want different caps. Both are still true and neither argues for a second
implementation: the caps are *values*, and both crates already depend on
`dira_core`. `update/retry.rs` was importing `parse_retry_after_secs` from the
very module the ladder now lives in.

The mirror had also already failed at the one thing it was for. "Keeping the
shape identical is the point" was the stated rationale, and the shapes had
drifted: the daemon capped both branches of `transient_wait`
(`unwrap_or_else(ladder).min(MAX)`), the updater capped only the `Retry-After`
one (`map_or_else(ladder, |a| a.min(max))`). Equivalent solely because the
ladder is pre-capped. So the day someone returns an uncapped value from the
ladder, one caller is wedged by a hostile `Retry-After` and the other is not.
A copy that must be kept identical by hand is not a shape guarantee.

The shared version caps both arms unconditionally, which is the redundant-but-
safe reading of the two.

## Rejected

- **Keep the mirror, fix the drift in place**: restores the invariant for
  exactly as long as nobody edits either copy again. The drift is the evidence
  that hand-synchronised copies do not stay synchronised.
- **Share the retry loop as well, not just the ladder**: the two loops are not
  the same thing. The daemon dispatches on typed `SyncError` variants with arms
  that slam to the cap or hard-stop; the updater has a binary retry/fatal split
  and an attempt budget. Unifying those would mean inventing a shape neither
  caller wants.
- **Put `Backoff` in a new `dira_core::retry` module**: `sync::ratelimit`
  already owns `Retry-After` parsing, which is the input to the very function
  being shared. A second module would split one concern across two.

## Agent directives

- One ladder: never re-add a local seed/double/cap implementation. Construct a
  `dira_core::sync::Backoff` with your own numbers instead.
- Both arms of `transient_wait` stay capped. The ladder arm is redundant today;
  it is what stops the two arms drifting apart again.
- Attempt budgets and per-attempt timeouts stay with the caller. Do not move
  `Policy::attempts` or `Policy::timeout` into `dira_core`.
- `Backoff` is pure timing. Nothing payload-bearing goes in it (D-0001).

## Verification

`cargo test -p dira-core --lib sync::ratelimit` (9 tests) covers seed/double/cap
on both callers' real numbers, `Retry-After` override and clamping, and asserts
directly that *both* arms are capped for every input. That is the property the
two copies disagreed on.

`cargo test -p dira --bin dira update::retry` asserts the updater still carries
500ms/4s after the move: the ladder became a value, and a value can be changed
without breaking a compile.

`cargo test -p dirad --lib sync::` (54 tests) is unchanged and still green.
`next_backoff`/`transient_wait` were kept as named wrappers, so no daemon call
site moved.
