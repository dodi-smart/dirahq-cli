# Meeting tracking without a bot: design

Date: 2026-11-09. Status: **proposed**. Nothing is implemented. Phased plan:
[`../plans/2026-11-09-meeting-tracking.md`](../plans/2026-11-09-meeting-tracking.md).

## Goal

dira tracks meeting time automatically, as a capture source next to AI-harness sessions, with
**no bot in the call** and **nothing content-bearing on the attestation wire** (D-0001). It
works on macOS, Windows and Linux, without admin rights and without asking for Accessibility,
Screen Recording, microphone or camera permission. Detection is cheap: it is driven by OS
notifications, and it does not poll steadily while no call is happening.

Detected meeting time must:

- count as human-engaged time once and only once, next to work in repos and agent time;
- be billable per client policy;
- feed the cloud's meeting-load, focus-time and meeting-cost views per lens, plus MCP metrics;
- stay under the control of the person being measured, in a form a European employer can deploy
  (GDPR, works council).

## Relation to earlier work

An earlier design exists on the unmerged branch `origin/docs/meeting-capture-design` (three
commits, 2026-10-22): `.zavet/specs/meeting-capture.md` v2 and a decision record that was numbered
`DIRASH-0047`. That number now belongs to the headless decision on `develop`. The cloud lab
prototype `cloud/src/lib/lab/meeting-signals.ts` (`LabMeeting`) is built from that spec.

**This design builds on that spec and does not replace it.** It keeps that spec's research and
detection rules:

- the four device signals A–D for each OS;
- a data-driven catalogue of about 35 call apps, with deny and relay lists;
- the muted-mic and camera-off evidence table;
- the fusion tiers;
- the calendar eligibility filter;
- the permissions matrix;
- the `dira doctor` checks;
- the validation matrix.

Those parts are summarised here, not repeated. Read them in the branch spec
(`git show origin/docs/meeting-capture-design:.zavet/specs/meeting-capture.md`).

What this design adds or changes:

| Area | Branch spec (2026-10-22) | This design |
|---|---|---|
| Sampling | polling every 5/10 s, listeners "not relied on" | OS-notification **gate** plus a 30 s process scan; the per-process walk runs only while a call is a candidate (§1.2) |
| Accounting | synthetic `ManualTick` events, overlap left as an open question | spans plus a pure `overlay` in `accounting.rs`, workspace precedence resolved per user, computed once on the device (§2) |
| Wire | optional `Interval.meeting {providerClass, confidence, source}` in 1.9 | a `meetings[]` record native to 2.0 plus `Interval.meetingId`; the same shape ships as an additive 1.9 fallback (§3) |
| Calendar | device-side token, start/end/eligibility only | adds the attendee bucket, recurring flag, HMAC series key, `external` flag, and attribution to client/project resolved locally (§4) |
| Titles | never | never on the attestation wire; an optional, separately consented labels channel (§3.3) |
| Cloud | out of scope | storage, billing rule, lens tiles, `facts_daily` columns, MCP metrics (§5) |
| Enterprise | open question | workspace policy the daemon obeys, the GDPR posture, a works-council checklist (§6) |
| Decision record | `DIRASH-0047` (collides) | renumbered and split into proposed records M1–M7 (§8) |

## Decisions made with the owner (2026-11-09)

1. **Overlap precedence is a workspace policy.** The default is *meeting wins*. A workspace can
   switch to *work wins*. §2 explains why precedence still has to be resolved per user.
2. **Titles never ride the attestation wire.** A user who opts in sends them over their own
   consent-gated channel, in DIRASH-0030 style.
3. **The calendar connects on the device.** The user pastes a secret ICS URL, or connects
   Google or Microsoft through OAuth on the device. Raw events never leave the machine.
4. **The record is native to contract 2.0, with a 1.x fallback.** If detection is ready before
   2.0 ships, the same record ships as an additive optional array in 1.9.

---

## 1. Local detection in `dirad`

### 1.1 Module shape

Detection lives in a new crate, `cli/meetings` (`dira-meetings`). It is not part of `cli/sources`,
which normalizes hook payloads, is pure and carries no OS dependencies. Detection is stateful and
needs platform FFI. The branch spec's layout stands:

```
cli/meetings/
  data/apps.toml       catalogue: id prefixes -> local class + kind (platform|messaging|browser|off_by_default),
                       role (call|deny|relay); data, not code
  src/lib.rs           MeetingSignal, Reading<T> (On|Off|Unavailable), Sample, Holder, ProviderClass
  src/catalog.rs       process identity -> Holder; drops the bundle id / exe path at this boundary
  src/gate.rs          NEW: per-OS event sources -> a Wake channel (see 1.2)
  src/fuse.rs          pure: device (+ presence, + calendar) -> tier
  src/tracker.rs       pure: debounce, grace, split, minimum length, stuck holder
  src/eligible.rs      pure: calendar event facts -> eligible? remote|in_person
  src/macos.rs | windows.rs | linux.rs    #[cfg(target_os)]
  src/fake.rs          replayable Sample fixtures
cli/dirad/src/meetings.rs   the task; owns gate + adapter + tracker; runs under the supervisor
                            next to idle_ticker (dirad/src/lib.rs:348) with a watchdog mark
```

Process identity, meaning the bundle id, exe name, path or `WM_CLASS`, becomes a closed local class
at the adapter boundary, and the string is dropped there. Nothing downstream ever sees it: not the
store, logs, `dira doctor --json` or the wire.

### 1.2 Event-driven gate (new)

The branch spec polled every 5–10 s. This design waits for the cheap device-level flags to change
and walks per-process state only while a call is a *candidate*.

| | Gate: mic / camera (event-driven) | Output and per-process sessions (only while a candidate exists) | Process scan (30 s, always while the module is on) |
|---|---|---|---|
| **macOS** | `AudioObjectAddPropertyListenerBlock` on `kAudioDevicePropertyDeviceIsRunningSomewhere` for every input device, plus a listener on the device list so a newly plugged device gets one. `CMIOObjectAddPropertyListenerBlock` on `kCMIODevicePropertyDeviceIsRunningSomewhere` for cameras. This is OverSight's pattern; the block runs on a private dispatch queue, so no run loop is needed. | `kAudioHardwarePropertyProcessObjectList` → `kAudioProcessPropertyIsRunningInput` / `IsRunningOutput` / `PID` (macOS 14.2+, the same floor Chromium and Granola use), walked every 5 s | `proc_listpids` + `proc_pidpath`, matched against catalogue prefixes. Zoom's `CptHost` exists **only during a meeting** |
| **Windows** | `RegNotifyChangeKeyValue(bWatchSubtree, REG_NOTIFY_CHANGE_LAST_SET)` on `HKCU\…\CapabilityAccessManager\ConsentStore\{microphone,webcam}`, re-armed after every signal (the notification does not say what changed, so rescan) | WASAPI `IAudioSessionManager2::RegisterSessionNotification` plus `IAudioSessionEvents::OnStateChanged` on capture and render endpoints; `GetProcessId` | `CreateToolhelp32Snapshot`; Zoom `CptService.exe` (not used under the VDI plugin, where it runs permanently) |
| **Linux** | A long-lived `pw-dump --monitor` child parsed as streaming JSON. This keeps static musl: no libpipewire link. Falls back to `pactl -f json subscribe` + `list source-outputs`. inotify `IN_OPEN`/`IN_CLOSE` on `/dev/video*` wakes a `/proc/*/fd` camera check | the same PipeWire stream (`Stream/Input/Audio`, `Stream/Output/Audio`, node `state`) | `/proc/*/exe`, `comm` |

**Candidate rule.** A call is a candidate when either of these holds:

- the gate reports a device running;
- the process scan finds a `platform` call app with a call-only process. That process is Zoom's
  `CptHost` or `CptService`, or a process listed as `call_process` in the catalogue.

The second case catches a call joined muted with the camera off, which the gate cannot see. While
a candidate exists, the adapter samples A–D every 5 s. Once the candidate has been gone for its
grace period, the adapter stops walking and waits on the gate again.

**What the steady state costs:**

- no wake-ups except the 30 s process scan;
- a resident-memory budget of under 5 MB for the crate.

Both numbers are acceptance criteria for P0.

**Listeners are a wake-up hint, never the only source of truth:**

- Per-process listeners were reported not to fire on macOS 15 and 26, so per-process state is
  always *walked*, never subscribed.
- The 30 s scan also re-reads the device flags. A missed notification therefore costs at most
  30 s of start latency, and the start is back-dated to the first sample, so no time is lost.

**Known gaps:**

- **Bluetooth mics on macOS** report "not running" (an open Apple bug). Signals B, C and D carry
  those calls.
- **Safari and every WebKit client** share `com.apple.WebKit.GPU`, so a Safari Meet call cannot
  be attributed and is excluded. This is unchanged from the branch spec.
- **Windows `LastUsedTimeStop == 0`** can be stale after a crash or sleep. Require a running
  process for that exe, and one "not in use" reading since the daemon started, before trusting it.

### 1.3 Confidence, debounce, idle

The branch spec's fusion tiers and state machine stand. Its thresholds are still provisional and
P0 settles them.

| Evidence (device family) | Confidence |
|---|---|
| `platform` app holds the mic (A) | high |
| `platform` app: output (B), call window class (C) and process (D) | high (joined muted, listen-only) |
| **new:** `platform` call-only process (`CptHost`/`CptService`) plus output (B) | high |
| `messaging` app: A and B, 90 s confirm (excludes voice notes) | high |
| `browser`: A and B in the same browser, sustained | high |
| `platform` B+D without C; `messaging` A only; `browser` single signal | low → local suggestion |
| D only or C only | nothing |

- **Start.** Confidence must hold for 30 s; the start is then back-dated to the first sample.
- **Continue.** A or B persists for the same provider class.
- **End.** After 60 s of grace. A sleep, a lid close or a jump in the monotonic clock ends the
  meeting at the last sample before the gap.
- **Minimum length.** 2 min.
- **Mixed confidence.** A run whose confidence changes is split: the high span becomes meeting
  time and the low span becomes a suggestion.
- **Stuck holder.** A device held for more than 3 h with no output activity is downgraded to a
  suggestion and reported by `meetings.stuck_holder` in doctor.
- **Duplicate suppression (new).** No detection runs while a manual session with
  `activity = "meeting"` is live (`dira start --activity meeting`). The human already said so.

**Idle and away.**

- **Idle never cancels device evidence.** A person listening to a call touches nothing. Treating
  idle as away would delete real meetings; this is ActivityWatch's failure mode.
- **Idle is sampled only** for calendar-only evidence and the stuck-holder rule. It uses
  `HIDIdleTime` (IOKit) on macOS, `GetLastInputInfo` plus `WTSRegisterSessionNotification` for
  lock on Windows, and `ext-idle-notify-v1` (event-driven) or logind `IdleHint` on Linux.
- **macOS lock has no public API.** It is reported as `Unavailable`.
- **No-prompt checks are pending.** Whether the idle readings and the per-process CoreAudio walk
  run without a prompt on a clean account is checked in P0. A signal that turns out to need a
  prompt is disabled and reported as `skip`. It is never prompted for.

### 1.4 Local surfaces

These are unchanged from the branch spec:

- `dira status` gets one line about meetings.
- `dira meetings` lists suggestions; `dira meetings confirm|dismiss|trace` acts on them.
- `dira doctor` gets `meetings.*` checks that report and never repair, with absent evidence
  reported as `skip` (DIRASH-0022).

New in this design:

- `dira meetings assign <id> <project>` attributes a meeting.
- `dira meetings export|forget` covers GDPR access and erasure (§6).

---

## 2. Accounting: meetings, work and agent time

This section went through the `accounting-reviewer` agent, which traced five interleavings and
reported findings H1–H4 and M1–M5. They are folded in below. The bugs it found in today's code are
listed in §9.

### 2.1 The problem

`accounting.rs` counts human time as the idle-trimmed gaps between human signals, merged across
every session. No wall-clock minute counts twice (`accounting.rs:1-18`). A person listening in a
call sends no signals. A meeting therefore has to enter accounting as something other than a
signal, and it overlaps whatever the person does during the call: prompting an agent, a manual
session, or work routed to another workspace.

### 2.2 Model

Meetings are **spans**: `[start, end)` with a meeting key. They live in a new local `meetings`
table (migration `0011_meetings.sql`, times in the `StoredAt` form per DIRASH-0043), and each row
has a monotonic `version`.

Meetings are not synthetic `ManualTick` events. Ticks were rejected because:

- Gap attribution goes to the key of the opening signal, so a precedence rule cannot be
  expressed. The overlap would split by signal order instead.
- Removing ticks emits no covering interval, so a retracted meeting would leave stale intervals
  in the cloud.
- Neither precedence nor retraction would hold when the overlap spans two workspaces.

A new pure, key-generic function joins the spans with the counted gaps:

```rust
pub enum Precedence { MeetingWins, WorkWins }
pub enum Slot<W, M> { Work(W), Meeting(M) }
pub struct Slice<W, M> { pub start: OffsetDateTime, pub end: OffsetDateTime, pub slot: Slot<W, M> }

/// Pure. `gaps` come from `counted_gaps` (unchanged). Returns non-overlapping slices
/// covering exactly |gaps ∪ normalize(spans)|, and the overlapped seconds.
pub fn overlay<W: Ord + Clone, M: Ord + Clone>(
    gaps: &[(Span, W)],
    spans: &[(Span, M)],
    precedence: impl Fn(&W, &M) -> Precedence,
) -> (Vec<Slice<W, M>>, OverlapStats /* engaged_during_meeting_s */);
```

`counted_gaps`, `per_key_seconds` and `agent_active_seconds` are unchanged.

**Agent time is untouched.** An agent working while its owner is in a meeting is agent time, as it
always was. It is computed per session, summed freely, and never billed as human time; compute is
billed from tokens.

**Rules:**

1. **Normalization.** Zero-length and negative spans are dropped. When meetings overlap each other
   (a Zoom call and a Slack huddle at once), every second belongs to the meeting that started
   first, with ties going to the lower id. Meetings never double count among themselves.
2. **MeetingWins.** Every second of a span becomes `Meeting(m)`. Gap seconds inside a span leave
   `Work` and are counted in `engaged_during_meeting_s`.
3. **WorkWins.** Every gap second stays `Work(w)`. Only span seconds that no gap covers become
   `Meeting(m)`. The covered seconds are counted in `engaged_during_meeting_s`.
4. **Precedence is resolved per user** (reviewer H1). Running the overlay once per workspace
   double-counts: a meeting for client A in W1 (MeetingWins) and coding for an open-source project
   in W2 (WorkWins) at the same time would bill the same minute in both. So the `precedence`
   closure applies the workspace's `overlapPrecedence` only when the work and the meeting route to
   **the same workspace**. **Across workspaces, the meeting wins.** A meeting attributed to project
   P routes to P's workspace, and an unattributed meeting routes to the device's home workspace.
5. **Bridge minutes** (M3). On the device, a span's edges also act as human signals. The gap from
   the last prompt before a call to the call's start counts, exactly as it does for
   `dira start/stop --activity meeting`, where the start and stop events are human signals.
6. **Unattributed meetings** get their own key, `Meeting(None)`. They never fall into "unresolved"
   or into the orchestration bucket (DIRASH-0041), where the repo-less backfill could move them
   into a repo. `report.rs`'s `LineKey (Option<String>, bool)` becomes an enum:
   `Work{project, orchestration} | Meeting{project}`.
7. **Rounding** (M2). Inputs are snapped to milliseconds. Each gap keeps `whole_seconds(gap)` in
   total: the part a span covers gets `floor`, and the rest gets the remainder. Cutting a gap
   therefore never loses seconds to truncation.
8. **Window and day splits** (M1). The overlay runs on unclipped gaps and spans, padded by `±idle`
   around the requested window. Slices are split at UTC midnight only after the overlay.
   Clipping first double-counts a gap that crosses midnight inside a meeting.

With no spans, the output equals today's gaps as `Work`. This is the **regression gate**:
`dira report`, the intervals and the rollups stay byte-identical while the module is off.

### 2.3 Computed once, on the device

The overlay result is **materialized at the daemon edge**. The `Interval`s on the wire are the
slices: work intervals clipped around meetings, plus meeting intervals (`activity: "meeting"`,
`meetingId`). This keeps `batch.rs:5-7` true ("the billable seconds the cloud sees match
`dira report`").

The alternative was rejected: send unclipped intervals and overlay at read time in the cloud.
About 24 cloud read sites sum `intervals.humanSeconds` directly, including:

- `billing/summary.ts:332`
- `billing/fold.ts`
- `parallelism.ts:433`
- `spec-work.ts:113`
- `knowledge.ts:791`
- `facts.ts`, `projects.ts`, `units.ts`

Under MeetingWins each of those would add a "Meetings" line on top of raw intervals and double
count (reviewer H2). A parity fixture would not catch it, because the bug sits in the callers, not
in the function.

**Late confirmation and retraction.** A suggestion confirmed, or a meeting retracted, days after
its window synced must not leave stale intervals. On every change of a meeting's version, the
daemon:

1. re-emits the slices for `[span.start − idle, span.end + idle]` on every day the span touches,
   plus the day before (a gap may start there);
2. sends the meeting record with its new version. A retraction is a tombstone
   (`state: retracted`).

The cloud's rebuild-delete (`cloud/src/lib/ingest/unpack.ts:497-553`) already replaces intervals
that overlap a re-sent range within one workspace. For meeting spans it is extended **across the
user's workspaces**, so a span that changed precedence outcome in another workspace is rebuilt
there too. Interval sign-offs keyed by `(session, start, end)` (`unpack.ts:540-544`) are re-keyed
by containment, so clipping an interval does not drop its sign-off.

**Transport** (reviewer H3):

- Meeting changes are tracked by an `updated_seq` cursor. A confirmation is an update, and the
  event rowid cursor never moves for updates.
- `(meeting id, version)` is folded into `batch_id_for_chunk` (`cli/core/src/sync/batch.rs:1076-1108`).
  A retry after a timeout then carries a new batch id instead of being acked as a "duplicate"
  while the confirmation is lost.
- The flush gate (`cli/dirad/src/sync.rs:842`) fires on pending meeting changes as well as on new
  events.
- The cloud rejects a lower version for an id it already has. Tombstones are kept.

**Compaction** (reviewer H4):

- A day touched by a meeting that is not yet final is not compacted.
- Suggestions expire after **7 days**, under the 14-day retention, so every meeting reaches its
  final state while its gaps still exist.
- Compacted rollups are recomputed from the global `counted_gaps` plus the overlay, keyed by
  `(day, line key)`, and they store meeting lines.

This depends on fixing today's compaction bug (§9).

**Cross-device duplicates** (M4, M5). Two devices of one user can both send intervals that overlap
in time, for example the same call seen on two laptops. The v2 cloud keeps a user's stored
intervals **non-overlapping at ingest**, so a plain sum in `facts_daily` equals the union. This is
the main session's fix on `feat/cloud-v2`, in its Phase 0:
- intervals carry `device_id`;
- a batch rebuilds only its own device's rows;
- another device's overlapping rows are trimmed around the batch's spans, with `human_seconds`
  prorated and floored.

By default, trimming lets the later-arriving device win. **Proposed for meeting rows:** when a
meeting interval and another device's work interval overlap, the overlap precedence decides the
winner, not arrival order. Across workspaces the meeting wins; within a workspace,
`overlapPrecedence` applies. This matches the device-side overlay. Engaged-during-meeting across
devices is computed in `refreshMeetingFacts` from `meetings` × intervals.

### 2.4 Where the overlay applies

| Site | Treatment |
|---|---|
| `cli/core/src/report.rs` `build` (:42), `build_merged` (:165) | overlay; new "Meetings · <project \| unattributed>" lines |
| `cli/core/src/store.rs:1158-1240` compaction | overlay, keyed per (day, line key); holds back days with meetings that are not final |
| `cli/dirad/src/control.rs:715-718` `report --project` | filter **after** accounting and the overlay |
| `cli/dira/src/tui/dashboard.rs:139-183` `watch` live tail | under MeetingWins the human tail stops inside a live span while the meeting tail grows; never both. The test at `dashboard.rs:937` changes |
| `cli/dirad/src/control.rs:774` session views, `cli/dirad/src/zavet.rs:1789` `price_sessions` | overlay keyed by session, so the session column still sums to TODAY |
| `cli/dira/src/render.rs:2245,2455` parallel multiplier | denominator is Work only |
| `cli/core/src/sync/batch.rs:478-560` wire intervals | emit slices (§2.3) |
| `cli/dirad/src/state.rs:673-692` live registry, `cli/dirad/src/heartbeat.rs:672` presence | stay gap-only, documented as raw presence (not a billing base); the cloud's "Right now" never adds them to meeting time |
| `cli/core/src/sync/batch.rs:221-230` degenerate-session pruning | gap-only is correct |

Doc comments that change wording: `accounting.rs:16-18`, `state.rs:373-388` and `batch.rs:5-7`.
Each now reads "after the overlay".

### 2.5 Property tests

These go next to the existing proptests (`accounting.rs:599-765`):

1. Slices never overlap, and every slice is wider than 0.
2. The sum of the slices equals the measure of gaps ∪ spans, under both precedences.
3. Work + Meeting + engaged-during-meeting = the sum of the gaps + the sum of the normalized spans,
   exactly, given rule 7.
4. Under MeetingWins, Meeting = the sum of the normalized spans. Under WorkWins, Work = the sum of
   the gaps.
5. Work(p) ≤ `per_key_seconds`(p) for every p, with equality under WorkWins. The overlay never
   moves time between projects.
6. With no spans, the slices equal the gaps and the overlap is 0.
7. Precedence never changes the union total.
8. Adding a span never lowers the union, never raises Work and never lowers Meeting.
9. Shuffling the input order gives identical output.
10. Adding a span and then retracting it gives the original output.
11. The per-day overlays (unclipped, then split at midnight) sum to the overlay of the whole range.
12. When meetings overlap, Meeting = the measure of the union of the spans. Each second goes to the
    meeting that started first, and Meeting(m) ≤ |m|.
13. Cross-workspace: one minute is never counted in two workspaces, whatever their precedences.
14. Generators include sub-second instants, zero-length and negative spans, spans that exactly abut
    gaps (half-open edges), identical starts, spans inside a single gap, and overlapping gaps.
15. A multi-session compaction proptest: compaction then `build_merged` equals `build` on the raw
    events.
16. Agent totals are identical with and without spans, checked at the `report::build` level. The
    overlay never sees agent data, so a test on the overlay alone cannot fail.

---

## 3. Contract

### 3.1 `meetings[]`

The record is native to contract 2.0. If detection is ready first, the same shape ships as an
additive optional array in 1.9.0: a minor bump, `#[serde(default, skip_serializing_if =
"Vec::is_empty")]`, so older payloads stay byte-identical.

```rust
pub struct Meeting {
    pub id: String,                 // minted at detection (daemon ULID); stable across confirm/retract
    pub version: u32,               // monotonic; cloud rejects lower
    pub start: String,              // RFC 3339 UTC, ms precision
    pub end: String,
    pub state: MeetingState,        // recorded | confirmed | retracted
    pub source: MeetingSource,      // device | calendar | presence | user
    pub confidence: MeetingConfidence, // high | confirmed
    pub provider_class: ProviderClass, // zoom|teams|slack|webex|meet|discord|facetime|chat|browser|in_person|other
    pub evidence: Vec<Evidence>,    // capture|camera|output|window|process|calendar|presence
    pub calendar: Option<CalendarFacts>,
    pub attribution: Option<Attribution>,
    pub engaged_during_meeting_s: u32, // overlap stat from the overlay (§2.2); a count, content-free
}
pub struct CalendarFacts {
    pub attendee_bucket: AttendeeBucket, // one | two | three_to_five | six_to_ten | eleven_plus
    pub recurring: bool,
    pub series_key: Option<String>,      // HMAC-SHA256(workspace salt, iCalUID), hex
    pub external: bool,                  // any attendee domain outside the user's own domains
}
pub struct Attribution {
    pub project_ref: Option<String>,     // cloud project id from the workspace directory
    pub client_ref: Option<String>,
    pub repo_canonical: Option<String>,  // only for method = context
    pub method: AttributionMethod,       // user | calendar_rule | domain | series | context
}
```

Only `high` and user-confirmed meetings cross the wire. Low-confidence suggestions stay local
until the user confirms them.

`Interval` gains an optional `meeting_id`, set on meeting slices, and those slices carry
`activity: "meeting"`. The meeting's time is the sum of its interval slices, not `end − start`.
That keeps precedence and bridge minutes consistent.

`SessionKind` and `Harness` do not change. A new enum value there would break older cloud
validators. Under 2.0, a meeting is identified by its record, not by a session kind.

### 3.2 Why this holds D-0001

- **Fixed vocabularies.** Every field is either a closed enum, a timestamp, a count bucket or a
  boolean, or one of these:
  - an id the device mints;
  - a reference to the workspace's own cloud entities;
  - a keyed hash of an opaque calendar UID.

  None of them has room for a title, an attendee, an email address, a domain, a URL, a meeting id
  or an app name.
- **Domains stay local.** Attendee domains and the organizer domain are used on the device to
  resolve `client_ref` and compute `external`, then dropped.
- **The series key is not reversible by the cloud.** `series_key` is keyed by a per-workspace salt
  that the cloud also holds. Calendar UIDs are random, though, so the cloud cannot recover anything
  from the key. It only lets the meetings of one series, across one workspace's members, group
  together. A title hash is deliberately **not** used, because "Standup" can be found by
  dictionary.
- **The denylist test covers the new fields.** No field name contains a token on the
  `wire_contract_carries_no_content_fields` denylist (`prompt`, `content`, `diff`, `body`, `text`,
  `patch`, `code`, `snippet`). That test's fixture populates every new field, so the walk covers
  them.
- **Generated artifacts.** `just contract` regenerates `attestation.schema.json` and the signing
  vector. Neither is ever edited by hand. The cloud picks them up with `just contract-pull`, and
  the contract-sync skill applies.
- **Cloud tolerance for new enum values.** Before the first extension of any of these enums, the
  cloud must map an unknown `providerClass` to `other` and an unknown `evidence` value to
  "ignored". A new catalogue app therefore never forces a cloud change.

### 3.3 Labels channel (titles, opt-in)

Titles are content, so they cannot ride the attestation wire. A user who wants them on invoices or
in the dashboard opts into a **separate channel**, built like knowledge sync (D-0001's "own
channel", DIRASH-0030):

- **Envelope.** `MeetingLabelsEnvelope { deviceId, labels: [{ meetingId, title }] }`, signed with
  the device key like `KnowledgeEnvelope`, and posted to its own route.
- **Its own prompt.** `MEETINGS_LABELS_DISCLOSURE` names exactly what is sent: the calendar title
  of each meeting matched to a calendar event, and nothing else. The prompt is never bundled with
  meeting capture, calendar connection, device linking or billing. Declining keeps titles local,
  in `dira meetings` only.
- **Workspace gate.** Labels must be allowed by workspace policy (§6). If they are not, the cloud
  answers `content_not_allowed`, as the knowledge tier does today.
- **Erasure.** Labels are deleted with their meeting, and by `dira meetings forget`.
- Changing what the channel sends changes `MEETINGS_LABELS_DISCLOSURE` in the same commit.

### 3.4 Consent, knobs and onboarding

| Knob (`KNOBS`, `cli/dira/src/config_cmd.rs:42`) | Values | Default |
|---|---|---|
| `modules.meetings` | `off \| suggest \| auto` | `off` |
| `meetings.calendar` | `off \| crosscheck \| primary` (per calendar for `primary`) | `off` |
| `meetings.labels` | `off \| on` | `off` |
| `meetings.presence` | `off \| on` (provider call-state, from the branch spec) | `off` |

- **Nothing is sampled while capture is off.** With `modules.meetings = off`, no gate or adapter
  is constructed.
- **Where the step sits.** `dira onboard` gets a `meetings` step after `cloud:repo` and before
  `knowledge`, which stays last (`cli/dira/src/onboard/mod.rs:109-197`). The step returns a
  `StepOutcome` and never aborts (DIRASH-0029). `--print` stays byte-identical.
- **What the prompt says.** It always prints `MEETINGS_DISCLOSURE`:
  - what is read: which app is using your microphone, camera or speakers, whether a call window
    exists, and whether a call app is running;
  - what is never read: audio, video, screen content, window titles, calendar entries, attendees;
  - what leaves the machine: start, end, app category, how sure dira is, and the project you
    assign;
  - where the result goes: `suggest` vs `auto`.
- **Declining** sets `off`. When the workspace policy says `capture: off`, the step reports
  `Skipped` ("disabled by your workspace") and does not ask.
- **Calendar, labels and presence** each get their own later prompt, through
  `dira meetings connect …` or `dira config set`, never folded into this step (DIRASH-0030).
- **Tests pin the prompt text,** as `KNOWLEDGE_DISCLOSURE`'s tests do.

---

### 3.5 Alignment with contract 2.0 (synced 2026-11-09)

The main session's contract 2.0 notes (shared notes, `contract-2.0-cli.md`) and the cloud v2
baseline (`origin/feat/cloud-v2`, `cloud/src/db/schema/facts.ts`) settle these points:

- **Meetings land as a 2.x minor.** 2.0 reserves no untyped slot. `meetings: Vec<Meeting>` plus
  `Interval.meetingId` arrive as additive optional fields in 2.x. The 1.9 fallback in §3.1 applies
  only if detection ships before 2.0.
- **Chained batches replace the batch-id fold.** 2.0 batches carry `seq`, `prevBatchHash` and
  `chainEpoch`, and a retry resends the identical payload. A meeting change made after a payload
  was built rides the *next* batch. So §2.3's "fold `(id, version)` into `batch_id_for_chunk`"
  becomes "pending meeting changes trigger a flush". The version check in the cloud stays.
- **`dayRollups.humanSeconds` is the overlaid figure.** It equals the sum of the interval slices
  (§2.3), so meeting time is included once. A day touched by a late confirmation or retraction is
  resent with `revision + 1`, which the 2.0 drift check already expects.
- **`facts_daily` is keyed by repo and late-bound to project/client.** A meeting usually has no
  repo. Meeting time therefore goes to its own `facts_daily_meetings`, keyed by (workspace, day,
  user, project_id nullable, provider_class, source) and carrying `meeting_seconds`,
  `meeting_external_seconds`, `meeting_count` and `engaged_during_meeting_seconds`. The meeting
  slices in `facts_daily` count toward `engaged_seconds` with `repo_id` null and `harness = ""`, so
  engaged totals stay complete. `focus_seconds` is derived at read time from `facts_hourly` and the
  meeting slices, not stored.
- **Local migrations.** Contract 2.0 takes `0010_v2.sql`, so meetings use `0011_meetings.sql`.

## 4. Calendar (phase 2)

### 4.1 Sources

All of these are on the device. Raw events never leave it.

| Source | How | Scope / permission | Notes |
|---|---|---|---|
| **Secret ICS URL** (first) | `dira meetings connect ics <url>`; the URL goes in the keychain; polled every 15 min, plus at the start of a candidate | none, no OAuth | Works for Google ("Secret address in iCal format") and Outlook ("Publish calendar"). An M365 admin may disable publishing. The ICS carries `ATTENDEE`, `ORGANIZER`, `RRULE`, `UID`, `X-GOOGLE-CONFERENCE` / `X-MICROSOFT-ONLINEMEETING*` |
| **Google** | `dira meetings connect google`; OAuth with PKCE on a loopback redirect in the `dira` process; token in the keychain; `dirad` reloads | `calendar.events.readonly`, a *sensitive* scope: Google app verification needed, no CASA security assessment | `events.list` with `singleEvents=true` and a `fields` mask. The mask is a bandwidth filter, not a consent boundary, and the prompt says so |
| **Microsoft Graph** | `dira meetings connect microsoft`; same flow | `Calendars.ReadBasic` (delegated, no admin consent; excludes body, attachments and extensions) | Whether `attendees`, `responseStatus` and `isOnlineMeeting` are returned is undocumented; **check live in P0**. If they are missing, the fallback is `Calendars.Read` with a larger prompt |
| macOS EventKit | **deferred** | needs `NSCalendarsFullAccessUsageDescription` in a bundled `.app`; `dirad` is a bare binary, and requests from it are silently denied | revisit if dira ships a menu-bar `.app` |
| Cloud-side OAuth | **rejected** for the base layer | — | would put full event data in the cloud. It stays for the enterprise attendance-reconcile layer only (branch spec "Later layers") |

`freeBusy` is rejected: it returns busy ranges only, with no conference data and no attendees.
Provider push (Graph webhooks, Google `watch`) needs a public endpoint, so every source above is
polled, over a window of −1 to +2 days, diffed by `id` + `updated`.

### 4.2 Reduce on the device

Each event is reduced on arrival to the branch spec's eligibility verdict, and the response is
dropped. An event is eligible only when all of these hold:

- it is not cancelled;
- the user's RSVP is not declined, tentative or unanswered;
- it shows as busy;
- it is not all-day;
- it is not focus time, out-of-office or a working location;
- it has another invitee or a conference link.

The reduction yields these local facts:

| Local fact | Use | Crosses the wire as |
|---|---|---|
| start, end | matching, tiers 3–6 | the meeting span (device bounds win) |
| eligible, `remote \| in_person` | the fusion ladder | `source: calendar`, `provider_class: in_person` |
| conference type | provider class (`meet`, `teams`) | `provider_class` |
| attendee count | meeting size | `attendeeBucket` |
| recurring, recurring event UID | series grouping | `recurring`, `series_key` (HMAC) |
| organizer and attendee **domains** | client attribution, external vs internal | `client_ref` / `project_ref` (resolved), `external: bool` |
| title | local display only | never on this wire; the labels channel if opted in |

**Matching to a detected span:** a calendar event matches when it overlaps the span by at least
50% of the event, or by at least 5 minutes. When several events overlap, the largest overlap
wins. Device bounds always win, so a calendar event never stretches or shrinks a detected meeting.
Calendar-only time (tier 6, `primary`) is clipped to the stretch where local presence shows the
user at the machine.

### 4.3 Attribution to a client or project

Rules are tried in order, and the first match wins. The method is recorded in
`attribution.method`.

1. **user**: `dira meetings assign`, or an assignment in the cloud UI. This is sticky for the
   meeting and is also learned as **series memory**.
2. **calendar_rule**: a rule mapping a whole calendar to a project ("the Acme shared calendar →
   project Acme"), set in the cloud.
3. **domain**: an attendee or organizer domain matches a client's domains. The match is resolved
   **locally** against the workspace directory below. Several clients matched means no match, and
   the meeting goes to the inbox with candidates.
4. **series**: a meeting of a series the user assigned before.
5. **context**: the repo with the most engaged time within ±15 min of the meeting. This is only
   ever a *suggestion* (`method: context`, shown as "suggested"). A policy may refuse to bill on
   it (§5.3).
6. None of the above: unattributed. The meeting goes to the "needs attribution" inbox and is
   never billed silently.

**Workspace directory (new cloud → device read).** `GET /api/v{1,2}/devices/me/directory`, per
workspace, authenticated like `ServerMeta`. It returns:

- `projects[{id, name}]`;
- `clients[{id, domains[]}]`;
- `calendarRules[{calendarHash, projectId}]`, where `calendarHash` is an HMAC of the calendar id,
  so the cloud never learns the calendar address;
- `seriesMemory[{seriesKey, projectId}]`;
- the workspace `salt`;
- the meeting policy (§6).

The device caches the directory and refreshes it when a sync is acked. Resolving locally means
attendee domains never cross the wire, and the cloud never needs domain hashes it could reverse
by dictionary.

---

## 5. Cloud

### 5.1 Storage

A new `meetings` table, in Drizzle for Postgres and SQLite:

| Columns | Notes |
|---|---|
| `workspace_id`, `user_id`, `device_id`, `meeting_id` | unique on (`device_id`, `meeting_id`) |
| `version`, `start`, `end`, `state` | an upsert with a lower version is rejected; retractions are kept as tombstones until the retention period ends |
| `source`, `confidence`, `provider_class`, `evidence` (smallint bitmask) | |
| `attendee_bucket`, `recurring`, `series_key`, `external` | |
| `project_id`, `client_id`, `repo_id`, `attribution_method` | |
| `assurance`, `source_attestation` | |

Assurance:

- Device-sourced and user-confirmed meetings are `attributed`, as typed meetings already are
  (`unpack.ts:765-770`).
- Calendar-only meetings are `unverified` until confirmed.

Meeting time lives in `intervals` rows that carry `meeting_id`. The `meetings` row holds only the
metadata. A labels table, `meeting_labels(meeting_id, title)`, exists only behind the labels
policy (§3.3).

### 5.2 `facts_daily` (schema v2 read model)

The main session owns schema v2. The columns below are a proposal to it, handed over in the shared
notes. Grain: (workspace, user, day, project).

| Column | Definition |
|---|---|
| `engaged_seconds` | sum of the user's intervals, which equals the union because ingest keeps them non-overlapping (§2.3) |
| `meeting_seconds` | the union of meeting-slice intervals |
| `meeting_billable_seconds` | the portion billable under the policy (§5.3) |
| `meeting_external_seconds` | meetings with `external = true` |
| `engaged_during_meeting_seconds` | overlap stat from the device. Rides the meeting record in 2.0 as `engagedDuringMeetingS` (a count, content-free) |
| `focus_seconds` | work in contiguous blocks of 90 min or more with no meeting slice inside. This matches the lab's `FlowWeek.focusHours` |
| `meeting_count` | distinct meetings (each counted on its start day) |

Dimensions that do not fit the grain (`provider_class`, `source`, `series_key`,
`attendee_bucket`) go in a narrow `meeting_facts_daily` table keyed (workspace, user, day,
provider_class, source). That table is the source for the series-cost and provider-mix tiles.

### 5.3 Billing

Today a repo-less interval is dropped from billing unless it is orchestration time
(`cloud/src/lib/data/billing/fold.ts:140-154`). A typed meeting is therefore shown but never
billed. That contradicts the manual-session design, which meant repo-less meetings to be billable.
Meetings get **their own fold path** and a new policy rule at the workspace, client and project
rungs of the existing cascade (`cloud/src/lib/billing/policy-schema.ts:36-68`):

```ts
meetings: {
  billable: "never" | "external_only" | "attributed" | "always",  // default "attributed"
  minAssurance: "device" | "confirmed",                           // default "device"
  allowContextAttribution: boolean,                               // default false
  allowCalendarOnly: boolean,                                     // default false
}
```

- **Rate.** The existing `perActivityRate.meeting` (`policy.ts:207-214`); when absent, the
  project's rate.
- **Precedence.** `workspaces.settings.meetings.overlapPrecedence`, applied on the device (§2).
  The cloud never re-decides it.
- **Unattributed or refused meetings.** These are meetings with no attribution, with `method:
  context` when that is not allowed, or calendar-only when that is not allowed. They go to a
  **"needs attribution" inbox** per person. Assigning one re-attributes the meeting with a new
  version through the device when it is online, or in the cloud, which writes an `entry`-rung
  override. Either way it is billed only after that.
- **Invoice lines.** `Project · Meetings — 6.5 h (12 calls)`, with `evidence_refs` naming the
  meeting ids. Never a title, unless labels are enabled and the workspace turned on "show meeting
  titles on invoices".
- **`external_only`.** Bills client-facing calls and leaves internal standups unbilled, without
  any title.

### 5.4 Tiles per lens

These fit the lenses spec (`.zavet/specs/lenses.md`, D-0070..D-0073, proposed): lens controls are
*rooms*, *reach*, *person rows* and *money*.

| Lens | Tile | Source | Guard |
|---|---|---|---|
| Self (every member) | **Meeting load**: hours per week, share of engaged | `facts_daily` | own data only |
| Self | **Focus vs meetings**: focus blocks of 90 min or more against meeting hours, per day | `focus_seconds`, `meeting_seconds` | |
| Self | **Fragmentation**: days where meetings break up every block over 90 min | slices | |
| Self | **Review**: suggestions and the "needs attribution" inbox | `meetings` | |
| Team | **Team meeting load**, **focus distribution** (histogram) | aggregates | `managerVisibility = aggregate_only` by default, **minimum group size 5**. Person rows only where both the lens's *person rows* and the workspace policy allow them |
| Money | **Cost of meetings** = meeting hours × `costPerEngagedHour` (`workspaces.settings.billing`) | `facts_daily` | the money control |
| Money | **Billable vs unbilled meeting hours per client** | billing fold | |
| Money | **Recurring series cost**: the cost of one series across members, grouped by `series_key` | `meeting_facts_daily` | aggregate only, minimum group size 5 |
| Client view | billable meeting hours on the project | invoice lines | no titles |
| Miara | **Meeting displacement** = meeting hours ÷ delivered units, replacing the lab's "no source" placeholder (`meeting-preview.tsx`) | `facts_daily` | |

The lab's `TeamData.meetingSource: "no_source" | "capture"` flips to `capture` once a workspace
has any meeting row.

### 5.5 MCP metrics

These are offered to the mcp-plan session. They read `facts_daily` and `meeting_facts_daily`, and
the lens's reach and money controls are enforced in the data layer, as everywhere else.

| Metric | Definition | Dimensions |
|---|---|---|
| `meetings.hours` | `meeting_seconds / 3600` | day/week, project, client, provider_class, source |
| `meetings.share` | `meeting_seconds / engaged_seconds` | as above |
| `meetings.count` | `meeting_count` | as above |
| `meetings.billable_hours` | `meeting_billable_seconds / 3600` | client, project |
| `meetings.cost` | hours × `costPerEngagedHour` (money lens only) | team, project, client |
| `meetings.series_cost` | cost grouped by `series_key` (aggregate, minimum group 5) | series |
| `focus.hours` | `focus_seconds / 3600` | day/week, person (self, or with person-rows reach) |
| `meetings.engaged_during` | `engaged_during_meeting_seconds / 3600` (multitasking) | self only |

No metric can return a title, an attendee, a domain or a raw `series_key`. Series appear as
"series #n" with their cost.

---

## 6. Privacy, GDPR and enterprise controls

### 6.1 Workspace meeting policy

`workspaces.settings.meetings` is set by the owner or an admin on the settings screen
(`settings-cockpit` spec). The device receives it in the workspace directory and in the sync
acknowledgement, caches it, and obeys it.

| Key | Values | Default | Effect on the device |
|---|---|---|---|
| `capture` | `off \| opt_in \| recommended` | `opt_in` | `off`: the onboarding step is skipped; meeting records and meeting slices for this workspace are not sent (the slices fold back into plain work); if every linked workspace says `off`, no gate is constructed at all. `recommended` changes only the wording of the onboarding prompt. **There is no "required"** |
| `calendar` | `off \| allowed` | `allowed` | `off`: calendar facts for this workspace are not sent and calendar-only tiers are not used |
| `labels` | `off \| allowed` | `off` | the labels channel (§3.3) |
| `managerVisibility` | `aggregate_only \| person_rows` | `aggregate_only` | cloud only (§5.4) |
| `overlapPrecedence` | `meeting_wins \| work_wins` | `meeting_wins` | the overlay (§2) |
| `retentionDays` | integer | 400 (13 months) | cloud deletes meeting rows, labels and meeting intervals older than this |

**The person keeps control:**

- The local knob always wins. A user can set `modules.meetings off` whatever the policy says.
- A per-user **opt-out in the cloud** stops ingest for that user. The device reads it and stops
  sending.
- An admin can disable meeting capture but can never force it on.
- `dira status` shows the effective state and why ("off — disabled by workspace Acme").

### 6.2 GDPR

The posture, for customers deploying dira to employees in the EU:

- **Lawful basis.** For an employer this is usually **Art. 6(1)(f), legitimate interest**,
  sometimes (b) for client billing, backed by a **DPIA (Art. 35)**. It is **not consent**: WP29
  Opinion 2/2017 (WP249) holds that employee consent is rarely freely given. The per-user opt-in
  in §6.1 is a product control on top of the lawful basis, not the basis itself.
- **Data minimisation (Art. 5(1)(c)) and privacy by design (Art. 25).** Meeting data is closed
  enums only, all reduction happens on the device, no titles or attendees reach the cloud by
  default, and team views are aggregates.
- **Transparency (Art. 13).** `MEETINGS_DISCLOSURE` gives the full list of what is read and what
  is sent. `dira meetings trace` shows live, per signal, what the detector sees.
- **Access and erasure (Art. 15, 17).** `dira meetings export` writes the local records as JSON.
  `dira meetings forget [--all | <id>]` deletes them locally and sends tombstones, which delete
  them in the cloud. The cloud's existing account deletion covers `meetings`, `meeting_labels`
  and meeting intervals.
- **Hosting and retention.** EU hosting for EU workspaces, and the `retentionDays` default of
  13 months.
- **Purpose limitation.** The Google Meet REST API's note against performance evaluation applies
  to Meet API data, which dira does not use. The **design rule** still holds: meeting data is
  never used for an individual performance score, and per-person meeting views need the
  person-rows lens and `managerVisibility = person_rows`.

### 6.3 Works council (Germany)

A works council has co-determination rights when a technical tool is *objectively suitable* to
monitor behaviour or performance, whatever the employer intends (BetrVG §87(1) Nr. 6; BAG
1 ABR 20/21, 8 March 2022, the Office 365 case). Software rolled out across a whole company falls
to the group works council. Automatic meeting detection qualifies. dira therefore ships:

- a **works-agreement checklist** in the docs. It covers the purpose, the fields collected, who
  sees what, retention, the opt-out, the ban on using the data for performance evaluation, and
  the audit log;
- a **lock**. Once `managerVisibility = aggregate_only` is locked, it needs two admins to change
  and is recorded in the audit log;
- a policy export (JSON or PDF) to attach to the agreement.

Legal review is needed before any feature built for employers, such as person rows of meeting
time or org-API reconcile, is marketed in the EU.

---

## 7. Interfaces summary

| Surface | New |
|---|---|
| Crate | `cli/meetings` |
| Daemon | `dirad/src/meetings.rs` task; control requests `MeetingsStatus`, `MeetingsList`, `MeetingsConfirm{id}`, `MeetingsDismiss{id}`, `MeetingsAssign{id, project}`, `MeetingsForget{ids\|all}`, `MeetingsReload` |
| CLI | `dira meetings [list\|confirm\|dismiss\|assign\|trace\|connect\|disconnect\|export\|forget]` |
| Store | migration `0011_meetings.sql`: `meetings`, `meeting_suggestions`, `calendar_cache` (start, end, eligibility and domains only, pruned at +2 days), `meeting_sync` cursor in `meta` |
| Accounting | `accounting::overlay`, `Precedence`, `Slot`, `Slice`, `LineKey` enum |
| Contract | `Meeting`, `CalendarFacts`, `Attribution` and their enums; `AttestationBatch.meetings`; `Interval.meetingId`; `MeetingLabelsEnvelope`; directory response type |
| Cloud | `meetings`, `meeting_facts_daily`, `meeting_labels` tables; the directory route; the labels route; the `meetings` billing rule; the inbox; tiles; MCP metrics |
| Config | `modules.meetings`, `meetings.calendar`, `meetings.labels`, `meetings.presence` |
| Onboarding | `meetings` step, `MEETINGS_DISCLOSURE`, `MEETINGS_LABELS_DISCLOSURE`, `CALENDAR_DISCLOSURE` |
| Doctor | the branch spec's `meetings.*` checks, plus `meetings.policy` (effective policy per workspace, `skip` when unlinked) |

## 8. Proposed decisions

These are drafted in `.zavet/decisions/` with `status: proposed`. Their numbers are provisional
(DIRASH-0060 to DIRASH-0066). Contract 2.0 took DIRASH-0052, so meetings start at 0060 to leave
room for the parallel sessions. They are renumbered with `zavet renumber` if one lands there first.

| # | Draft | Supersedes / relates |
|---|---|---|
| M1 | DIRASH-0060: Meeting time comes from local device-in-use signals, never a bot or call content | carries the branch's record forward; D-0001, DIRASH-0030 |
| M2 | DIRASH-0061: Meeting detection is gated by OS notifications; the per-process walk runs only while a call is a candidate | D-0006 spirit (no foreground cost) |
| M3 | DIRASH-0062: Meetings are their own versioned wire record; meeting time rides intervals with a `meetingId` | D-0020 (per-chunk acks), DIRASH-0035 |
| M4 | DIRASH-0063: The meeting/work overlay is computed once, on the device; precedence is per user, and a meeting wins across workspaces | `accounting.rs` invariant, DIRASH-0041 |
| M5 | DIRASH-0064: Calendar facts are reduced on the device; attribution resolves locally; no titles, attendees or domains cross | D-0001 |
| M6 | DIRASH-0065: Meeting titles ride only their own consent-gated labels channel | D-0001, DIRASH-0030 |
| M7 | DIRASH-0066: The daemon obeys the workspace meeting policy; admins can disable but never force on; the user's opt-out always wins | DIRASH-0029, DIRASH-0030 |

## 9. Existing bugs found during review

The `accounting-reviewer` found these in today's code. Each is a separate issue, to be fixed
before P1 (see the plan's P0a).

1. **Local compaction miscounts** (`cli/core/src/store.rs:1158-1211`). It computes
   `total_human_seconds` per (day, session) from that session's own signals only:
   - global de-duplication is lost: two interleaved sessions over 300 s count 480 s;
   - gaps between sessions and gaps that cross midnight are dropped;
   - every gap is credited to the session's first project;
   - agent time uses `active_seconds` instead of `agent_active_seconds`.

   `merged_report_matches_raw_before_compaction` (`report.rs:367`) uses a single session, so it
   misses all of this.
2. **`report --project` filters before de-duplicating** (`cli/dirad/src/control.rs:715-718`). With
   P at 0 s, Q at 60 s and P at 120 s, P is reported as 120 s instead of 60 s.
3. **Token-only batches get a constant id** (`cli/core/src/sync/batch.rs:1103-1107`). A flush or
   extra chunk with tokens only gets `Ulid::from(fnv(∅))`. The cloud answers "duplicate"
   (`route.ts:224-237`), the daemon advances `META_TOKEN_CURSOR` anyway (`sync.rs:1037-1043`), and
   the token rows are silently lost. `attestations.ulid` is a global primary key, so the id also
   collides across devices.

## 10. Open questions

- **Muted from the start, no call window.** Where C is `Unavailable` (Wayland, missing macOS
  keys), `platform` B+D falls to low. Should a sustained B+D, say 3 min, qualify? Decide from P0.
- **Bluetooth headsets on macOS.** If the process-level `IsRunningInput` also reads false, every
  AirPods call depends on B+C+D. P0 measures it.
- **`engagedDuringMeetingS` on the wire.** It is a count. Confirm that the main session accepts it
  on the 2.0 meeting record rather than deriving it in the cloud, which cannot see raw gaps.
- **Recurring series cost across people.** The `series_key` lets a team aggregate group one
  series across members. Is that welcome, or too close to monitoring for `aggregate_only`
  workspaces? The current default shows it only under the money lens, with a minimum group of 5.
- **The Graph `ReadBasic` field set** decides whether Microsoft needs the larger `Calendars.Read`
  prompt.
- **ICS URLs are bearer secrets.** Kept in the keychain and never logged. Should the cloud warn an
  admin when publishing is enabled tenant-wide? Out of scope for now.
- **Provider presence** (Teams `InACall`, Slack huddle) stays as specified in the branch spec,
  opt-in and cross-check only. It is not scheduled before P5.

## Sources

The detection and permission evidence, the muted-mic table, the catalogue and the
calendar-eligibility research are in the branch spec and its shared research notes. Added for this
design, checked 2026-11-09:

- OverSight's CoreAudio and CMIO listeners: github.com/objective-see/OverSight (`AVMonitor.m`).
- Granola's detection notes, including the per-process floor at macOS 14.2:
  docs.granola.ai/help-center/taking-notes/transcription.
- Zoom `CptHost` / `CptService` exist only during a meeting:
  gist.github.com/brunerd/5ca9bf0605e82df1678fab644e50fbaf, and the Zoom community thread on the
  VDI exception.
- `RegNotifyChangeKeyValue`: learn.microsoft.com (ms724892). ConsentStore semantics:
  gist.github.com/cobryan05/8e191ae63976224a0129a8c8f376adc6.
- WASAPI session notifications: learn.microsoft.com, `IAudioSessionManager2`.
- PipeWire node properties: docs.pipewire.org (`pipewire-props(7)`). PulseAudio subscribe:
  docs.rs/libpulse-binding.
- `ext-idle-notify-v1`: wayland.app. `GetLastInputInfo`: learn.microsoft.com.
- EventKit access levels: Apple TN3152.
- Google scope classification: developers.google.com/identity/protocols/oauth2/production-readiness/sensitive-scope-verification.
  `Calendars.ReadBasic`: graphpermissions.merill.net.
- BAG 1 ABR 20/21 (8 March 2022): commentary from hensche.de and advant-beiten.com. WP29 Opinion
  2/2017 (WP249): ec.europa.eu newsroom.
