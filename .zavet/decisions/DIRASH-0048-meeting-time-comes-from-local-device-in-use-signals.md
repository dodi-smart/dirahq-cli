---
id: DIRASH-0048
title: Meeting time comes from local device-in-use signals, never a bot or call content
status: proposed
guards:
  - cli/meetings/**
  - cli/dirad/src/meetings.rs
origin: session
verified: false
---

## Decision

dira captures meeting time from **local, content-free OS signals**:

- a microphone, camera or audio output held by a known call app;
- the call window's class or state;
- a running call process.

dira never joins a call. It never records or transcribes audio or video, and never reads a window
title. The signals fuse into a confidence level:

- **High** confidence is meeting time.
- **Low** confidence becomes a local suggestion. It is never billed until the user confirms it.

Coverage comes from a data catalogue (`cli/meetings/data/apps.toml`) with deny and relay lists.
Process identity is reduced to a closed class at the adapter boundary. Capture is opt-in
(`modules.meetings`, default `off`) and needs no OS permission prompt.

This carries forward the record drafted on `origin/docs/meeting-capture-design`. That record was
numbered DIRASH-0047, a number now held by the headless decision. The design is in
`docs/superpowers/specs/2026-11-09-meeting-tracking-design.md` and the branch spec.

## Why

A notetaker bot is visible to attendees, needs vendor OAuth and admin consent, and touches call
content. That puts dira on the wrong side of D-0001 by construction. The OS already knows which
process holds the mic, the camera or an audio stream: it draws its privacy indicator from that
state. Reading that state needs no content.

A single device signal has blind spots:

- a mic joined muted;
- a listen-only call;
- a dictation app holding the mic.

So nothing is billed unless independent signals agree, and the person is asked about the rest.

## Rejected

- **A bot or notetaker in the call.** It is visible to attendees and content-bearing.
- **Matching window or tab titles.** Titles are content, and reading them needs Screen Recording
  on macOS.
- **The calendar as the always-on primary source.** It misses ad-hoc calls and records skipped
  meetings. It stays optional (DIRASH-0052).
- **Local idle or lock as a veto.** A listener touches nothing, so it would delete real meetings.

## Agent directives

- Never add a window title, calendar title, attendee, meeting id, URL or raw process name to the
  store, the logs, `dira doctor --json` or `contract/`.
- Never bill a low-confidence run without the user's confirmation.
- Never request a TCC or privacy permission for this feature. A signal that needs one is disabled
  and reported as `skip`.
- A deny-list or relay holder alone never starts a meeting.
- App coverage is data, and the wire `providerClass` stays a small closed enum.
