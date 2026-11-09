# Meeting tracking without a bot: plan

Date: 2026-11-09. Status: proposed, awaiting approval. Nothing is implemented.

Design: [`../specs/2026-11-09-meeting-tracking-design.md`](../specs/2026-11-09-meeting-tracking-design.md)
(below, "the design"). It builds on the unmerged branch `origin/docs/meeting-capture-design`
("the branch spec").

Scope covers three repositories:

- dirahq-cli (this repo): detection, accounting, contract, CLI.
- dirahq-cloud: storage, billing, tiles, directory and labels routes.
- dirahq-zavet: nothing until the decisions land. The proposed records live in this repo's `.zavet/`.

Contract 2.0 and the schema v2 `facts_daily` are owned by the main session. This plan only proposes
the meeting parts to it, through the shared notes.

Each phase is a set of issues. Issues follow the dodi-smart model:

- Each has an Issue Type and an `area:*` label from `.github/areas.yml`.
- P1 adds a new `area:meetings` covering `cli/meetings/**`, `cli/dirad/src/meetings.rs` and `cli/dira/src/meetings*`.
- "Contract" says whether `/contract` changes. When it does, the contract-sync skill applies and the
  order in the memory note on cloud contract gates is followed: CLI-develop → cloud-develop →
  cloud-main → CLI-main.

## P0a: Prerequisite fixes (bugs found in review, independent of meetings)

These are wrong today. Meetings would sit on top of them, so they go first. One issue each, in
dirahq-cli. The `accounting-reviewer` agent found them by reading code, not with a failing test, so
each issue starts by writing the test that reproduces it.

| # | Issue | Type | Areas | Contract |
|---|---|---|---|---|
| 1 | Local compaction miscounts human and agent time: it computes per session instead of de-duplicating globally, drops cross-session and midnight gaps, credits the first project, and uses `active_seconds` for agent time (`cli/core/src/store.rs:1158-1211`) | Bug | accounting, store | no |
| 2 | `report --project` filters events before de-duplicating, which over-counts the filtered project (`cli/dirad/src/control.rs:715-718`) | Bug | accounting, commands | no |
| 3 | Token-only batches get a constant batch id, so the cloud acks them as duplicates and token rows are lost; the id also collides across devices (`cli/core/src/sync/batch.rs:1103-1107`, `sync.rs:1037-1043`) | Bug | sync | no |

**Tests**

- Fix 1:
  - A multi-session compaction proptest: compact, then `build_merged`, must equal `build` on the raw events.
  - Replace the single-session `merged_report_matches_raw_before_compaction` (`report.rs:367`).
- Fix 2: a P/Q/P interleaving.
- Fix 3: a token-only flush gets a unique id and is accepted by the cloud ingest test.

**Decision:** none. Use commit trailers.

## P0: Validation spike (no product code merged)

**Goal.** Turn the branch spec's and the design's "unverified" claims into measured facts, and set the thresholds.

**Where.** A throwaway `examples/meetings-probe` binary on a spike branch, run on real machines. The
spike can share the `dira meetings trace` output format, so P1 inherits it.

1. Everything in the branch spec's 10-item validation matrix:
   - each app joined muted with the camera off, then unmuted, then muted again;
   - Meet in Chrome, Edge and Firefox;
   - whether the camera is released when video goes off;
   - Windows `LastUsedTimeStop` after a crash or sleep;
   - all of Linux;
   - the helper ids of the added apps;
   - relays;
   - local presence.
2. **New in this design:**
   - **Listener reliability.**
     - macOS: do the `IsRunningSomewhere` and CMIO listeners fire on macOS 14.2, 15 and 26/27?
     - Windows: does `RegNotifyChangeKeyValue` fire on ConsentStore changes?
     - Linux: does `pw-dump --monitor` stream node state?
     - Count missed transitions over a day of calls.
   - **No-prompt check** on a clean account (`tccutil reset Microphone`, `ScreenCapture`): the
     per-process CoreAudio walk, `HIDIdleTime` and `CGWindowListCopyWindowInfo` keys.
   - **Bluetooth headsets on macOS:** check both the device-level and the per-process `IsRunningInput`.
   - **Zoom `CptHost` / `CptService`:** do they exist only during a meeting, on current Zoom builds?
   - **Budget:** CPU and RSS of the gate in steady state and during a call. Acceptance is under 5 MB
     RSS for the crate, no wakeups beyond the 30 s scan when idle, and under 0.5% of one core during a call.
   - **Graph `Calendars.ReadBasic`:** does it return `attendees`, `responseStatus`, `showAs` and
     `isOnlineMeeting`?
   - **ICS feeds:** which of `ATTENDEE`, `ORGANIZER`, `RRULE` and the conference properties appear in
     Google's and Outlook's secret feeds?

**Exit criteria.** Results are recorded in the meeting-capture spec. Provisional thresholds (30 s
start, 90 s messaging, 60 s grace, 2 min minimum, 3 h stuck holder, 7-day suggestion expiry) are
confirmed or changed. The branch spec moves to v3 with this design's deltas, and the decision drafts
are updated.

**Issue.** One Spike issue, areas meetings and doctor.

## P1: Local detection, local only (nothing syncs)

dirahq-cli. The module ships `off` by default, and nothing crosses the wire.

| # | Issue | Type | Areas |
|---|---|---|---|
| 1 | Land `.zavet/specs/meeting-capture.md` v3 from the branch spec plus P0 results plus this design. Record DIRASH-0048…0054 as `proposed` (renumber if taken). Add `area:meetings` to `.github/areas.yml` and the label list | Task | meetings, infra |
| 2 | `cli/meetings` crate: `MeetingSignal`, `Reading`, `Sample`, `Holder`, catalogue loader (`data/apps.toml`), pure `fuse`, `tracker`, `eligible`, and the `fake` adapter. Fusion table and tracker unit tests run on any CI runner | Feature | meetings |
| 3 | macOS adapter and gate: CoreAudio and CMIO listeners, per-process walk (14.2+), libproc scan with `CptHost`. Report `Unavailable` below 14.2, or when the keys are missing | Feature | meetings |
| 4 | Windows adapter and gate: ConsentStore `RegNotifyChangeKeyValue`, WASAPI session notifications, Toolhelp scan with `CptService`, the stale-0 guard. Native Windows CI lane (`ci.yml`) | Feature | meetings |
| 5 | Linux adapter and gate: `pw-dump --monitor` child (static musl, no libpipewire link), `pactl` fallback, inotify on `/dev/video*`, `/proc` scan | Feature | meetings |
| 6 | Migration `0010_meetings.sql` (`meetings` with `version` + `updated_seq`, `meeting_suggestions`, `calendar_cache`), `StoredAt` times, retention prune, the "hold compaction for non-final meeting days" rule. Use the `new-migration` skill | Feature | store, meetings |
| 7 | `accounting::overlay` plus `Precedence`/`Slot`/`Slice`, the `LineKey` enum, and wiring into `report::build`/`build_merged`, compaction, `--project`, session views, `price_sessions` and the `watch` tail (design §2.4). All 16 property tests from design §2.5. **Have `accounting-reviewer` review the PR** | Feature | accounting |
| 8 | `dirad/src/meetings.rs` task under the supervisor, with the watchdog mark. Control requests `MeetingsStatus/List/Confirm/Dismiss/Assign/Forget/Reload`. Detection suppressed while a manual `--activity meeting` session is live | Feature | daemon, meetings |
| 9 | `dira meetings list/confirm/dismiss/assign/trace/export/forget`, the `dira status` line, and the doctor `meetings.*` checks (DIRASH-0022: absent evidence → `skip`) | Feature | commands, doctor |
| 10 | Knobs `modules.meetings`, `meetings.labels`, `meetings.presence` in `KNOBS`. Onboarding `meetings` step before `knowledge`, with `MEETINGS_DISCLOSURE` and tests pinning the text; `--print` stays byte-identical | Feature | commands |

**Contract.** None in P1. The local overlay already uses `Precedence::MeetingWins`, because no
workspace policy reaches the device yet.

**Regression gate.** With `modules.meetings = off`, the existing `report`, `batch` and `sync` tests
are byte-identical, and so is `cargo test -p dira-core` for accounting.

**Spec.** meeting-capture v3, plus `capture-pipeline`, `onboarding` and `doctor` updates.

**Commits:** `feat(cli): …` and `feat(daemon): …`, each with a `Spec: meeting-capture` trailer.

## P2: Wire, cloud ingest, self tiles

**dirahq-cli**

| # | Issue | Type | Areas | Contract |
|---|---|---|---|---|
| 1 | `Meeting`, `CalendarFacts`, `Attribution` and their enums. `AttestationBatch.meetings`, `Interval.meetingId`. Shipped in contract 2.0 if it is out, otherwise as an additive 1.9.0. Populate the denylist fixture (`wire_contract_carries_no_content_fields`). `just contract` | Feature | contract, sync | **yes** |
| 2 | Materialize the overlay into wire intervals (`batch.rs:478-560`). Meeting change cursor `updated_seq`. `(id, version)` folded into `batch_id_for_chunk`. Flush gate on pending meeting changes. Re-emit `[span±idle]` slices on every version change. Restate the doc at `batch.rs:5-7` | Feature | sync, accounting | no (uses 1) |
| 3 | Version gate: send `meetings[]` and clipped intervals only when `ServerMeta` advertises support. Otherwise fold meeting slices back into plain work and keep meetings local | Task | sync | no |

**dirahq-cloud**

| # | Issue | Type | Notes |
|---|---|---|---|
| 4 | `just contract-pull`, Zod and types; ingest `meetings[]` with version-gated upsert and tombstones; `intervals.meeting_id` | Feature | waits on cli#1 merging to develop (staleness gate) |
| 5 | Rebuild-delete across workspaces for meeting spans (`unpack.ts:497-553`); interval sign-offs re-keyed by containment (`unpack.ts:540-544`) | Feature | |
| 6 | Unknown `providerClass`/`evidence` values mapped to `other`/ignored before the first enum extension | Task | |
| 7 | Meeting columns in `facts_daily` and `meeting_facts_daily` (design §5.2): engaged as the **union** across devices, never a sum. Coordinate with the main session's schema v2 | Feature | depends on v2 |
| 8 | Self tiles (meeting load, focus vs meetings, fragmentation, review). The lab's `meetingSource` flips to `capture` | Feature | promote from `cloud/components/dira/lab/meetings/*` |

**Order.** cli#1 → cloud#4 → cloud#5/6 → cli#2/3 → cloud#7/8. Promotion follows the gate memory:
CLI-develop, cloud-develop, cloud-main READY, CLI-main.

**Decisions:** M3 (DIRASH-0050) and M4 (DIRASH-0051) move to `active`, with checks bound to the
overlay proptests and the denylist test.

**Spec:** `attestation-sync` gains a meetings section.

## P3: Billing, lenses, workspace policy, MCP

**dirahq-cloud**

| # | Issue | Type |
|---|---|---|
| 1 | `workspaces.settings.meetings` policy (`capture`, `calendar`, `labels`, `managerVisibility`, `overlapPrecedence`, `retentionDays`). Settings UI. A per-user opt-out. Audit log for changes, and a two-admin lock on `aggregate_only` | Feature |
| 2 | Workspace directory route `GET /api/v*/devices/me/directory` (projects, clients plus domains, calendar rules, series memory, salt, policy). Policy also in the sync ack | Feature |
| 3 | `meetings` billing rule in the policy cascade (`policy-schema.ts`); a separate meetings fold path (no longer dropped at `fold.ts:154`); the "needs attribution" inbox; invoice lines "Meetings — N h (k calls)" with evidence refs | Feature |
| 4 | Team and money tiles (team load, focus distribution, cost of meetings, billable vs unbilled per client, series cost), with minimum group size 5 and `managerVisibility` enforced in the data layer | Feature |
| 5 | Miara meeting displacement from `facts_daily` | Feature |
| 6 | MCP metrics `meetings.hours/share/count/billable_hours/cost/series_cost/engaged_during`, `focus.hours`, with lens reach enforced. Handed to the mcp-plan session | Feature |
| 7 | Retention job (`retentionDays`) covering meetings, labels and meeting intervals | Task |

**dirahq-cli**

| # | Issue | Type | Areas | Contract |
|---|---|---|---|---|
| 8 | Fetch and cache the directory and policy. Obey `capture: off` per workspace; `overlapPrecedence` is per user, and a meeting always wins across workspaces. `dira status` shows the effective policy; doctor gets `meetings.policy` | Feature | sync, meetings | **yes** (directory response type) |
| 9 | Attribution methods `user`, `series`, `context` (calendar methods come in P4); `MeetingsAssign` pushes a new version | Feature | meetings |

**Decision:** M7 (DIRASH-0054) moves to `active`.

**Docs:** the GDPR posture, a DPIA template and a works-agreement checklist in the public docs
(landing repo or `docs/`).

## P4: Calendar

**dirahq-cli**

| # | Issue | Type | Areas |
|---|---|---|---|
| 1 | Secret ICS source: `dira meetings connect ics`, URL stored in the keychain, 15 min poll, reduction to local facts, eligibility, `calendar_cache` | Feature | meetings |
| 2 | Google: `dira meetings connect google` over a PKCE loopback in `dira`, `calendar.events.readonly`, `fields` mask, token in the keychain, reload. Google app verification (sensitive scope) is a separate Task | Feature | meetings |
| 3 | Microsoft: `Calendars.ReadBasic` (or `Calendars.Read` if P0 shows it is needed, with a larger prompt) | Feature | meetings |
| 4 | Fusion tiers 2–6 with calendar, `primary` per calendar, clipping to local presence; matching (overlap of at least 50% or at least 5 min); `CALENDAR_DISCLOSURE` prompt | Feature | meetings |
| 5 | `CalendarFacts` on the wire (attendee bucket, recurring, HMAC `series_key`, `external`); attribution methods `calendar_rule` and `domain` resolved against the directory | Feature | meetings, contract |

**dirahq-cloud.** Calendar-rule and client-domain management UI; series memory written on assignment.

**Decision:** M5 (DIRASH-0052) moves to `active`.

## P5: Labels channel (opt-in titles)

| # | Repo | Issue | Type |
|---|---|---|---|
| 1 | cli | `MeetingLabelsEnvelope` signed with the device key, its own route and channel, `meetings.labels` knob, `MEETINGS_LABELS_DISCLOSURE` prompt (DIRASH-0030 pattern), forget/erasure | Feature (contract) |
| 2 | cloud | `meeting_labels` table behind `labels: allowed`, `content_not_allowed` otherwise; an opt-in "show titles on invoices" setting | Feature |

**Decision:** M6 (DIRASH-0053) moves to `active`.

**Out of scope:**

- provider presence (Teams `InACall`, Slack huddle), which stays opt-in in the branch spec;
- the enterprise org-API attendance reconcile.

Both need their own design and legal review.

## Verification, end to end

- **Pure core:**
  - `cargo test -p dira-meetings`: the fusion table, tracker and replayed `fake` samples, on all CI OSes.
  - `cargo test -p dira-core accounting`: the 16 overlay properties.
- **Regression:** the module off means identical reports, intervals and rollups. The existing
  `batch.rs:1257` "Σ intervals = report" test, extended with meetings.
- **Manual, per OS, with an isolated daemon** (`DIRA_SOCKET_PATH`, `DIRA_DB_PATH`,
  `DIRA_HTTP_PORT`, per memory):
  - join a Zoom, Teams or Meet call;
  - `dira meetings trace` shows the gate and the signals;
  - `dira status` shows "in a meeting";
  - after the call, `dira report` shows a Meetings line;
  - prompting an agent during the call does not raise the human total under MeetingWins.
- **Wire:**
  - `just contract` gives no drift.
  - The cloud ingest test accepts a batch with `meetings[]`, rejects a lower version, and handles a
    tombstone.
  - A late confirmation re-emits slices, and the cloud totals stay equal to `dira report`.
- **Cloud:**
  - pg tests (standalone PG on 5439, per memory) for the meetings fold, the inbox and cross-workspace
    precedence: one minute is never billed twice;
  - lens guards: an aggregate with fewer than 5 members is hidden.
- **Consent:** tests pinning the text of `MEETINGS_DISCLOSURE`, `CALENDAR_DISCLOSURE` and
  `MEETINGS_LABELS_DISCLOSURE`; `onboard --print` stays byte-identical.
