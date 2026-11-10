//! Send meeting changes as restatement batches (contract 2.1, DIRASH-0063,
//! DIRASH-0067).
//!
//! On whenever there are meetings AND the cloud advertises
//! [`dira_contract::CAPABILITY_RESTATEMENT_WINDOWS`] in `GET /api/v1/meta`:
//! it applies `windows` to the device's claims in EVERY workspace
//! (dodi-smart/dirahq-cloud#310, cloud D-0087), so a restatement whose meeting
//! and surrounding work route to different workspaces leaves no stale claim
//! behind (accounting review H1). An older cloud replaces claims per workspace
//! and would bill a restated minute twice, so against one nothing here runs:
//! flushes are not clipped, no restatement is sent, and changes wait. While the
//! capability is missing, the step re-reads the meta at most hourly, so a cloud
//! deploy turns sync on without a daemon restart. The capability is also the
//! kill switch: a cloud that drops it turns sync off.
//!
//! Meetings only exist once `modules.meetings` has been on. Turning the module
//! off stops detection, not sync: a meeting already on the cloud keeps flushes
//! clipping around it, and a later change to it (a retraction) still goes out.
//! Stopping sync there would let a flush re-send work over a meeting the cloud
//! still bills.
//!
//! A flush's event chunks never carry a meeting's time (a meeting has no
//! events), and flushes clip their work around the counted meetings. Every
//! change to a meeting (recorded, confirmed, retracted, reassigned) triggers a
//! restatement:
//!
//! 1. read every stored event in the meeting's padded range BY TIME;
//! 2. rebuild that range (`dira_core::sync::meetings`);
//! 3. send it with the range as `windows` and the changed records, through
//!    `post_chained`.
//!
//! A group is restated only once its range has settled: `now` past the end of
//! the event read, which is the meetings' end plus `2 × idle_seconds` (10 min
//! by default). The wait is tied to `idle_seconds` on purpose: the overlay pads
//! the window by idle so its edge gaps have context, and events may still land
//! anywhere in that read until it has passed. Settling sooner would change the
//! bytes of a retry. A retry after a lost ack therefore re-sends identical
//! bytes.
//!
//! A range that reaches back into compacted days (raw events pruned), or past
//! the 40-day day ledger, is never restated: the rebuild would see no work
//! there and the window would delete claims the cloud rightly holds
//! (accounting review M3), or sign a day rollup the cloud cannot match. Such a
//! change sends its records alone, with no intervals and no window. To keep
//! that rare, compaction never prunes the padded range of a change still
//! waiting to be sent ([`compaction_floor`]), so only a change made after its
//! days were compacted (an edit to a meeting older than retention) goes out
//! records-only. A retraction sent that way leaves the meeting's slices billed
//! on the cloud (DIRASH-0067).
//!
//! The meeting cursor (`meeting_cursor`) moves only over a fully acknowledged
//! prefix of changes. Rejections keep their own streak, and the step runs on
//! its own backoff: it never fails or delays a flush.

use crate::state::AppState;
use crate::sync::{post_chained, SyncError, SYNC_BACKOFF};
use dira_contract::{
    AttributionMethod, Meeting, MeetingAttribution, MeetingConfidence, MeetingState as WireState,
};
use dira_core::signing::DeviceKey;
use dira_core::store::{DetectionConfidence, MeetingState, StoredMeeting};
use dira_core::sync::meetings::{build_meeting_records, build_meeting_restatement, CountedMeeting};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, Instant};
use time::{Duration, OffsetDateTime};

const META_MEETING_CURSOR: &str = "meeting_cursor";
const META_MEETING_STREAK: &str = "meeting_poison";

/// Whether the cloud advertised [`dira_contract::CAPABILITY_RESTATEMENT_WINDOWS`]
/// at the last meta handshake. False until one says so.
static CLOUD_RESTATES_WINDOWS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// When the step last re-read the cloud's meta while the capability was missing.
static LAST_CAPABILITY_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

/// How often a step re-reads the meta while the capability is missing.
const CAPABILITY_RECHECK: StdDuration = StdDuration::from_secs(3600);

/// Record the handshake's answer (`sync::check_schema_handshake`).
pub(crate) fn set_cloud_restates_windows(on: bool) {
    CLOUD_RESTATES_WINDOWS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether meetings ride the wire: the cloud applies windows everywhere.
pub(crate) fn cloud_restates_windows() -> bool {
    CLOUD_RESTATES_WINDOWS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Raw events this far back may no longer be summed into the device's day
/// ledger (`Store::prune_day_rollups` keeps 40 days); a restatement that far
/// back would sign a rollup the cloud cannot match. One day of margin.
const LEDGER_DAYS: i64 = 39;

/// The earliest instant compaction must keep raw events for: the padded read
/// start of the oldest meeting change not yet sent. `None` when nothing waits,
/// or when the cloud cannot take restatements (holding compaction for a cloud
/// that may never advertise would grow the event log without bound).
pub(crate) async fn compaction_floor(state: &AppState, idle: Duration) -> Option<OffsetDateTime> {
    if !cloud_restates_windows() {
        return None;
    }
    let after = cursor(state).await;
    let changed = state.store.meetings_changed_after(after).await.ok()?;
    changed
        .iter()
        .map(|m| m.started_at)
        .min()
        .map(|t| t - idle - idle)
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
/// Counted whatever the cloud can take, so an idle flush still reaches
/// [`step_paced`], which re-reads the meta while the capability is missing.
pub(crate) async fn pending(state: &AppState) -> i64 {
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
    if !cloud_restates_windows() {
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
    if !cloud_restates_windows() {
        let due = LAST_CAPABILITY_CHECK
            .lock()
            .ok()
            .map(|mut g| {
                let due = g.is_none_or(|t| t.elapsed() >= CAPABILITY_RECHECK);
                if due {
                    *g = Some(Instant::now());
                }
                due
            })
            .unwrap_or(false);
        if due {
            crate::sync::check_schema_handshake(client, Some(cloud_url)).await;
        }
        if !cloud_restates_windows() {
            return;
        }
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
        let compacted = state
            .store
            .compacted_since(lo - idle)
            .await
            .map_err(|e| SyncError::Fatal(format!("read compaction state: {e}")))?;
        let batch = if compacted || lo - idle < now - Duration::days(LEDGER_DAYS) {
            // The raw events are gone: restating would delete what the cloud
            // rightly holds (see the module doc). Report the records only.
            build_meeting_records(records, device_id, hi, now)
        } else {
            let events = state
                .store
                .events_in_at_range(lo - idle, hi + idle)
                .await
                .map_err(|e| SyncError::Fatal(format!("read events by time: {e}")))?;
            let counted_meetings = counted_between(state, lo - idle, hi + idle).await?;
            let fallback = state
                .store
                .latest_identity_email(hi + idle)
                .await
                .map_err(|e| SyncError::Fatal(format!("read identity: {e}")))?
                .unwrap_or_default();
            build_meeting_restatement(
                &events,
                (lo, hi),
                &counted_meetings,
                records,
                device_id,
                &fallback,
                idle,
                now,
            )
        };
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
}
