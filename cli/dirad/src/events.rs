//! Helpers for synthesizing the events that manual commands produce.
//!
//! A manual dira is represented as a `ManualStart` followed by periodic
//! `ManualTick`s and a closing `ManualStop`. Because the accounting engine counts
//! the gaps between consecutive human signals (when within the idle window), a
//! tick cadence below the idle threshold makes a manual dira accrue continuously
//! — and it still de-duplicates against any concurrent agent sessions.

use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// Tick spacing for materialized manual intervals (must be < idle threshold).
pub const TICK_SECS: i64 = 60;

fn new_id() -> String {
    Ulid::generate().to_string()
}

/// Short, user-facing handle for a session id (its ULID tail).
pub fn handle_of(session_id: &str) -> String {
    if session_id.len() > 6 {
        session_id[session_id.len() - 6..].to_string()
    } else {
        session_id.to_string()
    }
}

/// Build a single event for a manual session.
#[allow(clippy::too_many_arguments)]
pub fn manual_event(
    session_id: &str,
    kind: EventKind,
    at: OffsetDateTime,
    project: Option<String>,
    identity_email: Option<String>,
    label: Option<String>,
    activity: Option<String>,
    note: Option<String>,
) -> RawEvent {
    RawEvent {
        id: new_id(),
        at,
        session_id: session_id.to_string(),
        harness: Harness::Manual,
        kind,
        cwd: None,
        project,
        identity_email,
        // Manual sessions aren't bound to a working dir at event-build time, so the
        // session branch is left unset (the cloud falls back to author+time).
        branch: None,
        tool: None,
        label,
        activity,
        note,
    }
}

/// Materialize a closed retroactive interval `[start, end]` into start + ticks +
/// stop events, so it counts as continuous human time of `end - start`.
#[allow(clippy::too_many_arguments)]
pub fn materialize_interval(
    session_id: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
    project: Option<String>,
    identity_email: Option<String>,
    label: Option<String>,
    activity: Option<String>,
    note: Option<String>,
) -> Vec<RawEvent> {
    let mk = |kind, at| {
        manual_event(
            session_id,
            kind,
            at,
            project.clone(),
            identity_email.clone(),
            label.clone(),
            activity.clone(),
            note.clone(),
        )
    };

    let mut events = vec![mk(EventKind::ManualStart, start)];
    let mut t = start + Duration::seconds(TICK_SECS);
    while t < end {
        events.push(mk(EventKind::ManualTick, t));
        t += Duration::seconds(TICK_SECS);
    }
    events.push(mk(EventKind::ManualStop, end));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::accounting::{total_human_seconds, Signal};

    #[test]
    fn materialized_interval_accrues_full_duration() {
        let start = OffsetDateTime::UNIX_EPOCH;
        let end = start + Duration::minutes(10);
        let events = materialize_interval("s", start, end, None, None, None, None, None);
        let signals: Vec<Signal> = events
            .iter()
            .filter(|e| e.kind.is_human_signal())
            .map(|e| Signal {
                at: e.at,
                project: e.project.clone(),
            })
            .collect();
        // 10 minutes, counted continuously via 60s ticks under a 5min idle window.
        assert_eq!(total_human_seconds(&signals, Duration::minutes(5)), 600);
    }
}
