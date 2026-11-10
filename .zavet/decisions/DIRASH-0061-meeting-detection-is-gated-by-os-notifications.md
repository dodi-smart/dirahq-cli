---
id: DIRASH-0061
title: Meeting detection is gated by OS notifications; the per-process walk runs only while a call is a candidate
status: proposed
guards:
  - cli/meetings/src/gate.rs
  - cli/meetings/src/macos.rs
  - cli/meetings/src/windows.rs
  - cli/meetings/src/linux.rs
  - cli/dirad/src/meetings.rs
origin: session
verified: false
---

## Decision

Device-level "in use" flags are subscribed to, not polled:

- **macOS:** CoreAudio and CMIO `IsRunningSomewhere` listeners.
- **Windows:** `RegNotifyChangeKeyValue` on the ConsentStore, plus WASAPI session notifications.
- **Linux:** a `pw-dump --monitor` child process.

A 30 s process scan catches calls joined muted, through call-only processes such as Zoom's
`CptHost`/`CptService`. The per-process walk at 5 s runs **only while a candidate exists**. With no
candidate, the steady state is that one 30 s scan.

Listeners are a wake-up hint, never the only source of truth:

- The scan re-reads the device flags, so a missed notification costs at most 30 s of start latency.
- The start is back-dated, so that latency costs no time.

Linux talks to PipeWire through its CLI. It never links libpipewire, which keeps artifacts static
musl.

## Why

`dirad` runs all day on laptops. Polling every 5–10 s, as the October design did, costs battery
for the 90% of the day with no call. Per-process listeners were reported not to fire on
macOS 15/26, so walking per-process state is kept. It is limited to when it matters.

## Rejected

- **Steady 5/10 s polling.** It is simple, but costs wakeups all day.
- **Relying on listeners alone.** They are reported unreliable, and a missed edge would lose a
  meeting.
- **Linking libpipewire or libpulse.** That breaks static-musl Linux artifacts.

## Agent directives

- Never add a fixed-interval poll that runs while no candidate exists, beyond the 30 s process scan.
- Every listener path needs a scan backstop and a test with the `fake` adapter that drops
  notifications.
- Never link a system audio library on Linux.
