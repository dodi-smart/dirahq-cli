//! Send meeting changes as restatement batches (contract 2.1, DIRASH-0063).
//!
//! **Gated off.** Nothing here runs unless `DIRA_MEETINGS_SYNC` is set (the
//! isolated end-to-end stack). The cloud replaces a device's claims only where
//! a batch's intervals land, per workspace, and does not honour `windows` yet.
//! Until it applies `windows` to every workspace, a restatement could leave a
//! stale claim in another workspace and bill a minute twice (accounting
//! review H1). With the gate off, flushes do not clip work around meetings
//! either, so the wire is exactly what it was before meetings, and meetings
//! stay local (`dira meetings`, `dira report`).
//!
//! When on: the flush's event chunks never carry a meeting's time (a meeting
//! has no events). Every change to a meeting (recorded, confirmed, retracted,
//! reassigned) triggers a restatement:
//!
//! 1. read every stored event in the meeting's padded range BY TIME;
//! 2. rebuild that range (`dira_core::sync::meetings`);
//! 3. send it with the range as `windows` and the changed records, through
//!    `post_chained`.
//!
//! A group is restated only once its range has settled (`now` past its end
//! plus idle), so a retry after a lost ack re-sends identical bytes. The
//! meeting cursor (`meeting_cursor`) moves only over a fully acknowledged
//! prefix of changes. Rejections keep their own streak, and the step runs on
//! its own backoff: it never fails or delays a flush.

use crate::state::AppState;
use crate::sync::{post_chained, SyncError, SYNC_BACKOFF};
use dira_contract::{
    AttributionMethod, Meeting, MeetingAttribution, MeetingConfidence, MeetingState as WireState,
};
use dira_core::signing::DeviceKey;
use dira_core::store::{DetectionConfidence, MeetingState, StoredMeeting};
use dira_core::sync::meetings::{build_meeting_restatement, CountedMeeting};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};
use time::{Duration, OffsetDateTime};

const META_MEETING_CURSOR: &str = "meeting_cursor";
const META_MEETING_STREAK: &str = "meeting_poison";

/// Whether meetings ride the wire at all (see the module doc).
pub(crate) fn enabled() -> bool {
    std::env::var("DIRA_MEETINGS_SYNC")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
        .unwrap_or(false)
}

async fn cursor(state: &AppState) -> i64 {
    state
        .store
        .meta_get(META_MEETING_CURSOR)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// How many meeting changes are waiting: the flush gate's cheap check.
pub(crate) async fn pending(state: &AppState) -> i64 {
    if !enabled() {
        return 0;
    }
    let after = cursor(state).await;
    state
        .store
        .count_meetings_changed_after(after)
        .await
        .unwrap_or(0)
}

fn counted(m: &StoredMeeting) -> CountedMeeting {
    CountedMeeting {
        id: m.id.clone(),
        started_at: m.started_at,
        ended_at: m.ended_at,
        project: m.project.clone(),
    }
}

async fn counted_between(
    state: &AppState,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<Vec<CountedMeeting>, SyncError> {
    Ok(state
        .store
        .meetings_between(from, to)
        .await
        .map_err(|e| SyncError::Fatal(format!("read meetings: {e}")))?
        .iter()
        .filter(|m| m.state.counts())
        .map(counted)
        .collect())
}

/// The counted meetings a flush must clip its work around: those overlapping
/// the flush's events, padded by `idle`, read by time. An error aborts the
/// flush: sending unclipped work would undo a meeting already on the cloud.
pub(crate) async fn counted_for_flush(
    state: &AppState,
    events: &[dira_core::model::RawEvent],
    idle: Duration,
) -> Result<Vec<CountedMeeting>, SyncError> {
    if !enabled() {
        return Ok(Vec::new());
    }
    let (Some(lo), Some(hi)) = (
        events.iter().map(|e| e.at).min(),
        events.iter().map(|e| e.at).max(),
    ) else {
        return Ok(Vec::new());
    };
    counted_between(state, lo - idle, hi + idle).await
}

/// The wire record for a stored meeting, or `None` for one that never counted
/// and was never sent (a pending suggestion).
fn record(m: &StoredMeeting) -> Option<Meeting> {
    let state = match m.state {
        MeetingState::Suggested => return None,
        MeetingState::Recorded => WireState::Recorded,
        MeetingState::Confirmed => WireState::Confirmed,
        // A dismissal of something that was sent withdraws it; of something that
        // never was, it is a harmless tombstone.
        MeetingState::Dismissed | MeetingState::Retracted => WireState::Retracted,
    };
    let confidence =
        if m.state == MeetingState::Confirmed || m.confidence == DetectionConfidence::Low {
            MeetingConfidence::Confirmed
        } else {
            MeetingConfidence::High
        };
    Some(Meeting {
        id: m.id.clone(),
        version: m.version,
        started_at: crate::heartbeat::fmt_rfc3339(m.started_at),
        ended_at: crate::heartbeat::fmt_rfc3339(m.ended_at),
        state,
        source: m.source,
        confidence,
        provider_class: m.provider_class,
        evidence: vec![],
        calendar: None,
        attribution: m.project.as_ref().map(|p| MeetingAttribution {
            project_ref: None,
            client_ref: None,
            repo_canonical: Some(p.clone()),
            method: AttributionMethod::User,
        }),
        // Filled by the restatement builder from the overlay.
        engaged_during_meeting_seconds: 0,
    })
}

/// Group changed meetings whose padded ranges overlap, in cursor order of their
/// lowest change.
fn groups(changed: &[StoredMeeting], idle: Duration) -> Vec<Vec<&StoredMeeting>> {
    let mut by_start: Vec<&StoredMeeting> = changed.iter().collect();
    by_start.sort_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
    let mut out: Vec<(OffsetDateTime, Vec<&StoredMeeting>)> = Vec::new();
    for m in by_start {
        match out.last_mut() {
            Some((end, g)) if m.started_at - idle < *end => {
                *end = (*end).max(m.ended_at + idle);
                g.push(m);
            }
            _ => out.push((m.ended_at + idle, vec![m])),
        }
    }
    let mut gs: Vec<Vec<&StoredMeeting>> = out.into_iter().map(|(_, g)| g).collect();
    gs.sort_by_key(|g| g.iter().map(|m| m.updated_seq).min().unwrap_or(0));
    gs
}

/// Whether `err` (with `status`) is about the batch's bytes, so repeating it
/// cannot help: the same test the flush applies (DIRASH-0049).
fn content_rejection(err: &SyncError, status: Option<reqwest::StatusCode>) -> bool {
    matches!(err, SyncError::PayloadTooLarge(_))
        || (matches!(err, SyncError::Fatal(_))
            && status
                .is_some_and(|s| s.is_client_error() && s.as_u16() != 401 && s.as_u16() != 403))
}

/// The step's own backoff: a failing restatement slows only itself.
static NOT_BEFORE: Mutex<Option<(Instant, StdDuration)>> = Mutex::new(None);

/// Run [`step`] unless it is backing off; log and back off on failure. Never
/// fails the caller (the flush that already synced its events).
pub(crate) async fn step_paced(
    state: &AppState,
    device_key: &DeviceKey,
    client: &reqwest::Client,
    cloud_url: &str,
    device_id: &str,
) {
    if !enabled() {
        return;
    }
    let backing_off = NOT_BEFORE
        .lock()
        .ok()
        .and_then(|g| *g)
        .is_some_and(|(t, _)| Instant::now() < t);
    if backing_off {
        return;
    }
    match step(state, device_key, client, cloud_url, device_id).await {
        Ok(_) => {
            if let Ok(mut g) = NOT_BEFORE.lock() {
                *g = None;
            }
        }
        Err(e) => {
            let prev = NOT_BEFORE
                .lock()
                .ok()
                .and_then(|g| *g)
                .map_or(StdDuration::ZERO, |(_, d)| d);
            let wait = match &e {
                SyncError::Transient { retry_after, .. } => {
                    SYNC_BACKOFF.transient_wait(*retry_after, prev)
                }
                _ => SYNC_BACKOFF.next(prev),
            };
            if let Ok(mut g) = NOT_BEFORE.lock() {
                *g = Some((Instant::now() + wait, wait));
            }
            tracing::warn!(
                "meetings: restatement failed, retrying in {wait:?}: {}",
                crate::sync::describe(&e)
            );
        }
    }
}

/// Send every settled pending meeting change. Returns how many restatements
/// were sent.
async fn step(
    state: &AppState,
    device_key: &DeviceKey,
    client: &reqwest::Client,
    cloud_url: &str,
    device_id: &str,
) -> Result<usize, SyncError> {
    let after = cursor(state).await;
    let changed = state
        .store
        .meetings_changed_after(after)
        .await
        .map_err(|e| SyncError::Fatal(format!("read meeting changes: {e}")))?;
    if changed.is_empty() {
        return Ok(0);
    }
    let idle = state.config.idle();
    let now = OffsetDateTime::now_utc();
    let url = format!("{}/api/v1/ingest", cloud_url.trim_end_matches('/'));
    let mut chain = crate::chain::Chain::load(&state.store).await;
    let mut done: std::collections::BTreeSet<i64> = Default::default();
    let mut sent = 0usize;
    let mut failure: Option<SyncError> = None;

    for group in groups(&changed, idle) {
        let records: Vec<Meeting> = group.iter().filter_map(|m| record(m)).collect();
        let seqs: Vec<i64> = group.iter().map(|m| m.updated_seq).collect();
        if records.is_empty() {
            // Only pending suggestions changed: nothing counted, nothing to restate.
            done.extend(seqs);
            continue;
        }
        let first = group.iter().map(|m| m.started_at).min().unwrap_or(now);
        let last = group.iter().map(|m| m.ended_at).max().unwrap_or(now);
        let (lo, hi) = (first - idle, last + idle);
        if now < hi + idle {
            // Not settled: events may still land in the read range, which would
            // change the bytes of a retry. Stop here rather than skip: a later
            // group sent now could not move the cursor past this one, so it would
            // be re-sent on every flush until this one settles (at most 2·idle).
            break;
        }
        let events = state
            .store
            .events_in_at_range(lo - idle, hi + idle)
            .await
            .map_err(|e| SyncError::Fatal(format!("read events by time: {e}")))?;
        let counted_meetings = counted_between(state, lo - idle, hi + idle).await?;
        let batch = build_meeting_restatement(
            &events,
            (lo, hi),
            &counted_meetings,
            records,
            device_id,
            idle,
            now,
        );
        match post_chained(
            state, device_key, client, &url, device_id, &mut chain, batch,
        )
        .await
        {
            Ok(_) => {
                let _ = state.store.meta_set(META_MEETING_STREAK, "").await;
                done.extend(seqs);
                sent += 1;
            }
            Err((err, status)) => {
                let bounds = crate::quarantine::ChunkBounds {
                    event_cursor: Some(format!("meetings@{}", seqs.iter().max().unwrap_or(&0))),
                    artifact_rowid: None,
                    token_rowid: None,
                };
                if content_rejection(&err, status)
                    && crate::quarantine::note_rejection_on(
                        &state.store,
                        META_MEETING_STREAK,
                        &bounds,
                        &crate::sync::describe(&err),
                    )
                    .await
                    .is_some()
                {
                    tracing::warn!(
                        "meetings: the cloud kept rejecting a restatement; quarantined it (see `dira doctor`)"
                    );
                    done.extend(seqs);
                    continue;
                }
                failure = Some(err);
                break;
            }
        }
    }

    // Advance over the prefix of changes that are all acknowledged.
    let mut ordered: Vec<i64> = changed.iter().map(|m| m.updated_seq).collect();
    ordered.sort_unstable();
    let mut to = after;
    for s in ordered {
        if done.contains(&s) {
            to = s;
        } else {
            break;
        }
    }
    if to > after {
        state
            .store
            .meta_set(META_MEETING_CURSOR, &to.to_string())
            .await
            .map_err(|e| SyncError::Fatal(format!("advance meeting cursor: {e}")))?;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(sent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_contract::{MeetingSource, ProviderClass};
    use time::macros::datetime;

    fn m(
        id: &str,
        s: OffsetDateTime,
        e: OffsetDateTime,
        seq: i64,
        state: MeetingState,
    ) -> StoredMeeting {
        StoredMeeting {
            id: id.into(),
            version: 1,
            updated_seq: seq,
            started_at: s,
            ended_at: e,
            state,
            source: MeetingSource::Device,
            confidence: DetectionConfidence::High,
            provider_class: ProviderClass::Zoom,
            evidence: 0,
            project: None,
        }
    }

    #[test]
    fn close_meetings_share_one_restatement_and_groups_follow_the_cursor() {
        let idle = Duration::minutes(5);
        let all = [
            m(
                "a",
                datetime!(2026-11-06 10:00 UTC),
                datetime!(2026-11-06 10:30 UTC),
                3,
                MeetingState::Recorded,
            ),
            m(
                "b",
                datetime!(2026-11-06 10:34 UTC),
                datetime!(2026-11-06 11:00 UTC),
                1,
                MeetingState::Recorded,
            ),
            m(
                "c",
                datetime!(2026-11-06 15:00 UTC),
                datetime!(2026-11-06 15:30 UTC),
                2,
                MeetingState::Recorded,
            ),
        ];
        let gs = groups(&all, idle);
        let ids: Vec<Vec<&str>> = gs
            .iter()
            .map(|g| g.iter().map(|m| m.id.as_str()).collect())
            .collect();
        assert_eq!(ids, vec![vec!["a", "b"], vec!["c"]]);
    }

    #[test]
    fn records_map_states_and_skip_pending_suggestions() {
        let s = datetime!(2026-11-06 10:00 UTC);
        let e = datetime!(2026-11-06 10:30 UTC);
        assert!(record(&m("x", s, e, 1, MeetingState::Suggested)).is_none());
        let r = record(&m("x", s, e, 1, MeetingState::Recorded)).unwrap();
        assert_eq!(
            (r.state, r.confidence, r.provider_class),
            (
                WireState::Recorded,
                MeetingConfidence::High,
                ProviderClass::Zoom
            )
        );
        assert_eq!(
            record(&m("x", s, e, 1, MeetingState::Confirmed))
                .unwrap()
                .confidence,
            MeetingConfidence::Confirmed
        );
        assert_eq!(
            record(&m("x", s, e, 1, MeetingState::Dismissed))
                .unwrap()
                .state,
            WireState::Retracted
        );
    }

    #[test]
    fn content_rejections_match_the_flush_rule() {
        let fatal = || SyncError::Fatal("x".into());
        assert!(content_rejection(
            &fatal(),
            Some(reqwest::StatusCode::UNPROCESSABLE_ENTITY)
        ));
        assert!(!content_rejection(
            &fatal(),
            Some(reqwest::StatusCode::UNAUTHORIZED)
        ));
        assert!(!content_rejection(
            &fatal(),
            Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
        ));
        assert!(!content_rejection(&fatal(), None));
    }

    #[test]
    fn the_gate_is_off_by_default() {
        if std::env::var_os("DIRA_MEETINGS_SYNC").is_none() {
            assert!(!enabled());
        }
    }
}
