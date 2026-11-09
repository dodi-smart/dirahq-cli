---
id: DIRASH-0066
title: The daemon obeys the workspace meeting policy; admins can disable but never force capture on, and the user's opt-out always wins
status: proposed
guards:
  - cli/dirad/src/meetings.rs
  - cli/dira/src/onboard/**
origin: session
verified: false
---

## Decision

`workspaces.settings.meetings` sets the workspace meeting policy:

- `capture`: `off`, `opt_in` or `recommended`;
- `calendar` and `labels`;
- `managerVisibility`: `aggregate_only` or `person_rows`;
- `overlapPrecedence`;
- `retentionDays`.

The policy reaches the device in the workspace directory and in the sync acknowledgement, and the
daemon caches it.

When `capture` is `off` for a workspace:

- Meeting records for that workspace are not sent, and its meeting slices fold back into work.
- If every linked workspace is `off`, no gate is constructed.
- The onboarding step reports `Skipped`.

There is no "required" value. The local knob and a per-user cloud opt-out always win.
`dira status` shows the effective state and why.

## Why

Employers in the EU deploy dira under GDPR legitimate interest with a DPIA. In Germany, a works
council has co-determination over any tool that is objectively suitable for monitoring (BAG
1 ABR 20/21). An admin must therefore be able to turn the feature off org-wide, and to lock
person-level visibility to aggregates.

Forcing capture on would turn a self-tracking tool into covert monitoring, and it would contradict
the per-feature consent model (DIRASH-0030).

## Rejected

- **An admin "required" mode.** It is surveillance by default, and the user's local control would
  be meaningless.
- **Policy only enforced in the cloud,** by dropping ingested meetings. The device would still
  collect data its workspace forbids.

## Agent directives

- Never let workspace policy enable capture that the local knob has turned off.
- A policy fetch failure keeps the last cached policy. With no cache, the daemon behaves as
  `opt_in` and never as more permissive.
- Person-level meeting views require both the lens's person-rows control and
  `managerVisibility = person_rows`.
