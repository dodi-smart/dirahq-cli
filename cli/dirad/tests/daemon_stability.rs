//! Integration tests for Commit 1 (stability) + Commit 2 (status reactivity).
//!
//! These stand up real daemon state (an in-memory store, the live registry, the
//! writer loop, a bound control listener) so they exercise the same code the
//! binary runs. The transport tests go through `dira_ipc`, so on unix they
//! exercise the UDS path and on windows CI the named-pipe path — same tests,
//! both platforms.

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
    let (state, rx, _sync_rx, _knowledge_rx) = dirad::build_state(store, config)
        .await
        .expect("build_state");
    (state, rx)
}

/// A commit-bearing manual event carrying a cwd + project, so the writer's
/// capture trigger fires (manual ticks built by `events::manual_event` have no
/// cwd and wouldn't). The timestamp is explicit so accrual is deterministic.
fn manual_tick_with_repo(session: &str, at: OffsetDateTime) -> RawEvent {
    RawEvent {
        id: Ulid::generate().to_string(),
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
            .all()
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

/// (a2) Panic isolation (WP-B7): a message whose processing panics must be
/// caught, dropped, and counted — the receiver and the loop must survive and
/// keep accruing subsequent messages.
///
/// The panic is injected via the same `capture_fn` seam as the slow-git test
/// above. It fires on the *first* commit-bearing event (`ManualStart`, which
/// carries a repo) and, because of the per-repo capture throttle, never again
/// for the same project within the test's real-time window — so exactly one
/// panic is expected. Because the writer only calls `capture_fn` *after* the
/// triggering event is durably appended and folded into the registry (see the
/// accounting-ordering invariant documented on `writer::process_message`),
/// this also proves that a panic in the best-effort tail of processing can't
/// unwind the accounting-critical section that already ran for that event.
#[tokio::test]
async fn panicking_message_is_caught_and_writer_keeps_accruing() {
    let (state, rx) = test_state().await;

    fn panicking_capture(_state: &AppState, _cwd: &str, _canonical: &str) {
        panic!("simulated panic in commit capture");
    }

    let writer_state = state.clone();
    let writer = tokio::spawn(async move {
        dirad::writer::writer_with(rx, writer_state, panicking_capture).await;
    });

    let base = OffsetDateTime::now_utc() - Duration::minutes(10);
    // Triggers `panicking_capture` — its processing panics AFTER the store
    // append and registry `observe()` for this event already ran.
    state
        .tx
        .send(EventMsg::Raw(Box::new(manual_start_with_repo(
            "panic-1", base,
        ))))
        .await
        .unwrap();

    // Subsequent, unrelated messages must still be stored and accrue normally —
    // proof the writer loop didn't wedge or die.
    for i in 1..=3 {
        let at = base + Duration::seconds(30 * i);
        state
            .tx
            .send(EventMsg::Raw(Box::new(manual_tick_with_repo(
                "panic-1", at,
            ))))
            .await
            .unwrap();
    }

    // Poll until the panic is recorded and the subsequent ticks have accrued.
    let mut panics = 0u64;
    let mut active = 0u64;
    for _ in 0..200 {
        panics = state.progress.writer_panics();
        active = dirad::control::lock_recover(&state.sessions)
            .all()
            .into_iter()
            .find(|s| s.session_id == "panic-1")
            .map(|s| s.active_seconds)
            .unwrap_or(0);
        if panics >= 1 && active >= 90 {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }

    assert_eq!(panics, 1, "exactly one message's capture panicked");
    assert_eq!(
        active, 90,
        "subsequent ManualTicks must keep accruing active_seconds after a caught panic"
    );

    // The store is durable across the panic: every appended event (including
    // the one whose *capture* panicked) is present — the panic dropped nothing
    // that had already been stored, and lost nothing downstream either.
    let stored = state.store.events_since(None).await.expect("events_since");
    assert_eq!(
        stored.iter().filter(|e| e.session_id == "panic-1").count(),
        4,
        "ManualStart + 3 ManualTicks all durably stored despite the capture panic"
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
            db_path,
            control_channel_warning: _,
            uptime_seconds: _,
            http_ingress_error: _,
            storage_warning: _,
        } => {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert_eq!(schema_version, dira_contract::SCHEMA_VERSION);
            assert_eq!(pid, std::process::id());
            // The store the daemon actually opened. `dira` compares this against
            // its OWN resolution to catch the elevated/service-account case,
            // which neither side can detect alone — so it must never be absent
            // from a daemon new enough to know about it.
            assert_eq!(
                db_path.as_deref(),
                Some(state.config.db_path.display().to_string().as_str()),
                "DaemonInfo must report the store it opened"
            );
        }
        other => panic!("expected DaemonInfo, got {other:?}"),
    }
}

/// Frame a request, write it, read the framed response — over whichever
/// transport `dira_ipc` picked for this platform.
async fn rpc(stream: &mut dira_ipc::Stream, req: &Request) -> Response {
    let bytes = serde_json::to_vec(req).unwrap();
    dira_ipc::write_frame(stream, &bytes).await.unwrap();
    let resp = dira_ipc::read_frame(stream).await.unwrap();
    serde_json::from_slice(&resp).unwrap()
}

/// A per-test control endpoint: a short UDS path under `$TMPDIR` on unix
/// (kept under the ~104-char socket-path limit, sandbox-writable), a
/// uniquely-named pipe on windows (the pipe namespace is flat and global, so
/// uniqueness comes from the name, and nothing is created on disk).
fn test_endpoint(tag: &str) -> std::path::PathBuf {
    let unique = &Ulid::generate().to_string()[..10];
    #[cfg(unix)]
    {
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
        std::path::Path::new(&tmp).join(format!("{tag}{unique}.sock"))
    }
    #[cfg(windows)]
    {
        std::path::PathBuf::from(format!(r"\\.\pipe\dirad-test-{tag}-{unique}"))
    }
}

/// (c) Status reactivity: the control channel must answer `Ping` and `Status`
/// before a (deliberately slowed) hydrate completes.
#[tokio::test]
async fn socket_answers_ping_and_status_before_hydrate_completes() {
    let (state, _rx) = test_state().await;

    // Bind the control listener FIRST (as `run()` does), then start a slow
    // hydrate that hasn't flipped `hydrated` yet.
    let dir = test_endpoint("d");
    let listener = dira_ipc::Listener::bind(&dir).await.expect("bind control");
    dirad::serve_control(state.clone(), listener);

    // A hydrate that takes a while: `hydrated` stays false meanwhile.
    let hydrate_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(StdDuration::from_millis(500)).await;
        hydrate_state.hydrated.store(true, Ordering::Relaxed);
    });

    // Immediately connect and issue Ping + Status — both must succeed while
    // hydration is still in flight.
    let mut stream = dira_ipc::connect(&dir).await.expect("connect");
    assert!(
        matches!(rpc(&mut stream, &Request::Ping).await, Response::Pong),
        "Ping must answer immediately"
    );

    // Fresh connection per request (one request/response per connection).
    let mut stream = dira_ipc::connect(&dir).await.expect("connect");
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

    #[cfg(unix)]
    let _ = std::fs::remove_file(&dir);
}

/// The in-band orderly-shutdown path: `Request::Shutdown` over the control
/// channel must (1) answer `Ok` — written BEFORE the daemon starts tearing
/// down, so the CLI never loses the acknowledgment — and (2) wake whatever is
/// parked on `state.shutdown` (in the real binary, `wait_for_shutdown_signal`
/// inside `run()`). On windows this is the ONLY orderly-shutdown trigger
/// (there is no SIGTERM), which is why this test is deliberately
/// cross-platform where `sigterm_shutdown.rs` is `#![cfg(unix)]`.
///
/// Also proves the no-parked-waiter race is closed: the `notified()` future is
/// created only AFTER the response arrives — `notify_one`'s stored permit is
/// what makes that ordering safe (see the `AppState::shutdown` field doc).
#[tokio::test]
async fn shutdown_request_triggers_orderly_shutdown() {
    let (state, _rx) = test_state().await;

    let dir = test_endpoint("dstop");
    let listener = dira_ipc::Listener::bind(&dir).await.expect("bind control");
    dirad::serve_control(state.clone(), listener);

    let mut stream = dira_ipc::connect(&dir).await.expect("connect");
    match rpc(&mut stream, &Request::Shutdown).await {
        Response::Ok => {}
        other => panic!("expected Ok for Shutdown, got {other:?}"),
    }

    tokio::time::timeout(StdDuration::from_secs(2), state.shutdown.notified())
        .await
        .expect("Shutdown request must wake the daemon's shutdown waiter");

    #[cfg(unix)]
    let _ = std::fs::remove_file(&dir);
}
