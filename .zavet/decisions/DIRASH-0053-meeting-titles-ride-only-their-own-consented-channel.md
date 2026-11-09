---
id: DIRASH-0053
title: Meeting titles ride only their own consent-gated labels channel, never the attestation wire
status: proposed
guards:
  - contract/src/lib.rs
  - cli/dira/src/onboard/steps.rs
origin: session
verified: false
---

## Decision

A user can opt in to sending the calendar titles of their matched meetings. They go only through a
separate `MeetingLabelsEnvelope`, posted to its own route and signed with the device key, in the
same way as knowledge sync.

The opt-in has its own prompt, `MEETINGS_LABELS_DISCLOSURE`, which names exactly what is sent. That
prompt is never bundled with meeting capture, calendar connection, device linking or billing.

The workspace must also allow labels (`meetings.labels: allowed`). Otherwise the cloud answers
`content_not_allowed`. Declining keeps titles local, visible only in `dira meetings`. Labels are
erased with their meeting.

## Why

A title is content. D-0001 keeps content off the attestation wire and points to a separate,
consented channel, as knowledge sync does (DIRASH-0030). The owner chose an opt-in channel over
"local only" (2026-11-09) so that people who want titles on invoices can have them, without
weakening the content-free guarantee for anyone else.

## Rejected

- **Titles on the attestation wire when opted in.** That breaks D-0001 for every verifier of the
  wire.
- **A title hash.** It is reversible by dictionary and adds nothing over the series key.
- **Folding the title consent into the capture or calendar prompt.** DIRASH-0030 forbids implied
  consent.

## Agent directives

- Changing what the labels channel sends changes `MEETINGS_LABELS_DISCLOSURE` in the same commit.
- Never add a title field to `AttestationBatch` or to any of its records.
