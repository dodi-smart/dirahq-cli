//! Integration tests for `dira doctor --probe`'s daemon half.
//!
//! These drive the real dispatch surface against real daemon state (in-memory
//! store, live registry, the actual writer loop), so the probe traverses the
//! same `normalize_for` → queue → writer → `enrich` → `append` path a genuine
//! hook does. The CLI half — spawning the configured hook command — is covered
//! separately; what is asserted here is everything from `IngestHook` inward.

use dira_core::model::{is_probe_session, probe_hook_payload, PROBE_SESSION_PREFIX};
use dira_core::protocol::{ProbePhase, Request, Response};
use dira_core::{Config, Store};
use dirad::state::AppState;
use std::time::Duration;

async fn daemon() -> AppState {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let (state, rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, Config::default())
        .await
        .expect("build_state");
    let writer_state = state.clone();
    // The real writer, so the probe's short path is the code under test.
    tokio::spawn(async move { dirad::writer::writer(rx, writer_state).await });
    state
}

async fn arm(state: &AppState) -> String {
    match dirad::control::dispatch(
        state,
        Request::CaptureProbe {
            phase: ProbePhase::Arm,
        },
    )
    .await
    {
        Response::CaptureProbe(v) => v.session_id.expect("arm returns a session id"),
        other => panic!("expected CaptureProbe, got {other:?}"),
    }
}

async fn send_hook(state: &AppState, session_id: &str) -> Response {
    dirad::control::dispatch(
        state,
        Request::IngestHook {
            harness: "claude".into(),
            payload: probe_hook_payload(session_id, "/tmp"),
        },
    )
    .await
}

async fn verify(
    state: &AppState,
    session_id: &str,
    wait_ms: u64,
) -> dira_core::protocol::CaptureProbeView {
    match dirad::control::dispatch(
        state,
        Request::CaptureProbe {
            phase: ProbePhase::Verify {
                session_id: session_id.to_string(),
                wait_ms,
            },
        },
    )
    .await
    {
        Response::CaptureProbe(v) => *v,
        other => panic!("expected CaptureProbe, got {other:?}"),
    }
}

/// The happy path, and the constraint that matters most: the row lands, is
/// reported as landed, and is **deleted in the same request** — so a probe can
/// never leave billing data behind.
#[tokio::test]
async fn a_probe_lands_is_reported_and_is_reaped_in_the_same_request() {
    let state = daemon().await;
    let id = arm(&state).await;

    // The daemon mints the id; the CLI never chooses it.
    assert!(is_probe_session(&id), "{id}");
    assert!(id.starts_with(PROBE_SESSION_PREFIX));

    assert!(matches!(send_hook(&state, &id).await, Response::Ok));

    let v = verify(&state, &id, 2000).await;
    assert_eq!(
        v.landed,
        Some(true),
        "the probe row never reached the store"
    );
    assert_eq!(v.deleted, 1, "the row must be reaped by verify itself");

    // Nothing left behind, anywhere.
    assert_eq!(state.store.session_event_count(&id).await.unwrap(), 0);
    assert_eq!(state.store.count_events_after(None).await.unwrap(), 0);
}

/// A probe session is never a session: it must not reach the live registry,
/// which `partial_rollups` ships from and no SQL filter can reach.
#[tokio::test]
async fn a_probe_never_enters_the_live_session_registry() {
    let state = daemon().await;
    let id = arm(&state).await;
    send_hook(&state, &id).await;
    verify(&state, &id, 2000).await;

    let all = dirad::control::lock_recover(&state.sessions).all();
    assert!(
        all.iter().all(|s| !is_probe_session(&s.session_id)),
        "a probe session reached the registry: {:?}",
        all.iter().map(|s| &s.session_id).collect::<Vec<_>>()
    );
}

/// The containment guarantee. An id the daemon did not just mint is refused at
/// ingress, so nothing outside the probe path can create a probe row — not a
/// replayed hook, not a hand-crafted payload.
#[tokio::test]
async fn an_unarmed_probe_session_id_is_refused_at_ingress() {
    let state = daemon().await;
    let forged = format!("{PROBE_SESSION_PREFIX}01FORGED");

    match send_hook(&state, &forged).await {
        Response::Error { message } => assert!(message.contains("reserved"), "{message}"),
        other => panic!("a forged probe id must be refused, got {other:?}"),
    }
    // Give the writer a chance to have wrongly stored it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(state.store.session_event_count(&forged).await.unwrap(), 0);
}

/// ...and the armed id stops being admissible the moment it is verified, so a
/// late or duplicated hook cannot resurrect a probe that is already done.
#[tokio::test]
async fn an_armed_id_is_refused_again_once_the_probe_is_verified() {
    let state = daemon().await;
    let id = arm(&state).await;
    send_hook(&state, &id).await;
    verify(&state, &id, 2000).await;

    match send_hook(&state, &id).await {
        Response::Error { .. } => {}
        other => panic!("a disarmed probe id must be refused, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(state.store.session_event_count(&id).await.unwrap(), 0);
}

/// A verify for a row that never arrives reports honestly and returns within
/// its budget — a guard against the notify path regressing into a hang.
#[tokio::test]
async fn a_probe_that_never_arrives_reports_not_landed_within_its_budget() {
    let state = daemon().await;
    let id = arm(&state).await;

    let started = std::time::Instant::now();
    let v = verify(&state, &id, 100).await;
    assert_eq!(v.landed, Some(false));
    assert_eq!(v.deleted, 0);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "verify hung instead of giving up at its deadline"
    );
}

/// Two doctors at once: the second is refused rather than corrupting the first,
/// and its refusal is a plain error the CLI reports as a skip.
#[tokio::test]
async fn only_one_probe_may_be_in_flight() {
    let state = daemon().await;
    let first = arm(&state).await;

    match dirad::control::dispatch(
        &state,
        Request::CaptureProbe {
            phase: ProbePhase::Arm,
        },
    )
    .await
    {
        Response::Error { message } => assert!(message.contains("already in flight"), "{message}"),
        other => panic!("a concurrent arm must be refused, got {other:?}"),
    }
    verify(&state, &first, 0).await;
}

/// The issue's explicit constraint: a probe row cannot reach the cloud in the
/// window between the append and the delete. Near-tautological given the store
/// filters, and that is the point — it encodes the requirement at the sync
/// boundary so a future refactor of the batch query cannot quietly undo it.
#[tokio::test]
async fn a_probe_row_is_never_selected_for_a_sync_batch() {
    let state = daemon().await;
    let id = arm(&state).await;
    send_hook(&state, &id).await;

    // Wait for it to actually be in the store, then look at the sync window
    // WITHOUT reaping — this is the vulnerable interval.
    for _ in 0..100 {
        if state.store.session_event_count(&id).await.unwrap() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        state.store.session_event_count(&id).await.unwrap(),
        1,
        "probe row never landed, so this test would pass vacuously"
    );

    let until = state.store.max_event_id().await.unwrap().expect("head");
    let batch = state.store.events_between(None, &until).await.unwrap();
    assert!(
        batch.iter().all(|e| !is_probe_session(&e.session_id)),
        "a probe row entered the sync batch"
    );
    // And it is invisible to the backlog count `dira status` reports.
    assert_eq!(state.store.count_events_after(None).await.unwrap(), 0);

    verify(&state, &id, 1000).await;
}
