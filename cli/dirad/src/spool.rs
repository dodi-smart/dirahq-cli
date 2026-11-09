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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Queue a normalized hook for the writer, spilling it to the spool when the
/// queue is full. `Err` only when the daemon is shutting down.
pub fn enqueue(
    state: &AppState,
    harness_id: &str,
    norm: Normalized,
    harness: Harness,
    at: OffsetDateTime,
) -> Result<(), &'static str> {
    match state.tx.try_send(EventMsg::Hook { norm, harness, at }) {
        Ok(()) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Full(EventMsg::Hook { norm, at, .. })) => {
            spill(state, harness_id, norm, at);
            Ok(())
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err("daemon shutting down"),
    }
}

fn spill(state: &AppState, harness_id: &str, norm: Normalized, at: OffsetDateTime) {
    // A capture-probe row is admitted only live (DIRASH-0023); replaying one
    // later would fail admission anyway, so it is not worth keeping.
    if dira_core::model::is_probe_session(&norm.session_id) {
        return;
    }
    let observed_at = at.format(&Rfc3339).unwrap_or_default();
    let dir = spool::dir_for(&state.config.db_path);
    match spool::write(&dir, &Record::new(harness_id, observed_at, norm)) {
        Ok(_) => {
            SPILLED.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("ingest queue full; spilled a hook to the spool");
        }
        Err(e) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("ingest queue full and the spool is unwritable ({e}); dropped a hook");
        }
    }
}

/// How often the spool is checked when the last pass found nothing.
const IDLE_INTERVAL: Duration = Duration::from_secs(30);
/// How often while records are still waiting.
const BUSY_INTERVAL: Duration = Duration::from_secs(2);
/// Most records replayed per pass, so one pass never monopolises the writer.
const PASS_LIMIT: usize = 2_000;

/// Replay the spool forever: once right away, then on a short interval while
/// records remain and a long one when it is empty.
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
                _ = state.shutdown.notified() => return,
            }
        }
    });
}

/// One drain pass. Returns `(replayed, still waiting)`.
///
/// Replays only while the queue has room to spare, so live hooks always find
/// space ahead of the backlog. A record is deleted only after the writer queue
/// accepted it.
pub async fn drain_once(state: &AppState) -> (usize, usize) {
    let dir = spool::dir_for(&state.config.db_path);
    let dir_c = dir.clone();
    let trimmed = tokio::task::spawn_blocking(move || spool::trim(&dir_c, spool::MAX_RECORDS))
        .await
        .unwrap_or(0);
    if trimmed > 0 {
        DROPPED.fetch_add(trimmed as u64, Ordering::Relaxed);
        tracing::warn!(trimmed, "spool over its cap; dropped the oldest records");
    }
    let dir_c = dir.clone();
    let files = tokio::task::spawn_blocking(move || spool::list(&dir_c))
        .await
        .unwrap_or_default();
    let total = files.len();
    let mut replayed = 0;
    for path in files.into_iter().take(PASS_LIMIT) {
        if state.tx.capacity() < state.tx.max_capacity() / 4 {
            break;
        }
        let record = match spool::read(&path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(path = %path.display(), "unreadable spool record ({e}); quarantined");
                spool::quarantine(&dir, &path);
                continue;
            }
        };
        let Some(harness) = dira_sources::harness_for(&record.harness) else {
            spool::quarantine(&dir, &path);
            continue;
        };
        if dira_core::model::is_probe_session(&record.event.session_id) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let at = event_time(Some(&record.observed_at));
        let msg = EventMsg::Hook {
            norm: record.event,
            harness,
            at,
        };
        if state.tx.send(msg).await.is_err() {
            break; // shutting down; the record stays for the next start
        }
        let _ = std::fs::remove_file(&path);
        DRAINED.fetch_add(1, Ordering::Relaxed);
        replayed += 1;
    }
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
