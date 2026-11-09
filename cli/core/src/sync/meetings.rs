//! Meetings on the wire (contract 2.1, DIRASH-0062, DIRASH-0063).
//!
//! A meeting is a span with no events, so its time cannot ride the event-id
//! flush chunks. Two paths carry it:
//!
//! - **Ordinary flushes** clip their work intervals around the meetings already
//!   counted (recorded or confirmed), so a minute is never sent as both work and
//!   meeting. They send no meeting slices and no `windows`.
//! - **Restatements** run when a meeting changes (recorded, confirmed,
//!   retracted, reassigned). The daemon reads every stored event whose `at`
//!   falls in the meeting's padded range BY TIME (never a flush slice), and this
//!   module rebuilds that range: work clipped around the counted meetings, plus
//!   one interval per meeting slice (`activity: meeting`, `meetingId`). It sends
//!   them with the range as `windows` and the changed meeting records. The cloud
//!   replaces this device's claims there, so a late confirmation or a
//!   retraction re-bills exactly once.
//!
//! Content-free like the rest of the wire (D-0001): ids, times, closed enums and
//! seconds only. The device applies `MeetingWins` to its own overlaps (the
//! workspace policy is not delivered to devices yet). This cannot disagree with
//! the cloud's rule (`resolveClaims`, D-0085): a device's intervals reach the
//! cloud already disjoint, so the cloud's precedence only ever decides between
//! DIFFERENT devices' claims. Interval ids and the
//! batch id are derived from content, so a retry is byte-identical. Bridge
//! minutes (the gap between the last prompt and a meeting edge) are not counted
//! yet.

use super::batch::{fmt, interval_id};
use crate::accounting::{self, KeyedGap, MeetingSpan, Precedence, Slot};
use crate::model::RawEvent;
use dira_contract::{AttestationBatch, BatchWindow, Interval, Meeting};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// A counted meeting, as the producer needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountedMeeting {
    pub id: String,
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
    /// The repo it is filed under, if the person assigned one.
    pub project: Option<String>,
}

/// The synthetic session a meeting's slices are filed under.
pub fn meeting_session(id: &str) -> String {
    format!("meeting-{id}")
}

fn parse(t: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()
}

/// Overlay `intervals` (one device's counted work gaps, disjoint) with the
/// counted meetings under `MeetingWins`. Work pieces keep their interval's
/// session, identity, repo and activity, get content-derived ids, and keep the
/// overlay's exact seconds. With `emit_meetings`, meeting slices come back as
/// intervals too. Also returns, per meeting id, the engaged seconds it covered.
pub fn overlay_intervals(
    intervals: &[Interval],
    meetings: &[CountedMeeting],
    emit_meetings: bool,
    identity_email: &str,
) -> (Vec<Interval>, std::collections::BTreeMap<String, u64>) {
    let mut engaged = std::collections::BTreeMap::new();
    if meetings.is_empty() {
        return (intervals.to_vec(), engaged);
    }
    let gaps: Vec<KeyedGap<usize>> = intervals
        .iter()
        .enumerate()
        .filter_map(|(k, iv)| {
            Some(KeyedGap {
                start: parse(&iv.started_at)?,
                end: parse(&iv.ended_at)?,
                key: k,
            })
        })
        .collect();
    // A total order before keying, so which meeting wins an overlap never
    // depends on how the caller read them (the report orders the same way).
    let mut meetings: Vec<&CountedMeeting> = meetings.iter().collect();
    meetings.sort_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
    let spans: Vec<MeetingSpan<usize>> = meetings
        .iter()
        .enumerate()
        .map(|(k, m)| MeetingSpan {
            start: m.started_at,
            end: m.ended_at,
            key: k,
        })
        .collect();
    let overlaid = accounting::overlay(&gaps, &spans, |_, _| Precedence::MeetingWins);

    // Engaged-during per meeting: the work seconds each normalized piece covered.
    for p in &accounting::normalize_spans(&spans) {
        for g in &gaps {
            let (s, e) = (g.start.max(p.start), g.end.min(p.end));
            if e > s {
                *engaged.entry(meetings[p.key].id.clone()).or_insert(0) +=
                    (e - s).whole_seconds() as u64;
            }
        }
    }

    let mut out = Vec::new();
    for slice in &overlaid.slices {
        let (started_at, ended_at) = (fmt(slice.start), fmt(slice.end));
        match slice.slot {
            Slot::Work(k) => {
                let src = &intervals[k];
                out.push(Interval {
                    id: interval_id(
                        &src.source_session,
                        &started_at,
                        &ended_at,
                        &src.identity_email,
                        src.repo_canonical.as_deref(),
                    ),
                    started_at,
                    ended_at,
                    human_seconds: slice.seconds.max(0) as u64,
                    ..src.clone()
                });
            }
            Slot::Meeting(k) if emit_meetings => {
                let m = &meetings[k];
                let session = meeting_session(&m.id);
                out.push(Interval {
                    id: interval_id(
                        &session,
                        &started_at,
                        &ended_at,
                        identity_email,
                        m.project.as_deref(),
                    ),
                    repo_canonical: m.project.clone(),
                    identity_email: identity_email.to_string(),
                    started_at,
                    ended_at,
                    human_seconds: slice.seconds.max(0) as u64,
                    activity: Some("meeting".to_string()),
                    source_session: session,
                    meeting_id: Some(m.id.clone()),
                });
            }
            Slot::Meeting(_) => {}
        }
    }
    (out, engaged)
}

/// The most common identity among `events`, for a meeting slice (a meeting has
/// no session of its own). Empty when no event carries one.
pub fn dominant_identity(events: &[RawEvent]) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for e in events {
        if let Some(email) = e.identity_email.as_deref() {
            *counts.entry(email).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(e, _)| e.to_string())
        .unwrap_or_default()
}

/// Build one restatement batch for `window` (DIRASH-0063, contract 2.1
/// `windows`). `events` must be EVERY stored event with `at` in
/// `[window.start - idle, window.end + idle)`, read by time; `counted` the
/// recorded or confirmed meetings overlapping that padded range; `records` the
/// changed meeting records this batch reports. Everything is content-derived,
/// so a retry rebuilds the identical batch.
pub fn build_meeting_restatement(
    events: &[RawEvent],
    window: (OffsetDateTime, OffsetDateTime),
    counted: &[CountedMeeting],
    mut records: Vec<Meeting>,
    device_id: &str,
    idle: Duration,
    now: OffsetDateTime,
) -> AttestationBatch {
    let work = super::batch::build_intervals_seeded(events, idle, &[]);
    let (mut intervals, engaged) =
        overlay_intervals(&work, counted, true, &dominant_identity(events));
    // Send what touches the window; the padding only gives gaps their context.
    intervals.retain(|iv| match (parse(&iv.started_at), parse(&iv.ended_at)) {
        (Some(s), Some(e)) => s < window.1 && e > window.0,
        _ => false,
    });
    intervals.sort_by(|a, b| (&a.started_at, &a.id).cmp(&(&b.started_at, &b.id)));
    for r in &mut records {
        r.engaged_during_meeting_seconds = engaged.get(&r.id).copied().unwrap_or(0);
    }
    records.sort_by(|a, b| a.id.cmp(&b.id));
    let windows = vec![BatchWindow {
        started_at: fmt(window.0),
        ended_at: fmt(window.1),
    }];

    // Content id: the restated interval ids, each record's (id, version), and the
    // window, stamped with the window's end so ids sort by time like ULIDs.
    let versions: Vec<String> = records
        .iter()
        .map(|r| format!("{}@{}", r.id, r.version))
        .collect();
    let mut parts: Vec<&str> = intervals.iter().map(|iv| iv.id.as_str()).collect();
    parts.extend(versions.iter().map(String::as_str));
    parts.push(&windows[0].started_at);
    parts.push(&windows[0].ended_at);
    let lo = super::batch::fnv1a(&parts, 0xcbf29ce484222325);
    let hi = super::batch::fnv1a(&parts, 0x100000001b3);
    let content = ((hi as u128) << 64) | lo as u128;
    let ts = (window.1.unix_timestamp_nanos() / 1_000_000).max(0) as u128;
    let batch_id = Ulid::from(((ts & ((1u128 << 48) - 1)) << 80) | (content & ((1u128 << 80) - 1)))
        .to_string();

    AttestationBatch {
        batch_id,
        device_id: device_id.to_string(),
        generated_at: fmt(now),
        seq: 0,
        prev_batch_hash: String::new(),
        chain_epoch: 0,
        client: None,
        capture_health: None,
        intervals,
        sessions: vec![],
        token_usage: vec![],
        artifacts: vec![],
        day_rollups: vec![],
        meetings: records,
        windows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventKind;
    use dira_contract::{Harness, MeetingConfidence, MeetingSource, MeetingState, ProviderClass};
    use time::macros::datetime;

    fn prompt(k: i64) -> RawEvent {
        RawEvent {
            id: format!("e{k:04}"),
            at: datetime!(2026-11-06 10:00 UTC) + Duration::minutes(4 * k),
            session_id: "s-code".into(),
            harness: Harness::ClaudeCode,
            kind: EventKind::UserPrompt,
            cwd: None,
            project: Some("github.com/acme/api".into()),
            identity_email: Some("dev@example.com".into()),
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
            spec: None,
            lineage: Default::default(),
        }
    }

    fn record(id: &str, version: u32) -> Meeting {
        Meeting {
            id: id.into(),
            version,
            started_at: "2026-11-06T10:10:00Z".into(),
            ended_at: "2026-11-06T10:20:00Z".into(),
            state: MeetingState::Recorded,
            source: MeetingSource::Device,
            confidence: MeetingConfidence::High,
            provider_class: ProviderClass::Zoom,
            evidence: vec![],
            calendar: None,
            attribution: None,
            engaged_during_meeting_seconds: 0,
        }
    }

    /// Prompts every 4 minutes from 10:00 to 10:28; a meeting 10:10-10:20.
    fn fixture() -> (Vec<RawEvent>, Vec<CountedMeeting>) {
        let events = (0..8).map(prompt).collect();
        let meetings = vec![CountedMeeting {
            id: "m1".into(),
            started_at: datetime!(2026-11-06 10:10 UTC),
            ended_at: datetime!(2026-11-06 10:20 UTC),
            project: None,
        }];
        (events, meetings)
    }

    const IDLE: Duration = Duration::minutes(5);

    #[test]
    fn a_flush_clips_work_around_a_meeting_and_sends_no_meeting_slice() {
        let (events, meetings) = fixture();
        let work = super::super::batch::build_intervals_seeded(&events, IDLE, &[]);
        assert_eq!(work.iter().map(|i| i.human_seconds).sum::<u64>(), 28 * 60);
        let (clipped, engaged) = overlay_intervals(&work, &meetings, false, "dev@example.com");
        assert!(clipped.iter().all(|i| i.meeting_id.is_none()));
        assert_eq!(
            clipped.iter().map(|i| i.human_seconds).sum::<u64>(),
            18 * 60
        );
        assert_eq!(engaged.get("m1"), Some(&600));
        for i in &clipped {
            assert!(
                i.ended_at.as_str() <= "2026-11-06T10:10:00Z"
                    || i.started_at.as_str() >= "2026-11-06T10:20:00Z",
                "{i:?} overlaps the meeting"
            );
        }
    }

    #[test]
    fn no_meetings_leaves_the_intervals_untouched() {
        let (events, _) = fixture();
        let work = super::super::batch::build_intervals_seeded(&events, IDLE, &[]);
        let json = |v: &[Interval]| serde_json::to_string(v).unwrap();
        assert_eq!(
            json(&overlay_intervals(&work, &[], false, "").0),
            json(&work)
        );
    }

    #[test]
    fn a_restatement_carries_work_meeting_slices_records_and_its_window() {
        let (events, meetings) = fixture();
        let window = (
            datetime!(2026-11-06 10:05 UTC),
            datetime!(2026-11-06 10:25 UTC),
        );
        let now = datetime!(2026-11-06 11:00 UTC);
        let build = |v| {
            build_meeting_restatement(
                &events,
                window,
                &meetings,
                vec![record("m1", v)],
                "dev",
                IDLE,
                now,
            )
        };
        let b = build(1);
        let meeting: Vec<_> = b
            .intervals
            .iter()
            .filter(|i| i.meeting_id.is_some())
            .collect();
        assert_eq!(meeting.len(), 1);
        assert_eq!(
            (meeting[0].human_seconds, meeting[0].activity.as_deref()),
            (600, Some("meeting"))
        );
        assert_eq!(meeting[0].identity_email, "dev@example.com");
        assert_eq!(b.windows.len(), 1);
        assert_eq!(b.meetings[0].engaged_during_meeting_seconds, 600);
        let mut spans: Vec<_> = b
            .intervals
            .iter()
            .map(|i| (i.started_at.clone(), i.ended_at.clone()))
            .collect();
        spans.sort();
        for w in spans.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlap: {w:?}");
        }
        // Deterministic: a retry is byte-identical; a new version is a new batch.
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            serde_json::to_string(&build(1)).unwrap()
        );
        assert_ne!(b.batch_id, build(2).batch_id);
    }

    #[test]
    fn a_retracted_meeting_restates_the_work_unclipped() {
        let (events, _) = fixture();
        let window = (
            datetime!(2026-11-06 10:05 UTC),
            datetime!(2026-11-06 10:25 UTC),
        );
        let mut r = record("m1", 3);
        r.state = MeetingState::Retracted;
        let b = build_meeting_restatement(
            &events,
            window,
            &[],
            vec![r],
            "dev",
            IDLE,
            datetime!(2026-11-06 11:00 UTC),
        );
        assert!(b.intervals.iter().all(|i| i.meeting_id.is_none()));
        assert_eq!(b.meetings[0].state, MeetingState::Retracted);
        assert!(b
            .intervals
            .iter()
            .any(|i| i.started_at.as_str() < "2026-11-06T10:20:00Z"
                && i.ended_at.as_str() > "2026-11-06T10:10:00Z"));
    }
}
