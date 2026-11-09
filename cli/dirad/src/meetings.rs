//! Bot-less meeting detection, the daemon side (DIRASH-0060, DIRASH-0061).
//!
//! With `modules.meetings` off, nothing here runs: no adapter is constructed
//! and no device is sampled. With it on, one task samples an adapter, scores
//! each sample (`dira_meetings::fuse`), feeds the tracker, and stores what the
//! tracker emits: high-confidence spans as `recorded` in `auto` mode, and
//! everything else as `suggested` until the person confirms it.
//!
//! The adapter is `Replay` when `DIRA_MEETINGS_FAKE` names a JSON script (the
//! isolated end-to-end stack and tests), otherwise the OS adapter. OS adapters
//! land per platform (P1); until one exists for this OS the module reports
//! itself unavailable instead of guessing.

use crate::state::AppState;
use dira_core::config::MeetingsMode;
use dira_core::protocol::{MeetingAction, MeetingLiveView, MeetingRowView, MeetingsView, Response};
use dira_core::store::{MeetingState, NewMeeting, StoredMeeting};
use dira_meetings::{
    fuse::fuse, Catalog, Confidence, Emitted, MeetingSignal, Tracker, TrackerConfig,
};
use std::sync::Mutex;
use std::time::Duration;
use time::OffsetDateTime;

/// How often a running adapter is sampled. The OS adapters replace this steady
/// tick with notification gates (DIRASH-0061); replay has no OS to wait on.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// Unconfirmed suggestions expire after this long.
const SUGGESTION_TTL: time::Duration = time::Duration::days(7);

/// What the detector is doing right now, for `dira meetings` and `dira status`.
#[derive(Debug, Default, Clone)]
struct LiveState {
    adapter: Option<&'static str>,
    unavailable: Option<String>,
    live: Option<MeetingLiveView>,
}

static LIVE: Mutex<LiveState> = Mutex::new(LiveState {
    adapter: None,
    unavailable: None,
    live: None,
});

fn set_live(f: impl FnOnce(&mut LiveState)) {
    if let Ok(mut g) = LIVE.lock() {
        f(&mut g);
    }
}

/// The adapter this daemon samples, or why there is none.
fn adapter() -> Result<Box<dyn MeetingSignal>, String> {
    if let Some(path) = std::env::var_os("DIRA_MEETINGS_FAKE") {
        let json = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "DIRA_MEETINGS_FAKE: cannot read {}: {e}",
                path.to_string_lossy()
            )
        })?;
        let replay = dira_meetings::replay::Replay::from_json(&json, Catalog::embedded())
            .map_err(|e| format!("DIRA_MEETINGS_FAKE: invalid script: {e}"))?;
        return Ok(Box::new(replay));
    }
    Err("no meeting adapter for this OS yet".to_string())
}

/// Start detection, unless the module is off.
pub fn spawn(state: AppState) {
    let mode = state.config.modules.meetings;
    if mode == MeetingsMode::Off {
        tracing::debug!("meetings: module off");
        return;
    }
    let mut signal = match adapter() {
        Ok(a) => a,
        Err(why) => {
            tracing::info!("meetings: {why}");
            set_live(|l| l.unavailable = Some(why));
            return;
        }
    };
    set_live(|l| {
        l.adapter = Some(signal.id());
        l.unavailable = None;
    });
    tokio::spawn(async move {
        let mut tracker = Tracker::new(TrackerConfig::default());
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_expiry = OffsetDateTime::UNIX_EPOCH;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = state.shutdown.notified() => {
                    store_all(&state, mode, tracker.flush()).await;
                    return;
                }
            }
            let now = OffsetDateTime::now_utc();
            let sample = signal.sample();
            let fused = fuse(&sample);
            let done = tracker.observe(now, fused.as_ref());
            store_all(&state, mode, done).await;
            set_live(|l| {
                l.live = tracker.live().map(|live| MeetingLiveView {
                    provider_class: live.wire.as_str().to_string(),
                    since: fmt(live.since),
                    confident: live.confidence == Confidence::High,
                })
            });
            if now - last_expiry > time::Duration::hours(1) {
                last_expiry = now;
                if let Err(e) = state
                    .store
                    .expire_meeting_suggestions(now - SUGGESTION_TTL)
                    .await
                {
                    tracing::warn!("meetings: could not expire suggestions: {e}");
                }
            }
        }
    });
}

/// Store what the tracker finished. High confidence in `auto` mode is meeting
/// time; everything else waits for the person.
async fn store_all(state: &AppState, mode: MeetingsMode, done: Vec<Emitted>) {
    for e in done {
        let high = e.confidence == Confidence::High;
        let meeting = NewMeeting {
            started_at: e.start,
            ended_at: e.end,
            state: if high && mode == MeetingsMode::Auto {
                MeetingState::Recorded
            } else {
                MeetingState::Suggested
            },
            source: "device",
            confidence: if high { "high" } else { "low" },
            provider_class: e.wire.as_str(),
            evidence: 0,
        };
        match state.store.insert_meeting(&meeting).await {
            Ok(id) => tracing::info!(
                "meetings: {} {} meeting {id} ({}s)",
                meeting.state.as_str(),
                meeting.provider_class,
                (e.end - e.start).whole_seconds()
            ),
            Err(e) => tracing::warn!("meetings: could not store a meeting: {e}"),
        }
    }
}

fn fmt(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn row(m: &StoredMeeting) -> MeetingRowView {
    MeetingRowView {
        id: m.id.clone(),
        started_at: fmt(m.started_at),
        ended_at: fmt(m.ended_at),
        state: m.state.as_str().to_string(),
        provider_class: m.provider_class.clone(),
        confidence: m.confidence.clone(),
        project: m.project.clone(),
    }
}

/// `Request::Meetings`.
pub async fn view(state: &AppState) -> Response {
    let live = LIVE.lock().map(|g| g.clone()).unwrap_or_default();
    let now = OffsetDateTime::now_utc();
    let suggestions = match state.store.meeting_suggestions().await {
        Ok(rows) => rows.iter().map(row).collect(),
        Err(e) => {
            return Response::Error {
                message: format!("meetings: {e}"),
            }
        }
    };
    let recent = match state
        .store
        .meetings_between(now - time::Duration::days(7), now)
        .await
    {
        Ok(rows) => rows
            .iter()
            .filter(|m| m.state != MeetingState::Suggested)
            .map(row)
            .collect(),
        Err(e) => {
            return Response::Error {
                message: format!("meetings: {e}"),
            }
        }
    };
    Response::Meetings(Box::new(MeetingsView {
        mode: state.config.modules.meetings.as_str().to_string(),
        adapter: live.adapter.map(str::to_string),
        unavailable: live.unavailable,
        live: live.live,
        suggestions,
        recent,
    }))
}

/// `Request::MeetingAction`.
pub async fn act(
    state: &AppState,
    id: &str,
    action: MeetingAction,
    project: Option<String>,
) -> Response {
    let res = match action {
        MeetingAction::Confirm => {
            state
                .store
                .set_meeting_state(id, MeetingState::Confirmed)
                .await
        }
        MeetingAction::Dismiss => {
            state
                .store
                .set_meeting_state(id, MeetingState::Dismissed)
                .await
        }
        MeetingAction::Retract => {
            state
                .store
                .set_meeting_state(id, MeetingState::Retracted)
                .await
        }
        MeetingAction::Assign => state.store.assign_meeting(id, project.as_deref()).await,
    };
    match res {
        Ok(true) => Response::Ok,
        Ok(false) => Response::Error {
            message: format!("no meeting {id}"),
        },
        Err(e) => Response::Error {
            message: format!("meetings: {e}"),
        },
    }
}
