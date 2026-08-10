---
id: DIRASH-0023
title: The capture probe rides a daemon-minted reserved session prefix, and the daemon never spawns the child
status: active
guards:
  - cli/dirad/src/probe.rs
  - cli/dira/src/doctor/capture.rs
  - cli/core/src/model.rs
checks:
  - a probe row is invisible to every store read :: cargo test -p dira-core --lib a_capture_probe_row_is_invisible_to_every_read
  - an unarmed probe id is refused at ingress :: cargo test -p dirad --test capture_probe an_unarmed_probe_session_id_is_refused_at_ingress
  - the row is reaped in the same request :: cargo test -p dirad --test capture_probe a_probe_lands_is_reported_and_is_reaped_in_the_same_request
  - a probe row never reaches a sync batch :: cargo test -p dirad --test capture_probe a_probe_row_is_never_selected_for_a_sync_batch
  - the probe payload still normalizes :: cargo test -p dira-sources the_capture_probe_payload_still_normalizes
origin: session
verified: false
---

## Decision

`dira doctor --probe` writes a real event through the real capture path. It is
made safe by four things, in this order:

1. **The daemon mints the session id**, under the reserved prefix
   `dira-probe-`, and returns it from an `Arm` request. The CLI never chooses
   the session it is about to write to.
2. **The daemon admits that id only while its own arm is live** (30s TTL). Any
   other id under the reserved prefix is refused at ingress, so a stale replay,
   a hand-crafted payload, or a `doctor` that died between arm and verify can
   never create a probe row.
3. **Every read in `Store` filters the prefix in SQL** — `events_since`,
   `events_between`, `events_for_sessions`, `human_signal_seed`,
   `count_events_after`, and `compact`'s snapshot. `SessionRegistry::observe`
   refuses it too. `max_event_id` is deliberately left unfiltered.
4. **`verify` deletes the row unconditionally**, in the same request that
   reports it, failure path included.

And the load-bearing constraint on the other side: **the daemon never spawns the
hook child.** `dira doctor` does, under the user's own token.

## Why

**Why the daemon spawns nothing.** The probe exists to detect a daemon whose
control channel refuses ordinary processes — on Windows, one started from an
Administrator terminal, whose named pipe carries that token's DACL and a High
integrity label. A child the *daemon* forked would inherit the elevated token
and open that pipe happily. The probe would pass on exactly the machine the bug
is on, which is worse than having no probe: it would certify the broken state.
The spawning process must be the one whose access is in question.

**Why a daemon-minted id rather than a CLI-chosen one.** The reserved prefix
alone makes probe rows invisible, but it does not stop a probe being *aimed*.
If the CLI passed the id, anything that could send an `IngestHook` could write
under the prefix — and then arrange for real events to be filtered out of
billing by naming a session `dira-probe-…`. Minting server-side plus a TTL'd
admit turns "probe rows are ignored" into "probe rows can only exist because
this daemon asked for one, seconds ago".

**Why filter in `Store` rather than at call sites.** The store is the single
boundary every read crosses, so "a probe row cannot escape" becomes provable by
inspecting one file instead of auditing `sync.rs` (138 KB) forever.
`count_events_after` already has two callers and is the obvious thing a future
feature reaches for; a call-site rule must be re-learned by everyone, a
store-level one is handed to them. The cost is one bound `NOT LIKE` on windowed
queries that already scan — `append`, the actual hot path, is untouched.

**Why `max_event_id` is exempt.** It is only the snapshot upper bound for a sync
window. Filtering it would hold the cursor below a probe id that is about to be
deleted, stalling the window behind a ghost.

**Why `compact` matters most.** A rollup outlives the events it summarises and
no prefix filter can reach `session_rollup_daily`. Everywhere else a leak is
transient; there it would be permanent.

**Why the registry needs its own guard.** `partial_rollups` ships straight from
the live `SessionRegistry`, and `build_session_views` renders from it. Neither
touches SQL, so the store filters cannot reach either.

**Why an unconditional reap.** Deleting only on success leaves a row behind
precisely when something went wrong — the case where the user is least likely
to notice and most likely to be reading their numbers closely.

**Why the writer gets a probe short path** rather than a payload engineered to
survive coalescing and degenerate-session pruning: those behaviours are correct,
and a diagnostic that has to fight them breaks whenever they are tuned. The
probe still traverses `normalize_for` → `EventMsg` → the queue → `enrich` →
`append`, which is the whole chain under test.

**Blast radius if every guard failed at once:** one `user_prompt` row, no
project, no tool, no transcript, in a temp dir, under a session id no rollup
will ever match.

## Rejected

- **A daemon-side lost-hook counter on `DaemonInfo`.** The daemon cannot count
  hooks that never arrived. In the motivating incident it saw zero Claude events
  and looked perfectly healthy — that is the entire problem.
- **Letting the daemon fork the hook child.** Simpler, and wrong: see above.
- **A CLI-chosen probe id.** Removes the arm round-trip, and hands anyone who
  can reach the socket a way to write rows that billing ignores.
- **Filtering the prefix at each sync call site** instead of in `Store`. Spreads
  an invariant across a 138 KB file and every future caller.
- **Deleting by event id.** There is no `delete_event_by_id`, and adding one
  would be the wrong handle: the session id is what the daemon controls.
- **Relying on `assemble_batch`'s existing retain filter** (which already drops
  sessions with no engaged and zero agent time). Incidental, not a prefix rule,
  and it would silently stop protecting us the day that heuristic changes.

## Agent directives

- Never add a `Store` read of `events` without the `session_id NOT LIKE` guard,
  unless it is deliberately probe-facing — and say so in its doc comment, as
  `max_event_id` and `session_event_count` do.
- Never let the daemon spawn the hook child, on any platform, for any reason.
- Never accept a probe session id chosen by the caller. Minting stays in
  `probe::arm`, and `probe::admit` stays the only way a probe row is created.
- Keep the reap in `verify` unconditional. It is not an optimisation to skip it
  when `landed == false`.
- If a new derived table is added (like `session_rollup_daily`), check whether a
  probe row could reach it. A prefix filter cannot follow data once it has been
  aggregated.
- `DIRA_HOOK_PROBE` must never write `hook_health`, in either direction. A probe
  success calling `record_success()` would clear a genuine failure counter the
  user still needs to see.
- Changing the payload in `probe_hook_payload` means re-checking that it still
  normalizes to a human signal — otherwise the probe reports "the daemon never
  stored the event" on every healthy machine.

## Verification

`cli/core/src/store.rs`: a probe row and a real row, identical apart from the
session id and both accruing counted time, are appended; the probe row is
invisible to all six filtered reads, absent from the backlog count, and
`compact` rolls up one session, not two.

`cli/dirad/tests/capture_probe.rs` (7 tests, real writer + real dispatch): the
round trip lands and is reaped in the same request with zero rows left; an
unarmed id is refused; an armed id is refused again once verified; a probe never
enters the live registry; a verify with nothing to find gives up at its deadline;
a second concurrent arm is refused; and a probe row present in the store is not
selected for a sync batch.

`cli/dira/src/doctor/capture.rs`: the stage→verdict table, the access-denied
regression guard, `split_command` over every shape `init` writes plus every
shell-shaped rejection, and the spawn plumbing against a shell stub (payload on
stdin, `cwd == temp_dir()`, probe env set, exit-3 handling, deadline kill).

Verified by hand on macOS against a freshly built daemon: 76 ms round trip, zero
probe rows in the store afterwards, backlog count unchanged; plus the
missing-binary, acked-then-dropped and transport-refused paths each producing a
distinct verdict; and the version-skew backstop degrading to a skip against the
installed 0.3.0 daemon.

**Not verified on Windows** — the elevated-pipe refusal the probe exists to
catch cannot be reproduced on any runner we build on, and the probe has not been
run on a real Windows host.
