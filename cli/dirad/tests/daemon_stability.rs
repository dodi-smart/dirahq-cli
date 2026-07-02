//! Integration tests for Commit 1 (stability) + Commit 2 (status reactivity).
//!
//! These stand up real daemon state (an in-memory store, the live registry, the
//! writer loop, a bound UDS) so they exercise the same code the binary runs.

use dira_contract::Harness;
use dira_core::model::{EventKind, RawEvent};
use dira_core::protocol::{Request, Response};
use dira_core::{Config, Store};
use dirad::state::{AppState, EventMsg};
use std::sync::atomic::Ordering;
use std::time::Duration as StdDuration;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

/// Build daemon state backed by an in-memory store. Returns the state plus the
/// writer's ingest receiver (the caller spawns the writer with its chosen capture
/// fn) — the sync receiver is dropped, which is fine for these tests.
async fn test_state() -> (AppState, tokio::sync::mpsc::Receiver<EventMsg>) {
    let store = Store::open_in_memory().await.expect("in-memory store");
    let config = Config::default();
    let (state, rx, _sync_rx) = dirad::build_state(store, config)
        .await
        .expect("build_state");
    (state, rx)
}

/// A commit-bearing manual event carrying a cwd + project, so the writer's
/// capture trigger fires (manual ticks built by `events::manual_event` have no
/// cwd and wouldn't). The timestamp is explicit so accrual is deterministic.
fn manual_tick_with_repo(session: &str, at: OffsetDateTime) -> RawEvent {
    RawEvent {
        id: Ulid::new().to_string(),
        at,
        session_id: session.to_string(),
        harness: Harness::Manual,
        kind: EventKind::ManualTick,
        cwd: Some("/tmp/some-repo".into()),
        project: Some("github.com/acme/api".into()),
        identity_email: None,
        branch: None,
        tool: None,
        label: None,
        activity: None,
        note: None,
    }
}

fn manual_start_with_repo(session: &str, at: OffsetDateTime) -> RawEvent {
    let mut ev = manual_tick_with_repo(session, at);
    ev.kind = EventKind::ManualStart;
    ev
}

/// (a) Timer-stall regression: a slow/hung-git capture must NOT prevent
/// subsequent `ManualTick` events from advancing `active_seconds`.
///
/// The fake capture mimics the dangerous shape — a blocking git that never
/// returns — by spawning a `spawn_blocking` task that parks for the whole test.
/// Because the writer dispatches capture detached (never awaits it), the drain
/// loop keeps folding ticks into the registry. If the writer ever awaited the
/// capture inline (the pre-Commit-1 bug), it would wedge on the first tick and
/// `active_seconds` would never grow past 0 — failing this test.
#[tokio::test]
async fn slow_git_capture_does_not_stall_timer_accrual() {
    let (state, rx) = test_state().await;

    // Fake capture: simulate a hung git that blocks its `spawn_blocking` thread,
    // off the writer. A few seconds is long past the assertion poll below, so it
    // stands in for "never returns" without leaking long-lived blocking threads
    // that would delay test teardown.
    fn hung_capture(_state: &AppState, _cwd: &str, _canonical: &str) {
        tokio::task::spawn_blocking(|| {
            std::thread::sleep(StdDuration::from_secs(3));
        });
    }

    let writer_state = state.clone();
    let writer = tokio::spawn(async move {
        dirad::writer::writer_with(rx, writer_state, hung_capture).await;
    });

    // Drive one ManualStart + a run of ManualTicks 30s apart (well under the
    // 5-minute idle window, so every gap counts toward active_seconds).
    let base = OffsetDateTime::now_utc() - Duration::minutes(10);
    state
        .tx
        .send(EventMsg::Raw(Box::new(manual_start_with_repo("s1", base))))
        .await
        .unwrap();
    for i in 1..=6 {
        let at = base + Duration::seconds(30 * i);
        state
            .tx
            .send(EventMsg::Raw(Box::new(manual_tick_with_repo("s1", at))))
            .await
            .unwrap();
    }

    // Poll the registry until active_seconds reflects the 6 × 30s of ticks. If the
    // writer were wedged on the hung capture this would time out at 0.
    let mut active = 0u64;
    for _ in 0..100 {
        active = dirad::control::lock_recover(&state.sessions)
            .active()
            .into_iter()
            .find(|s| s.session_id == "s1")
            .map(|s| s.active_seconds)
            .unwrap_or(0);
        if active >= 180 {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    assert_eq!(
        active, 180,
        "ManualTicks must keep accruing active_seconds despite a hung-git capture"
    );

    writer.abort();
}

/// (b) Poison tolerance: a poisoned sessions mutex must still serve
/// `Status`/`Sessions` instead of panicking the control handler.
#[tokio::test]
async fn poisoned_sessions_mutex_still_serves_status_and_sessions() {
    let (state, _rx) = test_state().await;

    // Poison the mutex: panic while holding the guard.
    let sessions = state.sessions.clone();
    let _ = std::thread::spawn(move || {
        let _guard = sessions.lock().unwrap();
        panic!("intentional panic to poison the sessions mutex");
    })
    .join();
    assert!(
        state.sessions.lock().is_err(),
        "precondition: the sessions mutex is poisoned"
    );

    // Both control queries must answer with a real response, not crash.
    match dirad::control::dispatch(&state, Request::Status).await {
        Response::Status(_) => {}
        other => panic!("expected Status over a poisoned mutex, got {other:?}"),
    }
    match dirad::control::dispatch(&state, Request::Sessions).await {
        Response::Sessions { .. } => {}
        other => panic!("expected Sessions over a poisoned mutex, got {other:?}"),
    }
}

/// `Status` must carry today's token totals (the ◇ compute row) and reflect the
/// billing task's cached cloud summary, and both must be absent when there is
/// nothing to show — the skew-safe `None` path the renderers rely on.
#[tokio::test]
async fn status_carries_token_totals_and_cached_billing() {
    use dira_core::sync::{BillingSummary, CachedBillingSummary};
    use dira_core::tokens::TokenTurn;
    use time::format_description::well_known::Rfc3339;

    let (state, _rx) = test_state().await;

    // A fresh daemon: zero tokens today ⇒ Some(zero totals); no billing cache ⇒ None.
    match dirad::control::dispatch(&state, Request::Status).await {
        Response::Status(view) => {
            assert_eq!(view.tokens.map(|t| t.total_tokens), Some(0));
            assert!(view.billing.is_none(), "no billing cache yet");
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Record a token turn "now" (inside today's window) and seed the billing cache
    // the way the billing task does.
    let turn = TokenTurn {
        id: "turn-1".into(),
        at: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
        model: "claude-sonnet-4-5".into(),
        input: 1_000,
        output: 2_000,
        cache_read: 3_000,
        cache_create: 4_000,
    };
    state
        .store
        .upsert_token_usage(&turn, "s1", Some("github.com/acme/api"))
        .await
        .expect("upsert token usage");
    *state.billing.lock().unwrap() = Some(CachedBillingSummary {
        summary: BillingSummary {
            billable_hours: 10.4,
            unbilled_amount: 1064.0,
            currency: "€".into(),
            period: "week".into(),
            ..Default::default()
        },
        fetched_at: "2026-07-02T09:00:00Z".into(),
    });

    match dirad::control::dispatch(&state, Request::Status).await {
        Response::Status(view) => {
            let tokens = view.tokens.expect("token totals present");
            assert_eq!(
                tokens.total_tokens, 10_000,
                "input+output+cache_read+cache_create"
            );
            assert!(tokens.est_cost_usd > 0.0, "priced by the bundled table");
            let billing = view.billing.expect("cached billing attached");
            assert_eq!(billing.currency, "€");
            assert_eq!(billing.unbilled_amount, 1064.0);
            assert_eq!(billing.fetched_at, "2026-07-02T09:00:00Z");
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_info_reports_version_schema_and_pid() {
    let (state, _rx) = test_state().await;
    match dirad::control::dispatch(&state, Request::DaemonInfo).await {
        Response::DaemonInfo {
            version,
            schema_version,
            pid,
            uptime_seconds: _,
        } => {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert_eq!(schema_version, dira_contract::SCHEMA_VERSION);
            assert_eq!(pid, std::process::id());
        }
        other => panic!("expected DaemonInfo, got {other:?}"),
    }
}

/// Frame a request, write it, read the framed response.
async fn rpc(stream: &mut tokio::net::UnixStream, req: &Request) -> Response {
    let bytes = serde_json::to_vec(req).unwrap();
    dirad::control::write_frame(stream, &bytes).await.unwrap();
    let resp = dirad::control::read_frame(stream).await.unwrap();
    serde_json::from_slice(&resp).unwrap()
}

/// (c) Status reactivity: the control socket must answer `Ping` and `Status`
/// before a (deliberately slowed) hydrate completes.
#[tokio::test]
async fn socket_answers_ping_and_status_before_hydrate_completes() {
    let (state, _rx) = test_state().await;

    // Bind the control socket FIRST (as `run()` does), then start a slow hydrate
    // that hasn't flipped `hydrated` yet. Use `$TMPDIR` (sandbox-writable) and a
    // short name to stay under the ~104-char UDS path limit.
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let dir = std::path::Path::new(&tmp).join(format!("d{}.sock", &Ulid::new().to_string()[..10]));
    let _ = std::fs::remove_file(&dir);
    let uds = tokio::net::UnixListener::bind(&dir).expect("bind uds");
    dirad::serve_control(state.clone(), uds);

    // A hydrate that takes a while: `hydrated` stays false meanwhile.
    let hydrate_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        hydrate_state.hydrated.store(true, Ordering::Relaxed);
    });

    // Immediately connect and issue Ping + Status — both must succeed while
    // hydration is still in flight.
    let mut stream = tokio::net::UnixStream::connect(&dir)
        .await
        .expect("connect");
    assert!(
        matches!(rpc(&mut stream, &Request::Ping).await, Response::Pong),
        "Ping must answer immediately"
    );

    // Fresh connection per request (one request/response per connection).
    let mut stream = tokio::net::UnixStream::connect(&dir)
        .await
        .expect("connect");
    match rpc(&mut stream, &Request::Status).await {
        Response::Status(view) => assert!(
            view.hydrating,
            "status during warm-up must report hydrating=true"
        ),
        other => panic!("expected Status before hydrate completes, got {other:?}"),
    }

    // Confirm `hydrated` was still false when we answered (no race on the assert).
    assert!(
        !state.hydrated.load(Ordering::Relaxed),
        "hydrate should still be in flight at this point"
    );

    let _ = std::fs::remove_file(&dir);
}
