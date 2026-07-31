//! What the daemon *actually serializes* for a session view.
//!
//! Every assertion here is on the JSON, not on the Rust value. That is the whole
//! point: the bug these tests exist for was a correct-looking Rust type whose
//! wire form was wrong. `SessionView.kind` was built with `format!("{:?}", …)`,
//! and `Debug` ignores the enum's `rename_all = "snake_case"`, so the wire
//! carried `"Manual"` while every consumer compared against `"manual"`. The
//! comparisons silently never matched.
//!
//! The unit tests around the renderers could not catch it, because their fixtures
//! were hand-built with the lowercase spelling that production never emitted —
//! they asserted correct behaviour against an impossible input. Driving a real
//! `Request::Status` through real daemon state and reading the serialized bytes is
//! what closes that gap.

use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use dira_core::protocol::{Request, Response, SessionView, StatusView};
use dira_core::{Config, Store};
use dirad::state::{AppState, EventMsg};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

async fn test_state() -> (AppState, tokio::sync::mpsc::Receiver<EventMsg>) {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    (state, rx)
}

fn event(session: &str, harness: Harness, kind: EventKind, at: OffsetDateTime) -> RawEvent {
    RawEvent {
        id: Ulid::new().to_string(),
        at,
        session_id: session.to_string(),
        harness,
        kind,
        cwd: None,
        project: Some("github.com/acme/api".to_string()),
        identity_email: None,
        branch: None,
        tool: None,
        label: None,
        activity: None,
        note: None,
    }
}

/// Append events straight to the store and fold them into the live registry, the
/// same pair the writer performs — without needing the writer loop running.
async fn ingest(state: &AppState, events: &[RawEvent]) {
    let idle = state.config.idle();
    for ev in events {
        state.store.append(ev).await.expect("append");
        dirad::control::lock_recover(&state.sessions).observe(ev, idle);
    }
}

async fn status_view(state: &AppState) -> Box<StatusView> {
    match dirad::control::dispatch(state, Request::Status).await {
        Response::Status(v) => v,
        other => panic!("expected Status, got {other:?}"),
    }
}

/// `to_value` on the view the daemon just built — the bytes a CLI would parse.
fn wire(view: &SessionView) -> serde_json::Value {
    serde_json::to_value(view).expect("serialize SessionView")
}

/// A manual session must serialize as `"manual"`, lowercase, on **both** fields.
/// Before the fix these were `"Manual"`.
#[tokio::test]
async fn a_manual_session_serializes_in_snake_case() {
    let (state, _rx) = test_state().await;
    let t0 = OffsetDateTime::now_utc() - Duration::seconds(60);
    ingest(
        &state,
        &[
            event("m1", Harness::Manual, EventKind::ManualStart, t0),
            event(
                "m1",
                Harness::Manual,
                EventKind::ManualTick,
                t0 + Duration::seconds(30),
            ),
        ],
    )
    .await;

    let status = status_view(&state).await;
    let view = status
        .active
        .iter()
        .find(|s| s.session_id == "m1")
        .expect("manual session is live");
    let json = wire(view);

    assert_eq!(json["kind"], "manual");
    assert_eq!(json["harness"], "manual");
    // Structurally impossible for a manual timeline to have agent evidence: it
    // only ever emits ManualStart/ManualTick/ManualStop.
    assert_eq!(json["has_agent_activity"], false);
    assert_eq!(json["agent_seconds"], 0);
}

/// An agent session serializes as `claude_code` / `agent`, and carries evidence.
#[tokio::test]
async fn an_agent_session_serializes_in_snake_case_and_reports_evidence() {
    let (state, _rx) = test_state().await;
    let t0 = OffsetDateTime::now_utc() - Duration::seconds(60);
    ingest(
        &state,
        &[
            event("a1", Harness::ClaudeCode, EventKind::SessionStart, t0),
            event(
                "a1",
                Harness::ClaudeCode,
                EventKind::UserPrompt,
                t0 + Duration::seconds(5),
            ),
            event(
                "a1",
                Harness::ClaudeCode,
                EventKind::PreTool,
                t0 + Duration::seconds(10),
            ),
            event(
                "a1",
                Harness::ClaudeCode,
                EventKind::PostTool,
                t0 + Duration::seconds(40),
            ),
        ],
    )
    .await;

    let status = status_view(&state).await;
    let view = status
        .active
        .iter()
        .find(|s| s.session_id == "a1")
        .expect("agent session is live");
    let json = wire(view);

    assert_eq!(json["kind"], "agent");
    assert_eq!(json["harness"], "claude_code");
    assert_eq!(json["has_agent_activity"], true);
    assert!(
        json["agent_seconds"].as_i64().unwrap() > 0,
        "an agent session with tool calls must report wall-clock"
    );
}

/// **The reported bug, end to end.** Take the daemon's own serialized manual
/// session, parse it back exactly as `dira watch` does, run the dashboard's live
/// tail across 45s — past one 30s `ManualTick` — and assert the agent timer has
/// not moved.
///
/// This is the assertion the whole chain exists to protect: it consumes the real
/// wire bytes, so it fails if the daemon ever regresses to a `Debug` spelling,
/// even though the Rust type would still be correct.
#[tokio::test]
async fn a_manual_session_from_the_wire_never_grows_agent_time() {
    let (state, _rx) = test_state().await;
    let t0 = OffsetDateTime::now_utc() - Duration::seconds(30);
    ingest(
        &state,
        &[
            event("m1", Harness::Manual, EventKind::ManualStart, t0),
            event(
                "m1",
                Harness::Manual,
                EventKind::ManualTick,
                t0 + Duration::seconds(30),
            ),
        ],
    )
    .await;

    let status = status_view(&state).await;
    // Round-trip through JSON — the CLI never sees the daemon's in-process value.
    let bytes = serde_json::to_vec(&*status).expect("serialize status");
    let parsed: StatusView = serde_json::from_slice(&bytes).expect("parse status");

    let before = parsed
        .active
        .iter()
        .find(|s| s.session_id == "m1")
        .expect("manual session present")
        .agent_seconds;

    let now = OffsetDateTime::now_utc() + Duration::seconds(45);
    let ticked = dira_dashboard_tick(&parsed, now);

    let after = ticked
        .active
        .iter()
        .find(|s| s.session_id == "m1")
        .expect("manual session present")
        .agent_seconds;

    assert_eq!(
        after, before,
        "a manual session's agent timer must not move across a live tick"
    );
    assert_eq!(after, 0);
}

/// The dashboard's `tick` lives in the `dira` binary crate, which an integration
/// test cannot import. Re-express the one rule under test — grow the agent timer
/// only when the session `accrues_agent_time()` — against the parsed wire value.
/// If that predicate ever stops holding for manual sessions, this fails.
fn dira_dashboard_tick(s: &StatusView, now: OffsetDateTime) -> StatusView {
    let mut s = s.clone();
    for sess in &mut s.active {
        if sess.accrues_agent_time() {
            let last = sess
                .last_activity_at
                .as_deref()
                .and_then(|t| {
                    OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok()
                })
                .unwrap_or(now);
            sess.agent_seconds += (now - last).whole_seconds().clamp(0, 300);
        }
    }
    s
}
