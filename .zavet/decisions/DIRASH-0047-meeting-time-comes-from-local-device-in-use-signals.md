---
id: DIRASH-0047
title: Meeting time comes from local device-in-use signals, never call content or a bot
status: proposed
guards:
  - cli/meetings/**
origin: session
verified: false
---

## Decision

Dira captures meeting time from **local, content-free OS signals**: a
capture device (microphone, camera) or an audio output stream held by a known
meeting application, optionally corroborated by the presence of a call window
(class or state only) and by a meeting process running. It never joins a call,
never records or transcribes audio or video, never reads a window title, and
never reads a calendar event's title or attendees into anything that is stored
or sent.

The signals fuse into a confidence level. Only **high**-confidence intervals
become meeting time; **low**-confidence ones are surfaced as suggestions the
user confirms, and are never billed silently. The capture is opt-in
(`[modules] meetings`, default off) and runs inside `dirad`, one adapter per OS
behind a single `MeetingSignal` trait.

Calendar correlation and enterprise org-API reconcile are later, separately
opted-in layers. They may *raise* confidence or *correct* durations; neither is
required for the base layer to work.

Full design, the per-OS signal matrix and the muted-mic evidence are in the
`meeting-capture` spec.

## Why

Managers and developers want meetings tracked without remembering to start a
timer, and the obvious route (a notetaker bot that joins the call) is the
opposite of what a local-first, content-free tracker can offer: it is visible to
other attendees, needs per-vendor OAuth and admin consent, and touches call
content. D-0001 makes the attestation wire content-free and is the reason
managers can trust billing data; a bot would put dira on the wrong side of that
line by construction.

OS privacy indicators already answer "is a call happening?" without any content.
Every desktop OS exposes which process holds the microphone, camera or an
active audio render stream, because the OS itself draws the orange/green dot or
the tray indicator from that same state. Reading that state needs no prompt and
no entitlement that `dirad` does not already have (see the spec's permissions
section for the per-signal verification status).

The confidence split exists because a device-in-use signal has known blind spots
(a muted mic, a camera-off listen-only call, a mic held by a dictation app).
Treating every positive as billable would over-bill; treating every gap as
"not a meeting" would under-track. Billing only what two independent signals
agree on, and asking the human about the rest, keeps the tracker honest in both
directions.

## Rejected

- **A bot or notetaker joining the call.** Visible to attendees, per-vendor
  OAuth plus admin consent, touches call content. Violates the "no bot"
  requirement and the spirit of D-0001.
- **Window-title or calendar-title matching as a primary signal.** Titles are
  content. They would also need Screen Recording permission on macOS 10.15+
  (`kCGWindowName`). Class/state-only is the rule for window checks.
- **Calendar as the primary source.** Needs OAuth, misses ad-hoc calls, and
  records meetings that never happened. Kept as an optional corroborating layer.
- **Org-API (attendance reports) as the primary source.** Admin-gated, lagging,
  and Google's Meet API terms forbid performance-evaluation use. Kept as an
  optional enterprise reconcile only.
- **Reusing `SessionKind::Manual` for detected meetings.** Detected and typed
  time have different assurance, and the cloud must be able to tell them apart.

## Agent directives

- Never add a window title, calendar title, attendee, meeting id, URL or app
  window text to any type in `contract/`, to the local store, to logs, or to
  `dira doctor --json`. A provider is reduced to a closed `provider_class`
  enum at the adapter boundary and the original string is dropped there.
- Never bill a low-confidence interval without an explicit user confirmation.
- `dirad` must not request a TCC/privacy permission for this feature. If a
  signal needs one, that signal is disabled and reported as `skip` by
  `dira doctor`, never prompted for.
- `dira doctor` checks for meetings report and never repair; missing evidence
  (no audio stack, no display server, unsupported OS version) is `skip`
  (DIRASH-0022).
- New wire fields must stay clear of the content denylist tokens
  (`wire_contract_carries_no_content_fields`).
- Once implemented, bind checks to this record (classifier fusion table, the
  denylist test with the new fields populated, doctor skip-on-absent) and set
  `status: active`.
