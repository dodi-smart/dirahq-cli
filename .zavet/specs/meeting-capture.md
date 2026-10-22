---
title: Meeting capture — local device-in-use signals, no bot
version: 1
status: proposed
origin: session
verified: false
confidence: med
date: 2026-10-22
paths:
  - cli/meetings/**
  - cli/dirad/src/meetings.rs
  - cli/dira/src/doctor/meetings.rs
decisions: [D-0001, DIRASH-0022, DIRASH-0029, DIRASH-0030, DIRASH-0043, DIRASH-0047]
---

## Overview

**Status: proposed. Design only; no code exists.** The `paths` above are where
the work is expected to land, so staleness starts when commits touch them.

Today a meeting is time only when a human types `dira start --activity
meeting` (a `SessionKind::Manual` session). Managers and developers want
meeting time tracked automatically, with no bot in the call, working on macOS,
Windows and Linux, and with nothing content-bearing leaving the machine (D-0001).

The approach (DIRASH-0047) is to read what the OS already knows: **which
process holds a capture device or an active audio stream**. The same state
drives the OS's own orange/green privacy indicators. `dirad` samples it, fuses
four independent signals into a confidence level, and then:

- **high** confidence becomes meeting time, recorded as an `Interval` with
  `activity: "meeting"`;
- **low** confidence becomes a *suggestion* the user confirms or dismisses.
  It is never billed silently.

Calendar correlation and enterprise org-API reconcile are later, separately
opted-in layers (see "Later layers"). The base layer needs neither.

### Evidence legend

Every external claim below carries a tag:

- **V** verified against a primary source (vendor docs, SDK headers, protocol
  registries) read during this design;
- **R** reported: third-party code, issues, forum posts or blogs, not
  independently reproduced;
- **U** unverified: no source found, or inferred.

No claim in this spec was reproduced first-hand on a real call. Section
"Validation before this becomes active" lists what must be.

## Behavior

### The four signals

`dirad` polls an OS adapter (one `MeetingSignal` per OS, see "Module shape").
Each poll yields four readings. Each reading is `On`, `Off` or
`Unavailable(reason)`. **`Unavailable` is never read as "no meeting"**; it just
removes that signal from the fusion.

| | macOS | Windows | Linux |
|---|---|---|---|
| **A. Capture device held open by a meeting app** | CoreAudio `kAudioDevicePropertyDeviceIsRunningSomewhere` on each input device (V, header) as a cheap gate; then `kAudioHardwarePropertyProcessObjectList` plus per-process `kAudioProcessPropertyIsRunningInput`, `PID`, `BundleID` (V, header; macOS 14.0 to 14.2 floor, R). Camera: `kCMIODevicePropertyDeviceIsRunningSomewhere` (V, header) | `HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{microphone,webcam}\<app>` and `...\NonPackaged\<exe path with \ as #>`: `LastUsedTimeStart != 0 && LastUsedTimeStop == 0` means in use (**R only**, undocumented by Microsoft; Win10 1903+, R). Scan both key levels | PipeWire: `pw-dump -m` (V that `-m` monitors) nodes with `media.class = Stream/Input/Audio` or a `Video/Source` consumer, attributed by `application.process.binary` and `application.process.id` (V keys; JSON layout U). Fallback: `pactl -f json list source-outputs` (U on exact keys). Camera: `/dev/video*` held open via `/proc/*/fd` (same-uid only), plus portal-camera streams via PipeWire |
| **B. Per-process audio output active** (covers listen-only calls) | per-process `kAudioProcessPropertyIsRunningOutput` (V, header) | WASAPI: `IMMDeviceEnumerator::EnumAudioEndpoints(eRender)`, `IAudioSessionManager2::GetSessionEnumerator`, `IAudioSessionControl::GetState == AudioSessionStateActive`, `IAudioSessionControl2::GetProcessId` (V, Windows 7+). Microsoft warns the enumerator can miss sessions, so also register `RegisterSessionNotification` | `pw-dump` `Stream/Output/Audio` nodes, or `pactl -f json list sink-inputs` (`corked` key U) |
| **C. Call window present** (window **class or state only, never the title**) | `CGWindowListCopyWindowInfo` for owner PID, layer, on-screen flag. **Owner-name and title are not relied on**: names come from the PID via `proc_pidpath`/`NSRunningApplication` (see Permissions) | `EnumWindows` + `GetClassNameW` + `GetWindowThreadProcessId`. Class names are R only (Zoom `ZPContentViewWndClass`, `ZPControlPanelClass`); Teams and every Chromium app share `Chrome_WidgetWin_1`, so match the owning exe, not the class alone | X11: `_NET_CLIENT_LIST` + `WM_CLASS` + `_NET_WM_PID` (V, needs `DISPLAY`). Wayland: **no generic API**. `ext-foreign-toplevel-list` and the wlroots protocol exist on Sway, Hyprland, niri; Mutter and KWin do not offer them (V). C is `Unavailable` on stock GNOME and KDE Wayland |
| **D. Known meeting process running** | `NSWorkspace`/libproc, bundle id prefix match (Zoom helpers are separate process objects, match by prefix, R) | process list, exe name | `/proc/*/exe`, `comm` |

Process identity is reduced at the adapter boundary to a closed
`ProviderClass` (see "Contract proposal") and the raw bundle id, exe name, path
or `WM_CLASS` is dropped there. Nothing downstream, including logs and
`doctor --json`, ever sees it.

Per-OS attribution caveats (all R): macOS helper processes appear as
`com.google.Chrome.helper`, `com.microsoft.vcxpc` (Teams), and Safari's
`com.apple.WebKit.GPU` is shared by every WebKit client, so Safari is never
attributable to a meeting; `application.*` PipeWire keys are client-supplied
and spoofable (only `pipewire.sec.*` is trustworthy), and Flatpak/Snap PIDs may
be namespace-local, so fall back to `pipewire.sec.app-id` or
`application.name`.

### The muted-mic and camera-off case

This is the user's explicit question: *what happens when the camera is off
and the mic is muted?* Short answer: **it depends on the app and on whether the
user ever unmuted**. No vendor documents the OS-level behaviour. All evidence
below is third-party. The design therefore does not depend on A alone.

| App | Joined unmuted, then muted, camera off | Joined muted from the start, camera off |
|---|---|---|
| **Zoom** (native) | Mic stays open while muted: a Hammerspoon maintainer says Zoom "grabs the microphone at the start of a meeting" and mute is internal (R: github.com/Hammerspoon/hammerspoon/issues/3057); TalkTrack (Windows) says Teams and Zoom "usually keep the Windows mic open when muted in-app" (R, an assertion without a test: github.com/ObscureAintSecure/TalkTrack/issues/124). Zoom's speaking-while-muted notice implies local capture continues (U, inferred) | **Likely no mic stream** (R: a call joined muted "reads as no live input", github.com/BuilderIO/agent-native/pull/3611, app unnamed). Listening still drives output (B) |
| **Teams** (new, native) | Mic stays open: a test on Windows found ConsentStore `LastUsedTimeStop` is `0` for the whole call "muted or not" (R: ObscureAintSecure/TalkTrack/issues/148). Microsoft's "Your microphone is muted" feature exists (V) and a Microsoft Q&A answer says capture continues (R) | Likely no stream, same caveat (R/U) |
| **Slack huddles** | **Unknown.** One project suspects Slack releases the input while muted but was still sampling a live huddle (U: nsorros/meeting-recorder/pull/20). Slack does release the mic when the huddle ends (R) | Unknown (U) |
| **Webex** | A Cisco community snippet says the mic stays open for the "you need to unmute" prompt (U: page returned 403, only the snippet was seen) | Unknown (U) |
| **Meet in Chrome or Edge** | Chromium: setting `track.enabled = false` stops RTP, yet "the microphone device is still capturing" and the tab indicator stays on (V for a generic WebRTC app: mediasoup.discourse.group/t/demo-chrome-possible-small-bug/810; Chrome's WebRTC lead called it "a bug in Chrome", crbug 642785, V via github.com/w3c/mediacapture-main/issues/642). Hangouts worked around it by releasing the **camera** on mute and leaving the mic alone (V, same thread). **Meet's current implementation is U**, and one report says "a browser drops mic input when muted" (R, conflicting: BasedHardware/omi/pull/7629, browser unnamed, code later removed). Edge has no separate evidence; presumed same as Chrome (U) | Likely no stream (R/U) |
| **Meet in Firefox** | Firefox releases the device on mute: since Firefox 60 video mute turns the hardware light off (3 s minimum) and "this works for the microphone as well" (V: blog.mozilla.org/webrtc/better-privacy-on-camera-mute-in-firefox-60/). Whether the OS mic indicator clears on every Firefox mute is U | Same |

Camera-off: native apps appear to **release** the camera when video is turned
off (R, macOS: foeken/meeting-notes/pull/20, camera-based detection vanished
when the camera was turned off while the mic stayed held); Firefox does (V);
Chrome releases via a stop-and-reacquire workaround (R). Camera is therefore a
*bonus* signal, never a necessary one.

**What the evidence means for the design:**

1. A held mic is **sufficient but not necessary** evidence of a meeting. It
   must not be the only way in.
2. The dangerous case is **joined muted, camera off**: no capture stream, so A
   is dark. This is exactly why B (output), C (window) and D (process) exist.
   For a native meeting app, **B plus C plus D** carries the listen-only and
   joined-muted cases at high confidence.
3. For browsers B plus D cannot tell Meet from a music tab, and C cannot read
   titles, so a muted browser call with no mic stream is **low confidence**
   (a suggestion). Calendar correlation, when enabled, upgrades it.
4. Once a meeting is open, a mute that releases the device (Firefox, possibly
   Slack) must not end it. Continuation uses any of A or B for the same
   provider class, with a grace period (below).
5. Windows: `LastUsedTimeStop == 0` can be stale after a crash or sleep (R,
   Teams2HA), producing a phantom "in meeting". Require that a process of that
   executable is actually running, and require one observed "not in use"
   reading since daemon start before trusting a held reading.
6. Stuck holders happen (R: the macOS Sound settings pane held the mic for
   18 hours). A held-only reading with no output for over 3 hours downgrades
   to low and is surfaced as a suggestion, not billed.

Linux has **no evidence at all** for any app (U). macOS has the most (R),
Windows one tested report (R).

### Fusion into a confidence level

Inputs per sample: A, B, C, D, each `On/Off/Unavailable`, plus the provider
class of the holder. A `ProviderClass` is *native* (Zoom, Teams, Slack, Webex,
other known desktop client) or *browser* (Chrome, Edge, Firefox, other; never
Safari, see above).

| Rule | Confidence |
|---|---|
| Native: A on for a native meeting process | **high** |
| Native: B on and C on and D on (listen-only, joined muted) | **high** |
| Browser: A on **and** B on in the same browser (duplex audio), sustained | **high** |
| Native: B on and D on, no C | low |
| Browser: A on only, or B on only, or camera only | low |
| Any: only D on, or only C on | not a meeting (no interval, no suggestion) |
| Calendar layer (later): an overlapping scheduled online meeting | raises low to high; never creates time alone |

State machine, all values provisional tunables to be set from the validation
matrix:

- **Start**: a confidence level must hold for **30 s** sustained. The interval
  is then back-dated to the first sample of that run. Back-dating uses only
  timestamps, no content.
- **Continue**: while any of A or B holds for the same provider class.
- **End**: after a **60 s** grace with neither (reports cite device switches
  and screen-share blips, R). Ends immediately on a monotonic-clock jump over
  the grace period (sleep, lid close); the interval ends at the last sample
  before the gap.
- **Minimum length**: 2 minutes. Shorter runs (a voice-message test, a
  dictation burst) are dropped.
- **Polling**: cheap device-level flags every 5 s while a candidate exists, 10 s
  otherwise; the per-process walk runs only when a device flag says something
  is active (see macOS risk below). Listeners are not relied on: per-process
  listeners were reported not to fire on macOS 15 and 26 and scoped listeners
  not to fire on macOS 26 (R).

A run whose confidence changes mid-way is split: the high span is meeting time,
the low span is a suggestion.

### Opt-in

- Config knob `[modules] meetings = "off" | "suggest" | "auto"`, default
  **off**, settable with `dira config set modules.meetings <mode>` (a knob the
  CLI cannot write cannot honestly be offered for consent, DIRASH-0030).
  - `suggest`: nothing is recorded as meeting time without a confirmation;
    high-confidence runs are listed as suggestions too.
  - `auto`: high-confidence runs are recorded; low-confidence ones are
    suggestions.
- `dira onboard` gets a step of its own (DIRASH-0029 waterfall: it reports an
  outcome and never aborts the run). Its prompt names what is read ("which app
  is using your microphone, camera or speakers, and whether a call window
  exists") and what is not ("no audio, video, screen content, window titles,
  calendar entries or attendees"). It is never bundled with device linking or
  billing consent (DIRASH-0030). `--print` stays byte-identical.
- Nothing is sampled while the knob is `off`: no adapter is constructed.

### Surfacing

`dira status` gains one line, hidden when the module is off:

```
meetings  auto · idle                       # nothing detected
meetings  auto · in a meeting (zoom, high, 12m)
meetings  auto · 2 suggestions to review    # run: dira meetings
meetings  auto · degraded: window signal unavailable (wayland)
```

`dira meetings` lists pending suggestions (time range, provider class, which
signals fired, never a name); `dira meetings confirm <id>` records one,
`dira meetings dismiss <id>` drops it. Suggestions expire after 14 days.
Confirmed time is recorded with `source: user_confirmed`.

`dira doctor` gains `meetings.*` checks (ids added to `CHECK_IDS`, additive per
DIRASH-0022):

| id | Ok | Skip | Warn |
|---|---|---|---|
| `meetings.module` | module on | module off, or OS unsupported | n/a |
| `meetings.adapter` | adapter constructed, signals listed per `On/Off/Unavailable` | daemon unreachable (no evidence) | n/a |
| `meetings.signals` | every signal available | evidence absent (no audio stack, no `DISPLAY`, headless, pre-macOS-14) | a signal is `Unavailable` where it normally exists (for example a TCC denial) |
| `meetings.stuck_holder` | none | no samples yet | a device held over 3 h with no output |

Doctor **reports and never repairs**: there is no `--fix`, the remedy is prose
or a command the user runs, and absent evidence is a `Skip`, never a `Fail`
(DIRASH-0022). The data comes from the daemon's self-report over the control
channel, so doctor does not sample devices itself.

### Permissions

The requirement is that `dirad` raises **no extra permission prompt**. It is an
unsandboxed per-user process (LaunchAgent, Windows user process, `systemd
--user`), so no entitlement applies. Status of each claim:

| Signal | Prompt or entitlement | Status |
|---|---|---|
| macOS device flag (`...IsRunningSomewhere`, audio and CMIO) | None. Reading it does not open a stream, so it is not "using the mic" | V that the header has no privacy text; no-TCC is R |
| macOS per-process audio properties | Two write-ups report no TCC activity. **Counter-evidence:** one issue (Minutes #1127, macOS 27.0.1) measured `coreaudiod` sending about 8 microphone and 5 screen-capture TCC *requests* per poll, but did not say whether a dialog appeared, and that app already held mic permission | **U that a clean account gets no dialog. Test this first** |
| macOS `CGWindowListCopyWindowInfo` | Apple DTS: it never fails or prompts without Screen Recording; it withholds private values such as `kCGWindowName` (V). A forum report says `kCGWindowOwnerName` is **nil** for other apps' windows without Screen Recording on macOS 26 (R) and DTS says which keys appear is "undocumented and may change" | Titles are never needed. Whether PID, layer and bounds survive is U, so C on macOS must self-check and report `Unavailable` if keys are missing |
| macOS window titles | Need Screen Recording | **Not used.** Never call `CGRequestScreenCaptureAccess()`; at most an optional tier gated on `CGPreflightScreenCaptureAccess()`, off by default |
| macOS `NSWorkspace`, libproc | None when unsandboxed | R |
| Windows ConsentStore (HKCU) | None, no elevation | R |
| Windows WASAPI session enumeration | None documented | V that Learn states none; behaviour with the mic privacy toggle off is U |
| Windows `EnumWindows` | Needs the interactive desktop, so a Session 0 service sees nothing; `dirad` is a user process | V |
| Windows 11 Insider 26340.9212 (Aug 2026) | Reported to add per-app consent prompts when Win32 apps use the mic | R, not in Microsoft's notes; re-test registry and session reads on it |
| Linux PipeWire | A same-user native client sees all nodes; restricted portal clients see less | V |
| Linux X11 | `DISPLAY` required | V |

Mitigation for the macOS risk: poll the cheap device-level flag first and walk
process objects only when an input is active, backing off to 5 s, as the
affected project did. If the validation spike shows a prompt on a clean
account, the macOS per-process path ships disabled and A degrades to the device
flag plus D.

A prompt that turns out to be unavoidable is called out in `dira doctor` as a
`Warn` naming the system setting. `dirad` never requests it.

### Contract proposal (prose only)

Proposal for the producer contract; **no contract code changes in this PR**.

An `Interval` whose `activity` is `"meeting"` may carry one additive optional
field, `meeting`, an object with exactly three closed-enum fields:

- `providerClass`: `zoom | teams | slack | webex | meet | browser | other`.
  `meet` is only produced when the calendar layer proves it, since without
  titles a browser call is just `browser`.
- `confidence`: `high | low`. Only `high` and user-confirmed intervals exist on
  the wire; unconfirmed low runs are local suggestions and never leave the
  device.
- `source`: `capture | output | calendar | user_confirmed`, the basis of the
  decision (what A and B contributed, what the calendar layer added, or the
  human).

Why this holds D-0001:

- All three are small closed enums. There is no free-text field, so a title,
  attendee, meeting id, URL or app name has no place to ride. The adapter maps
  a process to a `ProviderClass` and discards the source string; a calendar
  event is mapped to a class from its conference type and discarded.
- The field names `meeting`, `provider_class`, `confidence` and `source` contain
  no `wire_contract_carries_no_content_fields` denylist token (`prompt`,
  `content`, `diff`, `body`, `text`, `patch`, `code`, `snippet`). `source`
  already coexists with `source_session`. The test's `Interval` fixture must
  populate the new field so the token walk covers it.
- Additive optional field, omitted when absent, so older payloads stay
  byte-identical: `SCHEMA_VERSION` 1.8.0 to 1.9.0, then `just contract`
  regenerates `attestation.schema.json` and the signing vector. They are never
  hand-edited.
- `SessionKind` is **not** extended: a new enum variant would break the
  cloud's validators for old devices. Detected meeting sessions use the
  `Manual` kind and `Harness::Manual`, and `Interval.meeting` is what tells the
  cloud they were detected. Whether a distinct kind is worth a coordinated
  cloud bump is an open question.

Repo attribution: a meeting interval has `repoCanonical` absent. Whether the
cloud nets meeting time against concurrent repo time is cloud policy
(capture stays policy-free); see Open Questions.

### Later layers

Both are **optional, separately opted in, and never required** for the base
layer.

**Calendar correlation (device-side, OAuth).** Corroborates and classifies; it
never creates time by itself (a calendar entry is not evidence a meeting
happened).

- Google: `calendar.events.readonly` (V; Sensitive scope, so a public app needs
  Google verification, V). Narrower `calendar.events.owned.readonly` (owned
  calendars only) also returns `conferenceData` (V by scope list). Free/busy
  scopes (`calendar.freebusy`, `calendar.events.freebusy`) return busy ranges
  only, **no conference data** (V); whether `events.list` under a freebusy
  scope returns anything useful is U. A `fields` mask requesting only
  `start,end,conferenceData.conferenceSolution.key.type` strips titles and
  attendees from the response, but it is a bandwidth filter, not a consent
  boundary: the consent screen still says "view events" and the UI must say so.
  `conferenceSolution.key.type` is `hangoutsMeet` or `addOn` (V). Zoom links
  pasted into the location or description do not show up there (U).
- Microsoft: Graph `Calendars.ReadBasic` (V; delegated only, no admin consent;
  reads events "except for properties such as body, attachments, and
  extensions", V; whether subject and attendees are excluded is not documented,
  U) with `$select=id,start,end,isOnlineMeeting,onlineMeetingProvider`
  (V), which strips them from the response (again a bandwidth filter).
  `onlineMeetingProvider` is `teamsForBusiness`, `skypeForBusiness`,
  `skypeForConsumer` or `unknown` (V); Zoom and Meet links live in the body or
  location, which `ReadBasic` withholds, so Microsoft correlation recognises
  Teams meetings only. `getSchedule` is not a substitute: its sample response
  carries subject and location (V).
- Tokens live in the OS keychain. Only start, end and a provider class are
  ever read into the daemon.

**Enterprise org-API reconcile (cloud-led, admin-granted).** Reconciles
durations after the fact. Device-side capture remains the source of truth for
*when the user was actually present*; org data only corrects start and end.

| Provider | Endpoint | Who grants | Notes |
|---|---|---|---|
| Teams | Graph `attendanceReports` with `$expand=attendanceRecords` | `OnlineMeetingArtifact.Read.All`. Delegated is organizer-only; application needs admin consent **plus a tenant PowerShell application access policy per user**, up to 30 min to apply | Channel meetings and breakouts not covered (V/R) |
| Meet | `conferenceRecords.participants.participantSessions` | `meetings.space.readonly` or `.created`, Sensitive (V) | User OAuth only; no app-only auth; participant data kept 30 days (V/R) |
| Zoom | `GET /report/meetings/{id}/participants` | `report:read:admin` (R) | Pro plan or higher; heavy rate limit; account admin approves the app; look-back 6 or 15 months, conflicting (R) |
| Slack | `user_huddle_changed` event, `users.profile.get` | `users:read`, `users.profile:read` (V) | Presence only: `huddle_state`, expiration, call id. No participants |
| Webex | meetings participants and compliance scopes | R | Plan and retention limits U |

**Terms: Google Meet API.** Google's Meet REST API overview states the API is
not intended for performance tracking or user evaluation within a domain and
that Meet data should not be collected for that purpose
(developers.google.com/workspace/meet/api/guides/overview, V). It is a
documentation note, and whether it binds contractually on its own is U. The
Workspace API User Data Policy (updated 2026-09-03, V) has no "performance"
wording but forbids monitoring Meet data "without legal authorization or
consent" and approves reporting "for the benefit of users". **Design rule:** the
Meet layer is framed and shipped as a user-owned self-report. It is never
surfaced in any manager-facing evaluation view, and the cloud must not roll
Meet-sourced durations into per-person performance metrics. The Microsoft,
Zoom and Slack terms checked (Microsoft APIs Terms of Use Oct 2025, Zoom App
Review Guidelines, Slack API ToS) contain no explicit employee-evaluation
clause but require minimum-necessary data and bar surveillance use (V); works
council and GDPR consent for employee monitoring in the EU is R from secondary
sources only. Legal review is needed before any employer-facing org-API
reconcile ships.

## Interfaces & data

### Module shape

One trait, per-OS adapters, in a **new crate** `cli/meetings` (`dira-meetings`).
It is deliberately not in `cli/sources`: that crate is pure, stateless
normalisation of harness payloads (`HarnessSource::normalize(Value)`) and
carries no OS dependencies. Meeting signals are stateful, polled and need
platform FFI (`objc2-core-audio`, `objc2-core-media-io`, `objc2-core-graphics`,
Windows APIs, `pipewire`/`x11rb`); putting those in `sources` would drag them
into every consumer, including the `dira` CLI.

```
contract/                      Interval.meeting (closed enums only)
cli/core/                      Store (suggestions table), accounting, events
cli/sources/                   harness hooks -> Normalized   (unchanged)
cli/meetings/    (new)
  src/lib.rs                   ProviderClass, Reading<T>, Sample, MeetingSignal
  src/classify.rs              process identity -> ProviderClass, then drop it
  src/fuse.rs                  pure: samples -> confidence; the table above
  src/tracker.rs               pure: debounce, grace, split, min length
  src/macos.rs   #[cfg(target_os = "macos")]
  src/windows.rs #[cfg(target_os = "windows")]
  src/linux.rs   #[cfg(target_os = "linux")]
  src/fake.rs                  replayable fixtures for tests
cli/dirad/src/meetings.rs      the poll task; owns the adapter
cli/dira/src/doctor/meetings.rs  reads the daemon's self-report
```

```rust
/// One OS adapter. `sample` is cheap, synchronous and side-effect free.
pub trait MeetingSignal: Send {
    fn id(&self) -> &'static str;              // "macos.coreaudio"
    fn sample(&mut self) -> Sample;            // A, B, C, D in one poll
}

pub enum Reading<T> { On(T), Off, Unavailable(Reason) }
pub struct Sample {
    pub at: std::time::Instant,                // monotonic
    pub capture: Reading<Vec<Holder>>,         // A: mic or camera
    pub output: Reading<Vec<Holder>>,          // B
    pub window: Reading<Vec<Holder>>,          // C
    pub process: Reading<Vec<Holder>>,         // D
}
pub struct Holder { pub class: ProviderClass, pub kind: HolderKind }
```

`fuse` and `tracker` are pure and cross-platform, so the fusion table,
debounce, grace, split and stuck-holder rules are unit-tested with fixtures on
any CI runner; only the three adapters need a real OS. A `fake` adapter replays
recorded `Sample` sequences.

Daemon side: `dirad/src/meetings.rs` runs a task next to `idle_ticker` under
the supervisor (restartable on panic, with a watchdog progress mark like the
ticker). It feeds the existing writer queue with
`ManualStart`/`ManualTick`/`ManualStop`-shaped events (`activity: "meeting"`,
no repo) under a daemon-minted session id with its own reserved prefix, so
accounting, rollups and sync reuse the existing path rather than a new one.
Suggestions are a local table (`meeting_suggestions`: id, started_at, ended_at,
provider_class, signals bitmask, state `pending|confirmed|dismissed`), times
stored in the fixed-width UTC form through `StoredAt` (DIRASH-0043),
pruned with the existing retention sweep. Suggestions never appear in an
attestation batch.

### Control channel

New requests: `MeetingsStatus` (feeds `dira status` and doctor),
`MeetingsList`, `MeetingsConfirm{id}`, `MeetingsDismiss{id}`. Responses carry
only provider class, times, signal flags and counts.

## Invariants

- Nothing content-bearing is read into stored, logged or sent data: no window
  title, calendar title, attendee, meeting id, URL, app window text, or raw
  process name (D-0001, DIRASH-0047).
- A low-confidence interval is never billed without an explicit confirmation.
- `Unavailable` never means "no meeting" and never lowers a standing meeting.
- `dirad` never requests a TCC/privacy permission for this feature, and never
  calls `CGRequestScreenCaptureAccess()`.
- `dira doctor` meeting checks report only; absent evidence is `Skip`
  (DIRASH-0022).
- The wire gains only closed enums, and the denylist test populates them.
- With the knob `off`, no adapter is constructed and no device is sampled.

## Validation before this becomes active

None of the muted-mic table has been reproduced first-hand. The spec stays
`proposed` until this matrix is run on a real machine per OS, with results
recorded here (and the provisional timings above tuned from it):

1. Zoom, Teams, Slack, Webex: join **muted, camera off**; then unmute; then mute
   again. Record A (device + per-process), B and C at each step.
2. Meet in Chrome, Edge, Firefox: same sequence; record input state and the tab
   indicator on mute.
3. Camera release on video-off, per app.
4. macOS clean-account TCC check for the per-process audio properties on
   current macOS (the Minutes #1127 counter-evidence).
5. Windows: ConsentStore `LastUsedTimeStop` semantics after an app crash and
   after sleep; re-test on the Insider build above.
6. Linux: everything. There is no evidence for any app.

A developer-facing `dira meetings trace` that prints per-signal states (class
level only) makes this repeatable by anyone.

## Open Questions

- **Muted-from-start native calls.** We assume B plus C plus D carries them at
  high confidence. If C is `Unavailable` (Wayland, macOS keys missing), these
  fall to low. Is that acceptable, or does native B plus D sustained for a
  longer window also qualify?
- **Slack huddles.** Unknown whether the input is released while muted, and a
  huddle window may be indistinguishable from the main Slack window by class
  (Electron). Likely low until measured.
- **Does the cloud net meeting time against concurrent repo time?** A
  meeting during a coding session is two overlapping human spans. Capture is
  policy-free; this is a cloud/billing rule, to be agreed with the sessions
  designing quality gates and EVM.
- **A distinct `SessionKind`?** Avoided here for compatibility; revisit with a
  coordinated cloud bump if the cloud needs session-level assurance.
- **Safari and other WebKit clients** share one GPU process and are
  unattributable; they are excluded. Is that acceptable for Mac users who use
  Safari for Meet?
- **macOS Sequoia's recurring screen-recording prompt.** No source ties it to
  metadata-only `CGWindowListCopyWindowInfo` calls (U).
- **Employer-facing use.** The Google Meet terms note and EU works-council rules
  mean any manager view of detected meeting time needs legal review and an
  explicit employee-visible disclosure. Out of scope here.
- Provisional thresholds (30 s start, 60 s grace, 2 min minimum, 3 h stuck
  holder, 5/10 s polling) are unvalidated.

## Sources

Full notes with every URL and tag are in the shared side-session notes folder
(`research-muted-mic.md`, `research-os-signals.md`,
`research-calendar-orgapi.md`). Primary sources read: the macOS SDK headers
(`AudioHardware.h`, `CMIOHardwareObject.h`), Microsoft Learn (WASAPI session
interfaces, Graph permissions and `attendanceReport`, Microsoft APIs Terms of
Use), PipeWire docs, wayland.app protocol registry, Google Meet REST API and
Workspace API User Data Policy, Mozilla WebRTC blog, Slack and Zoom developer
docs.
