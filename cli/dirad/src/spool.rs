//! Never drop a hook (DIRASH-0049).
//!
//! Two halves:
//! - [`enqueue`]: every ingress path hands a normalized hook to the writer
//!   queue through this. A full queue used to be answered `Ok` and the event
//!   silently discarded. It now **spills** the event to the on-disk spool.
//! - [`spawn_drain`]: replays the spool, which the `dira hook` shim also writes
//!   when it cannot reach this daemon at all, back into the writer at startup
//!   and every few seconds after, keeping each event's original time.
//!
//! Counters are process-wide statics, not `AppState` fields: there is one
//! daemon per process, and they are read only for health reporting.

use crate::state::{AppState, EventMsg};
use dira_contract::Harness;
use dira_sources::spool::{self, Record};
use dira_sources::Normalized;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Hooks written to the spool because the ingest queue was full.
pub static SPILLED: AtomicU64 = AtomicU64::new(0);
/// Spooled hooks replayed into the writer.
pub static DRAINED: AtomicU64 = AtomicU64::new(0);
/// Hooks lost for good: the spool itself could not be written, or it was over
/// its cap and the oldest records were dropped.
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

/// What the last acknowledged batch reported, so the next one sends deltas.
static REPORTED: std::sync::Mutex<Option<(Counters, u64)>> = std::sync::Mutex::new(None);

/// Capture health since the last acknowledged report (contract 2.0
/// `captureHealth`), plus the snapshot to pass to [`mark_reported`] once the
/// batch carrying it is acknowledged. Counters are per daemon process, so a
/// restart starts the deltas over; quarantined chunks are read from `meta`.
pub async fn pending_health(
    store: &dira_core::Store,
) -> (dira_contract::CaptureHealth, (Counters, u64)) {
    let now = counters();
    let quarantined = crate::quarantine::entries(store).await.len() as u64;
    let (prev, prev_q) = REPORTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .unwrap_or((Counters::default(), quarantined));
    let health = dira_contract::CaptureHealth {
        spooled: now.drained.saturating_sub(prev.drained),
        spilled: now.spilled.saturating_sub(prev.spilled),
        dropped: now.dropped.saturating_sub(prev.dropped),
        quarantined: quarantined.saturating_sub(prev_q),
        hook_failures: None,
    };
    (health, (now, quarantined))
}

/// Record that the batch carrying the health from [`pending_health`] was
/// acknowledged.
pub fn mark_reported(snapshot: (Counters, u64)) {
    *REPORTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
}

/// A snapshot of the counters, for `status`/`doctor` and the batch health.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub spilled: u64,
    pub drained: u64,
    pub dropped: u64,
}

pub fn counters() -> Counters {
    Counters {
        spilled: SPILLED.load(Ordering::Relaxed),
        drained: DRAINED.load(Ordering::Relaxed),
        dropped: DROPPED.load(Ordering::Relaxed),
    }
}

/// How far in the future an `observed_at` may sit before it is treated as a
/// clock error and replaced by receipt time.
const MAX_FUTURE_SKEW: time::Duration = time::Duration::minutes(2);

/// The event time for a hook: when the shim observed it, else now. A future
/// time (clock skew) is clamped to now so it can never open an interval that
/// has not happened yet.
pub fn event_time(observed_at: Option<&str>) -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    observed_at
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .filter(|t| *t <= now + MAX_FUTURE_SKEW)
        .map(|t| t.min(now))
        .unwrap_or(now)
}

/// Ids of hook records recently queued to the writer, so a record that
/// arrives twice (live after the shim timed out, then from the spool) is
/// stored once. Bounded; a daemon restart forgets it, which only reopens that
/// rare window.
struct Seen {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
const SEEN_CAP: usize = 65_536;

/// Claim this record id for the writer queue, atomically: `false` if a record
/// with it was already claimed (a duplicate), `true` otherwise. Empty ids are
/// always claimable. Check and insert happen under one lock, so a live copy and
/// a spooled copy racing through the drain cannot both be queued.
pub fn claim(id: &str) -> bool {
    if id.is_empty() {
        return true;
    }
    let mut guard = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let seen = guard.get_or_insert_with(|| Seen {
        set: Default::default(),
        order: Default::default(),
    });
    if !seen.set.insert(id.to_string()) {
        return false;
    }
    seen.order.push_back(id.to_string());
    if seen.order.len() > SEEN_CAP {
        if let Some(old) = seen.order.pop_front() {
            seen.set.remove(&old);
        }
    }
    true
}

/// Give a claim back: the record did not reach the writer queue (spilled, or
/// the send failed), so its spooled copy must still replay.
fn release(id: &str) {
    if id.is_empty() {
        return;
    }
    if let Some(seen) = SEEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        seen.set.remove(id);
    }
}

/// Spooled messages the drain has queued that the writer has not finished.
/// The degenerate-session prune waits for zero: a spooled prompt still in the
/// channel is as much "activity on its way" as one still on disk.
pub static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Called by the writer when it is done with a spooled message.
pub fn finished_spooled() {
    // Saturating decrement. A compare-exchange loop rather than `fetch_update`,
    // which newer toolchains deprecate in favour of `try_update` (not yet on
    // every supported toolchain), so it builds clean everywhere.
    let mut n = IN_FLIGHT.load(Ordering::Acquire);
    while n > 0 {
        match IN_FLIGHT.compare_exchange_weak(n, n - 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(current) => n = current,
        }
    }
}

/// Whether any spooled hook is still on its way: on disk, or queued and not
/// yet written.
pub fn pending(db_path: &std::path::Path) -> bool {
    IN_FLIGHT.load(Ordering::Acquire) > 0 || spool::has_records(&spool::dir_for(db_path))
}

/// Queue a normalized hook for the writer, spilling it to the spool when the
/// queue is full. `hook_id` is the shim's record id, or `""`. `Err` only when
/// the daemon is shutting down.
pub fn enqueue(
    state: &AppState,
    harness_id: &str,
    hook_id: &str,
    norm: Normalized,
    harness: Harness,
    at: OffsetDateTime,
) -> Result<(), &'static str> {
    // A record the writer already has (delivered live after the shim timed
    // out and spooled it, or the reverse) is acknowledged, not stored again.
    if !claim(hook_id) {
        return Ok(());
    }
    let msg = EventMsg::Hook {
        norm,
        harness,
        at,
        spooled: false,
    };
    match state.tx.try_send(msg) {
        Ok(()) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(EventMsg::Hook { norm, at, .. })) => {
            // Not queued: the spilled copy must be free to replay.
            release(hook_id);
            spill(state, harness_id, hook_id, norm, at);
            Ok(())
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            release(hook_id);
            Err("daemon shutting down")
        }
    }
}

/// The write stays inline on purpose. It is one small file that must be on disk
/// before this daemon answers `Ok`; deferring it would let a crash lose an
/// event the shim was told had landed.
fn spill(state: &AppState, harness_id: &str, hook_id: &str, norm: Normalized, at: OffsetDateTime) {
    // A capture-probe row is admitted only live (DIRASH-0023); replaying one
    // later would fail admission anyway, so it is not worth keeping.
    if dira_core::model::is_probe_session(&norm.session_id) {
        return;
    }
    let observed_at = crate::heartbeat::fmt_rfc3339(at);
    let dir = spool::dir_for(&state.config.db_path);
    let mut record = Record::new(harness_id, observed_at, norm);
    if !hook_id.is_empty() {
        record.id = hook_id.to_string();
    }
    match spool::write(&dir, &record) {
        Ok(_) => {
            SPILLED.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("ingest queue full; spilled a hook to the spool");
            WAKE.notify_one();
        }
        Err(e) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("ingest queue full and the spool is unwritable ({e}); dropped a hook");
        }
    }
}

/// Wakes the drain the moment the daemon spills, instead of waiting out an
/// interval.
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The backstop between passes when the spool was empty. The drain is woken
/// directly when this daemon spills; the backstop covers records the `dira
/// hook` shim spooled while this daemon was up but busy or slow, which nothing
/// else announces.
const IDLE_INTERVAL: Duration = Duration::from_secs(300);
/// How often while records are still waiting.
const BUSY_INTERVAL: Duration = Duration::from_secs(2);
/// Most records replayed per pass, so one pass never monopolises the writer.
const PASS_LIMIT: usize = 2_000;

/// Replay the spool forever: once right away (records from while the daemon
/// was down), then whenever a spill wakes it, every few seconds while records
/// remain, and on the idle backstop.
pub fn spawn_drain(state: AppState) {
    tokio::spawn(async move {
        loop {
            let (replayed, remaining) = drain_once(&state).await;
            if replayed > 0 {
                tracing::info!(replayed, remaining, "replayed spooled hooks");
            }
            let wait = if remaining > 0 {
                BUSY_INTERVAL
            } else {
                IDLE_INTERVAL
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = WAKE.notified() => {}
                _ = state.shutdown.notified() => return,
            }
        }
    });
}

/// One drain pass. Returns `(replayed, still waiting)`.
///
/// The spool is listed once, trimmed from that list, and the first
/// [`PASS_LIMIT`] records read, all on a blocking thread. Replays only while the
/// queue has room to spare, so live hooks always find space ahead of the
/// backlog. A record is deleted only after the writer queue accepted it.
pub async fn drain_once(state: &AppState) -> (usize, usize) {
    let dir = spool::dir_for(&state.config.db_path);
    let dir_c = dir.clone();
    let listed = tokio::task::spawn_blocking(move || {
        let mut files = spool::list(&dir_c);
        let trimmed = spool::trim(&mut files, spool::MAX_RECORDS);
        let total = files.len();
        let mut good = Vec::new();
        for path in files.into_iter().take(PASS_LIMIT) {
            match spool::read(&path) {
                Ok(r) => good.push((path, r)),
                Err(e) => {
                    tracing::warn!(path = %path.display(), "unreadable spool record ({e}); quarantined");
                    spool::quarantine(&dir_c, &path);
                }
            }
        }
        (trimmed, total, good)
    })
    .await;
    let Ok((trimmed, total, records)) = listed else {
        return (0, 0);
    };
    if trimmed > 0 {
        DROPPED.fetch_add(trimmed as u64, Ordering::Relaxed);
        tracing::warn!(trimmed, "spool over its cap; dropped the oldest records");
    }

    let mut done: Vec<std::path::PathBuf> = Vec::new();
    let mut bad: Vec<std::path::PathBuf> = Vec::new();
    let mut replayed = 0;
    for (path, record) in records {
        if state.tx.capacity() < state.tx.max_capacity() / 4 {
            break;
        }
        let Some(harness) = dira_sources::harness_for(&record.harness) else {
            bad.push(path);
            continue;
        };
        if dira_core::model::is_probe_session(&record.event.session_id) || !claim(&record.id) {
            done.push(path);
            continue;
        }
        let at = event_time(Some(&record.observed_at));
        let id = record.id;
        let msg = EventMsg::Hook {
            norm: record.event,
            harness,
            at,
            spooled: true,
        };
        IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        if state.tx.send(msg).await.is_err() {
            finished_spooled();
            release(&id);
            break; // shutting down; the record stays for the next start
        }
        done.push(path);
        DRAINED.fetch_add(1, Ordering::Relaxed);
        replayed += 1;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for p in &done {
            let _ = std::fs::remove_file(p);
        }
        for p in &bad {
            spool::quarantine(&dir, p);
        }
    })
    .await;
    (replayed, total - replayed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_time_keeps_a_past_observation_and_clamps_the_future() {
        let past = OffsetDateTime::now_utc() - time::Duration::hours(3);
        let got = event_time(Some(&past.format(&Rfc3339).unwrap()));
        assert_eq!(got.unix_timestamp(), past.unix_timestamp());

        let future = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let got = event_time(Some(&future.format(&Rfc3339).unwrap()));
        assert!(got <= OffsetDateTime::now_utc());

        let none = event_time(None);
        assert!((OffsetDateTime::now_utc() - none).whole_seconds() < 5);
        let garbage = event_time(Some("yesterday"));
        assert!((OffsetDateTime::now_utc() - garbage).whole_seconds() < 5);
    }
}
