---
id: D-0006
title: The update check never performs network I/O on the foreground path
status: active
guards:
  - cli/dira/src/update/notice.rs
origin: recorded
verified: true
---

## Decision

`dira status`, `dira version` and `dira daemon status` read a small cache
file and print at most one line. When that cache is stale they write a
timestamp sentinel, spawn a fully detached `dira update --check`, and do not
wait. The notice is written to stderr.

## Why

An update nag that adds a network round trip to `dira status` makes the tool
feel slow on exactly the command people run most, and makes it hang on a
flaky network. Reading a ~100-byte JSON file is sub-millisecond and cannot
fail in a way that matters — a missing or corrupt cache means "no notice",
never an error. Measured overhead is 149µs cold and 5µs warm.

The sentinel is written *before* spawning, otherwise two concurrent
`dira status` invocations both see a stale cache and both spawn a checker.

stderr rather than stdout is what keeps `dira status | cat` byte-identical
whether or not an update is pending — piping into a script must not change
shape because a release happened.

Suppression splits into two tiers on purpose. `update.check=off`,
`DIRA_NO_UPDATE_CHECK` and a dev build express intent to disable, so they
also gate the background refresh. `CI`, `NO_UPDATE_NOTIFIER` and a non-TTY
stderr only suppress the printed line — the cache still warms, so a later
interactive session is current instead of paying for a first fetch.

## Rejected

- **Check inline, with a short timeout** — still a network call on the hot
  path, and a timeout is a hang the user can feel.
- **Have `dirad` check on its heartbeat** — architecturally tidier, but needs
  a protocol field, gives no notice when the daemon is down or older than the
  CLI, and drags the daemon crate into this concern.
- **One flat suppression list** — would let a piped or CI invocation
  permanently starve the cache, so the first interactive run always pays.

## Agent directives

- Never add a blocking network call to `notice.rs` or to any command that
  calls `maybe_print`.
- Keep the notice on stderr, and keep the call sites to `status`, `version`
  and `daemon status` — never `watch`, `hook`, or `zavet emit`.
- Write the `checked_at` sentinel before spawning the refresh, not after.
- ~~`is_dev_build` here duplicates part of `update::replace::discover_install`
  (D-0004) deliberately, to avoid a dependency between the two modules.
  Unify only if the detection rule itself changes.~~
  **Struck — see the amendment below.** The two now share one predicate,
  `replace::under_target_release_or_debug`. Never re-derive it.

## Amendment: the dev-install predicate is shared (2026-08-14)

The duplication above was permitted "unless the detection rule itself changes".
It changed, silently, by drifting: `notice::is_dev_build_path` matched a
`target` component and a `release`/`debug` component *anywhere* in the path,
while `replace::under_target_release_or_debug` required them *adjacent*. So
`/home/target/projects/release/dira` was a dev build to the notice and an
ordinary install to the updater — the passive notice advertising an upgrade
that `dira update` would then refuse under this very record's sibling, D-0004.

`notice::is_dev_build_path` now calls the `replace` predicate. Adjacency won:
the looser rule has false positives, and a false positive here suppresses a
real upgrade notice.

This does not weaken the "avoid a dependency" reasoning so much as spend it:
`notice` already calls `super::resolve::compare_versions`, so a second
intra-`update` call costs nothing new, and the modules were never independent
crates.

What stays split is the *subject*, and that split is load-bearing. D-0004
requires the install location to come from the PATH entry via
`symlink_metadata`; the notice asks only about the canonicalized
`current_exe()`. Sharing the predicate must never become sharing the subject.

Pinned by `dev_build_path_agrees_with_the_install_guards_predicate`, which
asserts both functions return the same answer for the same path, including the
non-adjacent case that used to split them.
