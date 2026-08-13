---
id: DIRASH-0030
title: Full-content knowledge sync is opted into by its own prompt, never implied by linking
status: active
guards:
  - cli/dira/src/onboard/steps.rs
  - cli/dira/src/config_cmd.rs
checks:
  - the consent text names the content it sends :: cargo test -p dira --bin dira the_knowledge_prompt_names_what_it_sends
  - the tier round-trips in the core's own spelling :: cargo test -p dira --bin dira the_written_knowledge_tier_matches_the_cores_spelling
  - an explicit tier skips the prompt :: cargo test -p dira --bin dira an_explicit_tier_skips_the_prompt
origin: session
verified: false
---

## Decision

`dira onboard` asks about the knowledge sync tier in a step of its own, whose
prompt names exactly what `full` sends: decision and spec bodies, commit
trailer values, and guard check commands. The default answer is yes.

Three constraints hold that up:

1. **Never bundled.** The question is never folded into device linking, into
   billing consent, or into the zavet install. Declining content lands on
   `metadata`, not `off` — the user declined *content*, not the channel.
2. **Always restated.** The tier appears in the closing summary on every path,
   including `--yes` and `--knowledge <tier>`, so a non-interactive run never
   leaves the user unaware of what was set.
3. **Honest about reach.** With no linked device the step reports the tier as
   pending, because the daemon's flush is gated on a cloud URL and a link. It
   also states that the workspace has its own tier and that bodies are stored
   only when both ends say `full`.

`sync.knowledge` is settable through `dira config set`, which it was not
before.

## Why

D-0001 keeps the attestation wire content-free and knowledge on its own
channel, and explicitly refuses to couple knowledge consent to billing
consent. That separation existed on the wire but nowhere in the product: there
was no prompt, no ToS step, and no surfacing of the tier in `dira status` or
`dira doctor`. The only user-facing signal was a `tracing::warn!` in the daemon
log on a `content_not_allowed` response — visible to nobody. A wire-level
separation that the user is never told about is not consent.

Defaulting to yes is a separate question from bundling, and the distinction is
the whole point of this record. `full` is what makes the cloud's knowledge
surfaces useful, so it is the right default; what would be wrong is obtaining
it by implication. Asking on its own terms, in its own step, naming the
content, and restating the result keeps D-0001's separation intact while still
making the useful thing the easy thing.

**Why the knob had to become settable.** `[sync] knowledge` existed end to end
— producer, daemon transport, wire types — but was absent from `KNOBS`, so
`dira config set sync.knowledge full` hard-bailed. The only ways in were
hand-editing `config.toml` or exporting `DIRA_SYNC__KNOWLEDGE`. A consent
prompt cannot honestly be offered for a setting the CLI has no supported way
to write.

**Why `metadata` rather than `off` on a decline.** The two tiers differ by
field, not by record (`KnowledgeBatch::strip_content`): `metadata` still
carries ids, titles, status, guard globs and record hashes, which is what the
cloud needs to correlate decisions with sessions at all. Dropping to `off`
would silently disable the feature the user just installed.

## Rejected

- **Fold the tier into the device-link consent.** Exactly the coupling D-0001
  refuses. Linking is about billing and sync of content-free attestations;
  knowledge content is a different question with a different blast radius.
- **Default to `metadata` and let users opt up.** Nothing surfaces the tier in
  `status` or `doctor`, so a user who wanted `full` would have no way to
  notice they were not getting it. If the disclosure is honest, the useful
  default is the right one.
- **Write `[sync] knowledge` directly from the onboard step.** A second place
  deciding what a valid tier is. It goes through `config_cmd`'s own
  validation via `set_quiet`.
- **Say "knowledge sync enabled" without the workspace caveat.** The gate is
  double-ended; claiming success for a setting that stores nothing until the
  dashboard agrees is the kind of quiet lie this codebase's doctor work
  exists to eliminate.

## Agent directives

- Never obtain knowledge-content consent as a side effect of another step.
  A new surface that enables content sync asks for it separately.
- Any change to what `full` transmits must change `KNOWLEDGE_DISCLOSURE` in
  the same commit. That constant is the only place the user is told, and a
  test asserts it names the content.
- Never report a tier as active when the device is unlinked or when the
  workspace side is unknown — say pending, and name the dashboard setting.
- Write the tier through `config_cmd::set_quiet`, never by editing TOML in
  place.
- A decline means `metadata`. Only an explicit `--knowledge off` means off.

## Verification

Unit tests in `cli/dira/src/onboard/steps.rs` drive the step with a scripted
`Ui` that records every line the user was shown:
`the_knowledge_prompt_names_what_it_sends` asserts the disclosure mentions
record bodies, trailer values and check commands;
`declining_content_falls_back_to_metadata_not_off` pins the decline target;
`an_explicit_tier_skips_the_prompt` pins the flag path.

`config_cmd`'s `the_written_knowledge_tier_matches_the_cores_spelling`
round-trips the emitted TOML against `KnowledgeSyncMode::as_str` — core's own
declaration of the `config.toml` spelling — so a rename there breaks the test
rather than shipping a config the daemon rejects at startup. The e2e test
`the_knowledge_tier_is_written_to_config_toml` confirms the value reaches a
real `config.toml` under an isolated `$HOME`.

**Not verified end to end against a live cloud.** That the daemon actually
POSTs bodies at `full` — and that a metadata-tier workspace answers
`content_not_allowed` — is covered by the daemon's own tests, not by anything
this work added.
