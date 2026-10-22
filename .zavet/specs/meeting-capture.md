---
title: Meeting capture — local device-in-use signals, no bot
version: 2
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
drives the OS's own orange/green privacy indicators. `dirad` samples it, and
fuses evidence from up to three independent **families** into a confidence
level:

| Family | What it says | Needs | Default |
|---|---|---|---|
| **Device** (signals A to D) | a known call app holds a mic/camera/audio stream | nothing, runs in `dirad` | on with the module |
| **Presence** | a provider says the user is in a call, and/or the OS says whether the user is at the machine | optional provider token (provider presence); none for local idle/lock | off, separately opted in |
| **Calendar** | an accepted, online or in-person event was scheduled | optional calendar token | off, separately opted in |

Then:

- **high** confidence becomes meeting time, recorded as an `Interval` with
  `activity: "meeting"`;
- **low** confidence becomes a *suggestion* the user confirms or dismisses.
  It is never billed silently.

Device evidence alone is enough for high confidence on a native app, so the
base layer needs neither presence nor calendar. Calendar and presence do two
jobs: they **cross-check** the device signal (raise low to high, veto
implausible events), and, if the user opts in per calendar, they can be the
**main evidence** where devices are blind (a call joined muted from a browser,
an in-person meeting). Enterprise org-API reconcile remains a later,
admin-granted layer (see "Later layers").

Coverage is **every app that does calls**, not only the big four: a data-driven
catalogue of about 35 apps (Zoom, Teams, Slack, Discord, Webex, FaceTime,
Tuple, Telegram, WhatsApp, Signal, and more; see "App coverage").

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

### App coverage

Apps are not hard-coded. A versioned, embedded **catalogue**
(`cli/meetings/data/apps.toml`) maps process identity to a provider class and
a *kind*. Adding an app is a data change, not a code change, and a catalogue
entry can carry a note but never user data. Matching is by **prefix on the
bundle id / exe name / Flatpak id**, because the process that holds the mic is
often a helper, not the app (every fact below is R unless marked):

- Discord's call mic was held by `com.hnc.Discord.helper.Renderer` (measured
  2026-09-26), so an exact-id match missed the call.
- Slack huddle audio is in `com.tinyspeck.slackmacgap.helper`.
- FaceTime moves input and output to the system daemon
  `com.apple.avconferenced` once connected; iPhone-relay calls use
  `com.apple.TelephonyUtilities`.
- Prefix `com.microsoft.teams` covers new Teams (`teams2`, V) and its helpers;
  prefix `us.zoom.` covers Zoom and its helpers (`us.zoom.xos` is V).
- Safari and every WebKit client share `com.apple.WebKit.GPU`: **unattributable,
  excluded**.

**Kinds** drive how much corroboration a holder needs:

| Kind | Meaning | Rule |
|---|---|---|
| `platform` | a meeting product (Zoom, Teams, Webex, FaceTime, GoTo, RingCentral, Tuple, Jitsi, 8x8, Dialpad, Lark, DingTalk, Tencent Meeting, Gather) | a held mic is enough, once debounced |
| `messaging` | a chat app that also calls (Slack, Discord, Telegram, WhatsApp, Signal, Messenger, LINE, Viber, WeChat, KakaoTalk, Element) | also holds the mic for **voice notes**, so: longer confirm window (90 s, vs 30 s) and a held mic must be corroborated by output activity (B) or presence |
| `browser` | Chrome, Edge, Firefox, Brave, Arc and PWAs (`com.google.Chrome.app.*`, `com.microsoft.edgemac.app.*`) | duplex audio (A and B) only; Meet and Whereby are browser-only |
| `off_by_default` | probably not a work meeting: Mumble, TeamSpeak, Steam voice, Parsec, AnyDesk, TeamViewer, Dino, SIP softphones | listed so `doctor` can say "ignored"; user can enable |

**Do not add** (retired): Skype consumer (retired 2025-05-05, V), classic Teams
(unavailable from 2025-07-01, R), Amazon Chime (support ended 2026-02-20, V),
BlueJeans (wound down 2024, R) and Around (R). Keep an alias only if cheap.

**Never a meeting on their own** (the mic holder is not evidence of a call):

- *Deny list.* Dictation and voice typing (Superwhisper, Wispr Flow, MacWhisper,
  VoiceInk and about 15 more), Apple Voice Memos, Siri and the Sound settings
  pane (which held the mic for 18 h), recorders (Loom, OBS, Screen Studio, Cap,
  CleanShot), AI assistants with voice, launchers (Raycast) and other
  note-takers (Granola, Fathom, Krisp), who are *consumers* of the call, not
  evidence of it. Rule that worked in another detector: skip detection only
  when **every** mic holder is on the deny list, and fail open when the holder
  list is unknown. dirad's own pid is always dropped.
- *Relay list.* Krisp, NVIDIA Broadcast, Voicemod, BlackHole, Loopback, Elgato
  Wave Link and PipeWire filter-chains sit between the physical mic and the
  call app and can hold the physical device permanently (that they do is U).
  Never declare a call from the device-level flag alone; when the only holders
  are relays do not start; when a relay is co-active with a known call app,
  count the call app; treat an output device that is itself virtual as no
  evidence for B.

**Browser calls.** Without titles a browser holder is ambiguous (any website
can open a mic), so it is high only with duplex audio, or with calendar or
provider-presence corroboration (below). Title or URL matching is *not* used,
by DIRASH-0047: titles are content.

A small starter catalogue (full ids, Windows exe names, Flatpak ids and the
evidence behind each are in the shared notes `research-app-catalogue.md`;
**spot-check every id on a real machine before shipping**, the cask data was
read through a summarising fetch):

| Class | Kind | macOS prefix | Windows | Linux |
|---|---|---|---|---|
| `zoom` | platform | `us.zoom.` | `Zoom.exe`, `CptHost.exe`, `aomhost64.exe` (R) | Flatpak `us.zoom.Zoom` (V) |
| `teams` | platform | `com.microsoft.teams` | `ms-teams.exe`; package family `MSTeams_8wekyb3d8bbwe` (V) | no official client |
| `slack` | messaging | `com.tinyspeck.slackmacgap` (V) | `slack.exe` (R) | Flatpak `com.slack.Slack` (V) |
| `discord` | messaging | `com.hnc.Discord` (V) incl. `.helper.Renderer` | `Discord.exe` (R) | Flatpak `com.discordapp.Discord` (V) |
| `webex` | platform | `Cisco-Systems.Spark`, `com.cisco.webexmeetingsapp` (V) | `CiscoCollabHost.exe`, `webexhost.exe` (V) | no Flatpak found |
| `facetime` | platform | `com.apple.FaceTime`, `com.apple.avconferenced` (R) | n/a | n/a |
| `tuple` | platform | `app.tuple.app` (V) | U | U |
| `telegram` | messaging | `ru.keepcoder.Telegram`, `com.tdesktop.Telegram` (V, two builds) | `Telegram.exe` (R) | Flatpak `org.telegram.desktop` (V) |
| `whatsapp` | messaging | `net.whatsapp.WhatsApp` (V) | `WhatsApp.exe` (R) | unofficial only |
| `signal` | messaging | `org.whispersystems.signal-desktop` (V) | `Signal.exe` (R) | Flatpak `org.signal.Signal` (V) |
| `meet` | browser | PWA prefix `com.google.Chrome.app.` (ids conflict between sources, U) | browser exe | any browser |
| `jitsi`, `gotomeeting`, `ringcentral`, `8x8`, `dialpad`, `lark`, `dingtalk`, `tencent_meeting`, `gather`, `viber`, `wechat`, `element`, `line`, `messenger`, `kakaotalk` | per catalogue | see notes | see notes | see notes |

Known id conflicts to settle on a real Mac (`defaults read
/Applications/X.app/Contents/Info CFBundleIdentifier`): Slack has
`com.tinyspeck.slackmacgap` (V) and an unsourced `com.slack.Slack`; Discord's
`com.discordapp.Discord` is U (likely the Flatpak id leaking in).

Wire note: the *local* catalogue has fine-grained classes, but the *wire*
`providerClass` is a smaller closed enum (see "Contract proposal"), so a new
catalogue entry never forces a cloud change.

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

For the apps added in "App coverage" (Discord, Telegram, WhatsApp, Signal,
Tuple, GoTo and the rest) there is **essentially no muted-mic evidence**. The
only item is Discord on iOS, where the dot stays on while muted in a call and
support called it expected (R, iOS only). Treat them as "likely hold while
connected, unverified", which is a second reason `messaging` apps need
corroboration and the validation matrix must cover them.

### Fusion into a confidence level

Evidence has three families (Overview). Within a sample every signal is
`On`, `Off` or `Unavailable`, and the holder carries a provider class and kind.

**Device family (A to D).**

| Rule | Confidence |
|---|---|
| `platform`: A on for a meeting process | **high** |
| `platform`: B on and C on and D on (listen-only, joined muted) | **high** |
| `messaging`: A on and B on (voice notes excluded), longer confirm | **high** |
| `browser`: A on **and** B on in the same browser (duplex audio), sustained | **high** |
| `platform`: B on and D on, no C | low |
| `messaging`: A on only; `browser`: A only, B only, or camera only | low |
| Only D on, or only C on | not a meeting (no interval, no suggestion) |

**Presence family.** Two different things, kept apart:

- *Provider call-state* (P): the provider says the user is in a call. Only
  some states are real attendance: Teams `InACall` (Graph presence), Slack
  `huddle_state = in_a_huddle`, Zoom "In a Zoom meeting" / "On a call", Webex
  `call`. **Many "in a meeting" states are calendar echoes, not attendance**:
  Teams `InAMeeting` is "automatically reflected during scheduled calendar
  meeting events" (V), Slack's calendar integration sets "In a meeting" only
  for events you accepted (V), and Zoom lists "In a calendar event"
  separately (R). Echoes are therefore classified as **calendar** evidence and
  never double-counted as presence. P has minutes of latency (the Teams client
  polls; changes lag a few minutes, R), so it can corroborate a run but never
  places its edges.
- *Local presence* (L): whether the human is at the machine, as `idle seconds`
  and `locked` tri-states. **L never vetoes device evidence**: a person
  listening to a call touches no input and may have locked nothing, so
  idle-means-away would delete real meetings (this is exactly how ActivityWatch's
  AFK logic would behave, and why we do not copy it). L is consulted only for
  calendar-only evidence (below).

**Calendar family (K).** An *eligible* event is: not cancelled, the user's own
RSVP not `declined`, `tentative` or `needsAction`, shown busy (not free/OOO),
not all-day, not a focus-time / out-of-office / working-location type, and
with another invitee or a conference link (a solo block is a personal
reminder, not a meeting). Eligible events are `remote` (has a conference link
or online-meeting flag) or `in_person` (none).

**Combined ladder**, strongest first:

| Tier | Evidence | Result | Source on the wire |
|---|---|---|---|
| 1 | Device high | **high**, recorded (auto mode) | `device` |
| 2 | Device low, plus a corroborating P or an eligible overlapping K | **high**, recorded | `device` (device bounds win) |
| 3 | Eligible `remote` K, no device or P, L shows the user at the machine at some point in the window (unlocked and input within it) | **low**, suggestion | `calendar` |
| 4 | Eligible `remote` K, no device or P, L shows locked or idle throughout | not a meeting; listed only as "skipped, you looked away" in `dira meetings` | n/a |
| 5 | Eligible `in_person` K | **low**, suggestion; never auto-recorded (a skipped meeting and an in-person one look the same to a laptop). Machine locked or idle through the window is mild support; active coding during it contradicts | `calendar` |
| 6 | **`calendar = "primary"`** (per-calendar opt-in): tier 3 events are promoted to **high** and recorded | **high** | `calendar` |

**Contradiction (downgrade).** dira already holds the user's own work
timeline. Heavy human-engaged repo time (prompts, edits) during the *entire*
window of a calendar-only event lowers it one tier (primary mode records it as
a suggestion instead). It never applies to tier 1 or 2, where the device says
the call was live.

**Bounds.** Device bounds always override scheduled bounds: a meeting that ran
10 minutes short or long is recorded as the device saw it. Calendar-only time
(tier 6) is clipped to the span where L shows presence, never the full
scheduled slot; the calendar APIs do not expose ran-short or no-show
meetings, so only a device edge or an attendance report can correct those.

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
- **Calendar and presence are separate opt-ins**, each with its own prompt
  naming exactly what is read and what is not (DIRASH-0030: never bundled with
  the device step, with device linking or with billing consent):
  - `[meetings] calendar = "off" | "crosscheck" | "primary"`, default `off`.
    `crosscheck` uses calendar only to raise device-low runs and to suggest.
    `primary` additionally records tier-3 events (above) as high; it is chosen
    **per calendar** (a work calendar yes, a personal one no) and needs
    `modules.meetings = "auto"`.
  - `[meetings] presence = "off" | "on"`, default `off`. `on` enables provider
    call-state (one token per provider the user connects).
  - Local presence (idle seconds, locked) is sampled **only while a calendar
    mode is on**, because the device family never needs it.
  - `dira meetings connect <google|microsoft|teams|slack|zoom|webex>` runs the
    provider's own OAuth flow; tokens live in the OS keychain; `dira meetings
    disconnect <provider>` revokes and deletes. Both are settable and listed in
    `dira status`, so consent is never only a prompt.

### Surfacing

`dira status` gains one line, hidden when the module is off:

```
meetings  auto · idle                       # nothing detected
meetings  auto · in a meeting (zoom, high, 12m)
meetings  auto · 2 suggestions to review    # run: dira meetings
meetings  auto · degraded: window signal unavailable (wayland)
meetings  auto · calendar crosscheck (google) · presence: teams
```

`dira meetings` lists pending suggestions (time range, provider class, which
evidence fired: device / presence / calendar / local presence, never a name);
`dira meetings confirm <id>` records one, `dira meetings dismiss <id>` drops
it, `dira meetings confirm --all-in-person` batches the tier-5 events.
Suggestions expire after 14 days. Confirmed time is recorded with
`source: user_confirmed`. Calendar-only events skipped because the user looked
away (tier 4) appear in a separate list so nothing is silently hidden.

`dira doctor` gains `meetings.*` checks (ids added to `CHECK_IDS`, additive per
DIRASH-0022):

| id | Ok | Skip | Warn |
|---|---|---|---|
| `meetings.module` | module on | module off, or OS unsupported | n/a |
| `meetings.adapter` | adapter constructed, signals listed per `On/Off/Unavailable` | daemon unreachable (no evidence) | n/a |
| `meetings.signals` | every signal available | evidence absent (no audio stack, no `DISPLAY`, headless, pre-macOS-14) | a signal is `Unavailable` where it normally exists (for example a TCC denial) |
| `meetings.stuck_holder` | none | no samples yet | a device held over 3 h with no output |
| `meetings.catalogue` | catalogue loaded; lists classes ignored via deny/relay/`off_by_default` (class names only) | module off | n/a |
| `meetings.calendar` | connected, last poll within 2 h | calendar mode `off`, or daemon unreachable | token expired or revoked (remedy: `dira meetings connect <provider>`); calendar returned no events for 7 days |
| `meetings.presence` | provider connected and answering | presence `off` | token expired; provider returns presence as hidden (Webex omits status when sharing is off) |
| `meetings.local_presence` | idle and lock readable | calendar mode `off`, or evidence absent (no logind hints, no `DISPLAY`) | n/a |

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
| **Local presence, macOS** idle | `CGEventSourceSecondsSinceLastEventType(HIDSystemState, kCGAnyInputEventType)` returns a duration only. No Apple source says it needs Input Monitoring (only event *taps* prompt, R) | V that the call exists in the SDK header; no-prompt on a clean account is **U, test** |
| Local presence, macOS lock | **No supported API.** `CGSSessionScreenIsLocked` is not in the SDK headers; Apple DTS says lock notifications are not public. Report lock as `Unavailable` and use idle only | V |
| Local presence, Windows | `GetLastInputInfo` (per-session, not guaranteed monotonic); `WTSRegisterSessionNotification` for lock/unlock needs a message-only window in `dirad`; no permission documented | V |
| Local presence, Linux | logind `IdleHint` / `LockedHint` are set by the desktop, so absence means *unknown*; `ext-idle-notify-v1` on KWin, Sway, Hyprland, niri, COSMIC but **not Mutter**; GNOME `Mutter.IdleMonitor` on the session bus (blocked for Snap, R); X11 `XScreenSaverQueryInfo` | V/R |
| Provider presence and calendar | An OAuth consent screen at connect time, run by the user via `dira meetings connect`. **This is the one unavoidable prompt in the whole feature, and it is explicit, opt-in and provider-owned**, never an OS permission | by design |

No local-presence call reads keystroke or pointer content; each returns a
duration or a lock bit. ActivityWatch's Linux AFK watcher counts raw
keyboard/mouse listener events, which is more invasive, and we deliberately
do not copy it.

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

- `providerClass`: `zoom | teams | slack | webex | meet | discord | facetime |
  chat | browser | in_person | other`. This is deliberately **coarser than the
  local catalogue**: the catalogue's ~35 apps fold into these (Telegram,
  WhatsApp, Signal, Element and similar become `chat`; Tuple, GoTo, Jitsi and
  the long tail become `other`), so adding an app never changes the wire.
  `meet` is only produced when the calendar layer proves it, since without
  titles a browser call is just `browser`. `in_person` comes only from
  calendar evidence. Cloud validators must treat an unknown value as `other`;
  that tolerance is a cloud issue to land before the first extension of this
  enum.
- `confidence`: `high | low`. Only `high` and user-confirmed intervals exist on
  the wire; unconfirmed low runs are local suggestions and never leave the
  device.
- `source`: `device | calendar | presence | user_confirmed`, the family that
  carried the decision (`device` when the device family set the bounds even if
  calendar or presence corroborated; `presence` when provider call-state did;
  `calendar` for tier 6; or the human).

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

### Calendar and presence providers

Both are **optional, separately opted in, and never required** for the base
layer. They run device-side, with a user-granted token, inside `dirad`.
Provider push (Graph webhooks, Google Calendar `watch`) needs a public HTTPS
endpoint (V), which a local daemon does not have, so everything here is
**polling**.

**Calendar (family K).** A cross-check by default (`crosscheck`), main
evidence only where the user chooses `primary` per calendar. A calendar entry
is not itself evidence a meeting happened, which is why tiers 3 and 5 are
suggestions and tier 6 is gated by local presence.

- Google: `calendar.events.readonly` (V; Sensitive scope, so a public app needs
  Google verification, V). Narrower `calendar.events.owned.readonly` (owned
  calendars only) also returns `conferenceData` (V by scope list). Free/busy
  scopes (`calendar.freebusy`, `calendar.events.freebusy`) return busy ranges
  only, **no conference data** (V); whether `events.list` under a freebusy
  scope returns anything useful is U. A `fields` mask requesting only
  `start,end,status,transparency,eventType,conferenceData.conferenceSolution.key.type`
  plus the eligibility fields below strips titles and attendees from the
  response (selecting only the self attendee's `responseStatus` through a
  nested `fields` mask is U; the fallback is `maxAttendees=1`), but it is a bandwidth filter, not a consent
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
- Tokens live in the OS keychain. Only start, end, an eligibility verdict and
  a provider class are ever kept; everything else is dropped at the client.

*Eligibility fields* (used to compute the verdict, then discarded):

| Exclude when | Google Calendar | Microsoft Graph |
|---|---|---|
| declined, tentative or unanswered | self attendee `responseStatus` (`maxAttendees=1` still returns your own row, V; with over 200 guests RSVP is not propagated, V) | `responseStatus.response` |
| cancelled | `status = cancelled` | `isCancelled` |
| free / out of office | `transparency = transparent`; `eventType` focusTime, outOfOffice, workingLocation | `showAs` free, oof, workingElsewhere |
| all-day | `start.date` | `isAllDay` |
| personal block, no other invitee | `organizer.self` and no attendees | `isOrganizer` and no attendees |
| remote vs in-person | `conferenceData` or `hangoutLink` | `isOnlineMeeting` + `onlineMeetingProvider` (Teams only) |

**Graph caveat (U):** `Calendars.ReadBasic` documents what it *excludes* (body,
attachments, extensions) but not what it returns. Whether `responseStatus`,
`showAs` and `isOnlineMeeting` are present needs a live test; if they are not,
the filter needs `Calendars.Read`, a larger consent, and the spec must say so
in the prompt. Recurring events: Google `singleEvents=true`; Graph
`calendarView` returns occurrences and exceptions, not series masters (V).
Sync: windowed polling (about -1 to +2 days) diffed by `id` + `updated`,
because Google `syncToken` cannot be combined with `timeMin/timeMax` (V) and
Graph `calendarView/delta` needs a fixed window and does not support
`$select` (V). Google `reader` delegates see private events with details
hidden (V); which fields survive is U.

**Provider presence (family P).** Only the call-state values count; calendar
echoes are filtered out (see Fusion).

| Provider | Call-state signal | Access | Status |
|---|---|---|---|
| Teams | Graph `GET /me/presence`, `activity = InACall` (`InAMeeting` is calendar-derived) | `Presence.Read`, delegated work/school only (no personal MSA). Poll limit 1,500 req / 30 s / app / tenant; the Teams client lags a few minutes. Admin consent for `Presence.Read`: older Q&A says required, **test per tenant** | V/R |
| Slack | `huddle_state = in_a_huddle` via `users.profile.get` (`users.profile:read`) or `user_huddle_changed` (`users:read`); `users.getPresence` is only active/away | `huddle_state` is missing from the field table, so the contract is soft | V/U |
| Zoom | presence "In a Zoom meeting" / "On a call" | `user:read:presence_status` or `user:read`; docs lag behaviour, an undocumented `In_Meeting` value was confirmed by staff | R |
| Webex | `GET /people/me` `status`: `call`, `meeting`, `presenting` | `spark:people_read`; omitted if the user's status sharing is off; same-org only; enum from an SDK mirror | V (mirror) |
| Discord | local RPC voice state | `rpc.voice.read` is partner-only | **not viable** (V) |
| Google (Meet) | none. Chat `users.availability` exists but only ACTIVE/IDLE/AWAY/DND, own user | | V |
| GoTo, RingCentral | nothing usable found | | U |

Verdict: provider presence is a per-vendor, minutes-latency classifier. It is
a good **cross-check** and a poor **main evidence**: that is why it can lift a
device-low run to high but never creates an interval by itself.

**Prior art on posture.** Calendar-as-suggestion is the norm (Harvest and
Timely prefill from an event, RescueTime reports meetings). Toggl has an opt-in
that auto-converts events, with no RSVP filter mentioned. Rize is closest to
this design: it detects live Meet/Teams calls on the device and logs them
without a calendar entry, then relabels from the calendar. Auto-logging from
the calendar alone is opt-in everywhere it exists, which is why `primary` is
per-calendar opt-in here and the default posture is `crosscheck`.

### Later layers

Optional and never required for the base layer.

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
  data/apps.toml               the catalogue: prefixes -> class + kind; deny, relay
  src/catalog.rs               loads the catalogue; process identity -> Holder,
                               then drops the source string
  src/fuse.rs                  pure: device + presence + calendar -> tier
  src/eligible.rs              pure: event facts -> eligible? remote/in_person
  src/tracker.rs               pure: debounce, grace, split, min length
  src/macos.rs   #[cfg(target_os = "macos")]
  src/windows.rs #[cfg(target_os = "windows")]
  src/linux.rs   #[cfg(target_os = "linux")]
  src/fake.rs                  replayable fixtures for tests
cli/dirad/src/meetings.rs      the poll task; owns the adapter
cli/dirad/src/meetings/        provider clients (HTTP via dira_core::httpclient,
                               tokens via the keychain): google, microsoft,
                               slack, zoom, webex. Each yields facts, not text
cli/dira/src/doctor/meetings.rs  reads the daemon's self-report
```

Two seams. `MeetingSignal` is the **local, synchronous, permission-free** seam
(device family plus local presence). The provider clients sit behind two small
async traits, `PlanSource` (events -> `EventFacts`, the eligibility inputs
only) and `PresenceSource` (call-state -> `CallState`), so the pure fuser can be
tested with fixtures and a provider can be added without touching fusion.
Provider clients convert a response straight into `EventFacts` / `CallState`
and drop the response: no title, attendee or id reaches a type that can be
stored or logged.

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
    pub idle: Reading<std::time::Duration>,    // local presence; sampled only
    pub locked: Reading<bool>,                 //   while a calendar mode is on
}
pub struct Holder { pub class: LocalClass, pub kind: AppKind, pub role: HolderRole }
pub enum AppKind { Platform, Messaging, Browser, OffByDefault }
pub enum HolderRole { Call, Deny, Relay }      // Deny/Relay holders never start a meeting
```

`LocalClass` is the catalogue's fine-grained class; `fuse` maps it to the
coarse wire `ProviderClass` only when it emits an interval.

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
`MeetingsList`, `MeetingsConfirm{id}`, `MeetingsDismiss{id}`,
`MeetingsReload` (after `connect`/`disconnect`). OAuth runs in the `dira` CLI
process (loopback redirect), which writes the token to the keychain and then
asks the daemon to reload; the daemon never opens a browser. Responses carry
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
- Local presence (idle, lock) **never vetoes device evidence**; it only
  qualifies calendar-only evidence.
- Calendar-only and in-person evidence is never recorded without either an
  explicit per-calendar `primary` opt-in (remote events only) or a user
  confirmation. In-person events are never auto-recorded.
- Calendar echoes of presence (Teams `InAMeeting`, Slack calendar status, Zoom
  "In a calendar event") are calendar evidence, never presence evidence, so
  nothing is counted twice.
- Device bounds override scheduled bounds; calendar-only time is clipped to
  observed local presence.
- Provider and calendar tokens live in the OS keychain, are revocable with
  `dira meetings disconnect`, and are shown in `dira status`.
- A Google Meet-sourced duration is never surfaced in a manager-facing
  evaluation view (see Terms).

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
7. The added apps (Discord, Telegram, WhatsApp, Signal, Tuple, GoTo, Telegram's
   two builds): `defaults read ... CFBundleIdentifier` per app, the helper ids
   that actually hold input (`kAudioProcessPropertyIsRunningInput`), join
   muted, and the Windows ConsentStore keys and exe paths during a real call.
8. Relays: with Krisp / NVIDIA Broadcast running and no call, which process and
   device read as "running"?
9. Graph `Calendars.ReadBasic`: does it return `responseStatus`, `showAs`,
   `isOnlineMeeting`? Slack `huddle_state` self-read; Zoom presence scope;
   Teams `Presence.Read` admin-consent behaviour per tenant.
10. Local presence: macOS idle call on a clean account (any prompt?), Windows
    `SessionFlags` lock semantics, Linux `LockedHint` population per desktop.

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
- **Should `calendar = "primary"` exist at all?** It is the user's call: it
  buys coverage of calls joined muted from a browser, at the cost of recording
  scheduled-but-skipped meetings when the user happened to be at the machine.
  The spec offers it per calendar, remote events only, clipped to observed
  presence, and still never for in-person. Does that line sit right, or should
  `primary` stay out of the first release?
- **In-person meetings.** A laptop cannot tell a skipped meeting from an
  in-person one, so they are suggestions. Is a one-tap "confirm all in-person"
  batch enough, or should an accepted organiser-owned event with other invitees
  be recorded by default?
- **Counting attendees** to tell a personal block from a meeting needs the
  attendee *count*, not identities. With Google `maxAttendees=1` the count is
  hidden; `attendeesOmitted` only says "more exist". Whether that is enough is
  U.
- **Messaging apps.** The 90 s confirm window and the A-and-B requirement are a
  guess to keep voice notes out. Needs the validation matrix.
- **Wire enum tolerance.** `providerClass` is closed but coarse; the cloud must
  map unknown values to `other` before the first extension.
- **Local presence on macOS lock.** No supported API; idle only. Acceptable?
- Provisional thresholds (30 s start, 90 s for messaging, 60 s grace, 2 min
  minimum, 3 h stuck holder, 5/10 s polling, 14 d suggestion expiry) are
  unvalidated.

## Sources

Full notes with every URL and tag are in the shared side-session notes folder
(`research-muted-mic.md`, `research-os-signals.md`,
`research-calendar-orgapi.md`, `research-app-catalogue.md`,
`research-presence-calendar.md`). Primary sources read: the macOS SDK headers
(`AudioHardware.h`, `CMIOHardwareObject.h`), Microsoft Learn (WASAPI session
interfaces, Graph permissions and `attendanceReport`, Microsoft APIs Terms of
Use), PipeWire docs, wayland.app protocol registry, Google Meet REST API and
Workspace API User Data Policy, Mozilla WebRTC blog, Slack and Zoom developer
docs.
