---
id: DIRASH-0039
title: dira consumes source-backed specs only through the installed plugin's `zavet specs --json`, never the repo's vendored zavet
status: active
guards:
  - cli/core/src/zavet_specs.rs
  - cli/dirad/src/zavet_sources.rs
checks:
  - the repo's vendored zavet is never executed :: cargo test -p dirad --test zavet_sources the_repos_vendored_zavet_is_never_executed
  - a wedged plugin is killed at the deadline :: cargo test -p dira-core --lib zavet_specs::tests::the_system_runner_kills_a_wedged_plugin_at_the_deadline
  - an old plugin or a failed resolve changes nothing :: cargo test -p dirad --test zavet_sources a_failed_or_unsupported_resolve_changes_nothing
  - a vanished unit is marked removed, never deleted :: cargo test -p dirad --test zavet_sources a_source_unit_is_upserted_removed_and_never_deleted
  - text never crosses at the metadata tier and is stripped on the retry :: cargo test -p dirad --lib knowledge_sync::tests::source_wire
  - removals only come from the default-branch checkout :: cargo test -p dirad --test zavet_sources removals_only_happen_from_the_default_branch_checkout
origin: session
verified: false
---

## Decision

Spec-driven-development frameworks (OpenSpec, Spec Kit, BMAD, superpowers)
reach dira as ordinary zavet living specs (slug `<kind>:<id>`, origin
`external`, never verified) and **only** through the installed zavet plugin's
`zavet specs --json`. The adapters that read those frameworks' files live once,
in zavet. dira owns none of them.

1. **The binary is the plugin's, never the repo's.** It is resolved offline from
   Claude Code's own registry (`dira_core::zavet_specs::plugin_root_offline`)
   and run as `<plugin_root>/bin/zavet`. The repo's vendored
   `.zavet/bin/zavet` is checked-in code; dira never executes it.
2. **Capability-gated and time-boxed.** `version --json` must advertise
   `specs_json >= 1`, and each invocation has a 5 s deadline after which the
   child is killed.
3. **Fail open.** A missing plugin, an old plugin, a timeout or a malformed line
   yields no source specs for that sweep (or one skipped row), logged at debug.
   A failed resolve changes nothing and in particular never marks a unit
   removed.
4. **Metadata by default, text only on request.** Rows are mapped to the
   contract `SpecSource` without text. `--with-text` is passed only by the
   consent-gated full-tier sync, which re-reads the text from the repo each
   time; the store never holds it.
5. **Removed, never deleted, and only on good evidence.** A source slug seen
   before and absent from a fresh, successful resolve becomes state `removed`,
   provided that resolve can speak for the repo: (a) the resolving checkout is
   on the repo's default branch (`git symbolic-ref refs/remotes/origin/HEAD`,
   else a local `main`/`master`; unknown is "not default"), because the store
   is keyed by canonical repo while a resolve sees a single checkout, so a
   feature branch or worktree that predates a unit would flap it removed and
   back; and (b) an empty answer for non-empty `spec-source:` declarations is
   corroborated by every declared root being absent from the checkout. Off the
   default branch, seen units are still upserted and nothing is removed. A
   reader that stalls is a timeout and output over the 16 MiB cap is its own
   error: neither is ever a shorter, "successful" answer. A timeout kills the
   plugin's whole process group.

## Why

Two forces pull opposite ways and this is where they meet. The cloud needs
framework state now, so spec search and SDLC statistics can be built later
without another contract bump. But parsing four frameworks' file layouts is
exactly the knowledge `bin/zavet` already has to carry for `zavet specs`, `gate`
and `audit`; a second implementation in Rust would disagree with the first the
first time a framework changes a heading.

The binary choice is a security boundary, not a convenience. The daemon is a
long-lived process that runs as the user on every repo it has seen a session in.
`.zavet/bin/zavet` is whatever the last commit on whatever branch put there, so
executing it from the daemon would let any repository a developer merely opened
run code in the daemon's context. The installed plugin is code the user chose
to install. DIRASH-0024 draws the same line for the adapters step ("the stale
artifact being regenerated"); this applies it to a path that runs unattended.

Fail-open is what makes it safe to ship ahead of the plugin: a dira that
predates the capability, or a machine with no plugin, behaves exactly as before.
Not removing on a failed resolve is the same principle from the other side: an
empty answer caused by a timeout is indistinguishable from "every change was
deleted", and only a successful resolve is allowed to say the latter.

## Rejected

- **Execute `.zavet/bin/zavet`.** It is faster to find and always version-matched
  to the repo, and it runs repository content in the daemon. Rejected outright.
- **Parse the frameworks in Rust.** One adapter per framework in dira, a second
  copy of each in zavet. Drifts on the first framework release.
- **Shell out from the writer task.** The writer is the single accrual path;
  a hung plugin would stall every session's accounting. The resolve runs after
  the commit walk under its own budget so a slow plugin costs source specs, not
  commits.
- **Store the text locally so the full tier need not re-run the plugin.**
  Everything content-bearing stays out of SQLite (D-0001, DIRASH-0030); the
  re-read costs one plugin run per changed unit at the tier that already
  accepted content.
- **Treat a failed resolve as "no units".** Would mark every unit removed on a
  timeout.
- **Let any checkout mark removals.** A worktree on an old branch resolves
  without a unit that exists on the default branch; the next sweep from the
  default checkout would resurrect it, and each flap re-pushes the knowledge
  set.

## Agent directives

- Never resolve a plugin path from the repo. Add a root only to
  `plugin_root_offline`, and keep `DIRA_ZAVET_PLUGIN_ROOT` authoritative when
  set: a pinned root must not silently fall back to a different one.
- Never run the plugin on the writer task or inside `git_walk`'s blocking
  budget. `capture::refresh_sources` owns it.
- Never pass `--with-text` outside the full-tier sync, and never persist what it
  returns. If a field of `SpecSource` becomes content, add it to
  `strip_source_text`, `KnowledgeBatch::strip_content` and
  `KNOWLEDGE_DISCLOSURE` in the same commit.
- Never delete a source spec row, and never mark one removed from a resolve that
  did not succeed, from a non-default-branch checkout, or from an empty answer
  whose declared roots still exist.
- Keys that ride the signed wire (`story_status`, `by_state`) come from a
  closed vocabulary with an `other` bucket, never from verbatim framework
  strings: a key like `__proto__` is stripped by the cloud's zod and fails the
  batch as `bad_signature`.
- A session's spec is bound from facts only: an explicit `dira start --spec`,
  else an exact `Spec:` trailer slug, else an exact branch match with a unique
  hit. A substring or fuzzy match is a guess, and guesses are not recorded.

## Open questions

- Source units are not discovered by `uncaptured` (decisions/wiki list records on
  disk the store lacks): a unit is not a file, and enumerating it needs a plugin
  run on a read path. `dira zavet reindex` is the recovery.
