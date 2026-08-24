//! Telemetry sync task (WP2): ship the local `telemetry_events` queue
//! (`dira_core::telemetry`, WP1) to `POST {cloud_url}/api/v1/pulse`.
//!
//! A smaller, unsigned sibling of [`crate::sync`]/[`crate::knowledge_sync`]:
//! same debounce/backstop shape, same per-chunk cursor-after-2xx discipline
//! (D-0020), same shared backoff ladder — but no device key, no envelope, no
//! JCS canonicalization. A telemetry batch is anonymous by construction (see
//! `dira_core::telemetry`'s module doc): there is nothing here that needs a
//! device's identity to be trusted, only an install id that never leaves this
//! machine's own queue.
//!
//! ### The gate is deliberately weaker than sync/knowledge sync's
//! Both of those require the device to be **linked** (`identity::device_id`)
//! before they send anything. Telemetry only requires `cloud_url` to be set
//! and `[telemetry] enabled` to be true — an unlinked install (nobody has run
//! `dira device link` yet) still reports anonymous usage, because the whole
//! point of this channel is to see usage from installs that may never link at
//! all. `Store::insert_telemetry_event`'s callers (`ingest`/`enqueue_local`)
//! independently re-check the same knob, so a disabled install never even
//! queues an event; this gate is what stops an already-queued backlog (e.g.
//! from before the knob was turned off) from draining.
//!
//! ### Poison-chunk policy (400 `content_not_allowed`-shaped rejection)
//! The knowledge channel has a rich taxonomy of 4xx error codes because its
//! payload is signed and workspace-gated. Telemetry has none of that: a 400
//! from `/api/v1/pulse` can only mean the batch itself is malformed or the
//! cloud no longer understands `v: 1` — something about THIS chunk's bytes,
//! not a transient condition that a retry would fix. Retrying it forever
//! would wedge the whole queue behind one bad chunk, silently dropping every
//! event minted after it (D-0020's per-chunk cursor exists precisely to avoid
//! one bad window blocking the rest). So a 400 is treated as **permanent-skip**:
//! log it loudly (the body may say why), advance the cursor past the chunk
//! exactly as a 2xx would, and keep draining. The chunk's rows are left in
//! place rather than deleted — cheap local debris, and worth keeping around
//! for `dira doctor`/support to inspect if the malformed-batch rate turns out
//! to matter. This is the same choice the underlying event model already
//! makes safe: `TelemetryEventWire` is a fixed, versioned shape, so "our own
//! batch is malformed" should be rare enough that losing one window of
//! anonymous counts is a fair trade against wedging the queue forever.
//!
//! 413 (payload too large) and 429/5xx (rate limit / server trouble) are
//! ordinary transient failures — retried with the shared backoff ladder, same
//! as sync/knowledge sync. A 404 means the cloud predates the endpoint,
//! mirroring knowledge sync's quiet `endpoint_missing` skip.

use crate::state::AppState;
use crate::sync::{
    next_backoff, record_channel_health, retry_after_from_headers, transient_wait, HealthChannel,
};
use dira_core::protocol::Response;
use dira_core::telemetry::wire::{batch_id, TelemetryBatch, TelemetryEventWire};
use dira_core::telemetry::{identity, META_TELEMETRY_CURSOR, META_TELEMETRY_HEALTH};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration as StdDuration;
use tokio::sync::mpsc;
use tokio::time::{sleep, sleep_until, Instant};
use ulid::Ulid;

/// Debounce window: coalesce a burst of triggers into one flush.
const DEBOUNCE: StdDuration = StdDuration::from_secs(5);
/// Backstop cadence — telemetry moves at command speed, not event speed;
/// slower than both sync (90s) and knowledge sync (120s).
const BACKSTOP: StdDuration = StdDuration::from_secs(300);
/// HTTP timeout for a single pulse chunk POST. Sized like knowledge sync's —
/// a chunk's worth of small, flat JSON events, not a full ingest batch.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(30);
/// Events per chunk POSTed to `/api/v1/pulse`.
const CHUNK_SIZE: i64 = 200;

/// Handle to the telemetry sync task. Cloneable; shares the trigger channel.
///
/// Unlike [`crate::sync::channel`]/[`crate::knowledge_sync::channel`], this
/// does NOT hand its receiver back to the caller to thread through
/// [`crate::build_state`]'s return tuple: that tuple is destructured at every
/// one of `build_state`'s many call sites (production and test), and growing
/// its arity for a single new background task would touch every one of them
/// for a change that is purely internal to this module. Instead the receiver
/// is built alongside the sender and stashed here, `Option`-wrapped so
/// [`spawn`] can `take()` it exactly once; a second `spawn` call (or one
/// before [`channel`] ran) is a wiring bug, logged rather than panicking a
/// running daemon over it.
#[derive(Clone)]
pub struct TelemetrySyncHandle {
    /// Non-blocking trigger; `ingest`/`enqueue_local` `try_send(())` here
    /// after a durable append. A full channel is fine — the backstop covers a
    /// missed nudge.
    pub trigger: mpsc::Sender<()>,
    rx: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
}

/// Create the trigger channel + handle before `AppState` exists (the handle
/// is a field of `AppState`), mirroring [`crate::sync::channel`]'s ordering —
/// see [`TelemetrySyncHandle`]'s doc for why the receiver travels inside the
/// handle instead of alongside it.
pub fn channel() -> TelemetrySyncHandle {
    let (trigger, rx) = mpsc::channel::<()>(1);
    TelemetrySyncHandle {
        trigger,
        rx: Arc::new(Mutex::new(Some(rx))),
    }
}

/// Spawn the background telemetry sync task, taking its receiver out of
/// `state.telemetry_sync` the one time this runs.
pub fn spawn(state: AppState) {
    let rx = state
        .telemetry_sync
        .rx
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    let Some(rx) = rx else {
        tracing::warn!(
            "telemetry sync: spawn called with no receiver available (already spawned, or \
             `channel()` never ran) — the task will not start"
        );
        return;
    };
    tokio::spawn(run(state, rx));
}

async fn run(state: AppState, mut rx: mpsc::Receiver<()>) {
    let mut backstop_at =
        Instant::now() + crate::jitter::jittered(BACKSTOP, crate::jitter::DEFAULT_FRAC);
    let mut backoff = StdDuration::ZERO;
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            recv = rx.recv() => {
                if recv.is_none() {
                    break;
                }
                sleep(DEBOUNCE).await;
                while rx.try_recv().is_ok() {}
            }
            _ = sleep_until(backstop_at) => {
                backstop_at = Instant::now()
                    + crate::jitter::jittered(BACKSTOP, crate::jitter::DEFAULT_FRAC);
            }
        }

        match flush_telemetry(&state).await {
            Ok(Outcome::Synced) | Ok(Outcome::Nothing) => {
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                record_health(&state, None, 0, 0).await;
            }
            Ok(Outcome::Skipped(kind)) => {
                backoff = StdDuration::ZERO;
                consecutive_failures = 0;
                record_health(&state, Some(kind), 0, 0).await;
            }
            Err(err) => {
                let (kind, wait) = match &err {
                    TError::Transient {
                        message,
                        retry_after,
                    } => {
                        let wait = transient_wait(*retry_after, backoff);
                        tracing::warn!(
                            "telemetry sync: transient failure, backing off {wait:?}: {message}"
                        );
                        ("transient", wait)
                    }
                    TError::Fatal(e) => {
                        let wait = next_backoff(backoff);
                        tracing::warn!("telemetry sync: error, backing off {wait:?}: {e}");
                        ("fatal", wait)
                    }
                };
                backoff = wait;
                consecutive_failures += 1;
                record_health(&state, Some(kind), consecutive_failures, backoff.as_secs()).await;
                sleep(backoff).await;
            }
        }
    }
}

#[cfg_attr(test, derive(Debug))]
enum Outcome {
    Synced,
    Nothing,
    /// Not running, with the health kind saying why (`"off"`, `"skipped"`,
    /// `"endpoint_missing"`).
    Skipped(&'static str),
}

#[cfg_attr(test, derive(Debug))]
enum TError {
    Transient {
        message: String,
        retry_after: Option<StdDuration>,
    },
    Fatal(String),
}

/// One telemetry flush. Re-reads the knob + `cloud_url` every call so a
/// config change takes effect without a daemon restart.
///
/// Deliberately does NOT gate on device linkage (see the module doc) — only
/// `cloud_url` + `[telemetry] enabled`.
async fn flush_telemetry(state: &AppState) -> Result<Outcome, TError> {
    if !state.config.telemetry.enabled {
        return Ok(Outcome::Skipped("off"));
    }
    let Some(cloud_url) = state.config.cloud_url.clone() else {
        return Ok(Outcome::Skipped("skipped"));
    };

    let until = state
        .store
        .telemetry_max_event_id()
        .await
        .map_err(|e| TError::Fatal(format!("read max telemetry id: {e}")))?;
    let Some(until) = until else {
        return Ok(Outcome::Nothing); // queue is empty
    };
    let mut cursor = state
        .store
        .meta_get(META_TELEMETRY_CURSOR)
        .await
        .map_err(|e| TError::Fatal(format!("read cursor: {e}")))?
        .filter(|s| !s.is_empty());
    if cursor.as_deref() == Some(until.as_str()) {
        return Ok(Outcome::Nothing); // already caught up to the snapshot bound
    }

    // Minted at most once per flush — install id + salt never change between
    // this flush's chunks, and `load_or_mint` is a store round-trip.
    let mut install_id: Option<String> = None;
    let mut synced_any = false;
    let mut any_poison = false;

    let url = format!("{}/api/v1/pulse", cloud_url.trim_end_matches('/'));
    loop {
        let rows = state
            .store
            .telemetry_events_since(cursor.as_deref(), &until, CHUNK_SIZE)
            .await
            .map_err(|e| TError::Fatal(format!("load telemetry events: {e}")))?;
        if rows.is_empty() {
            break;
        }
        let chunk_len = rows.len();
        let first_id = rows.first().expect("checked non-empty above").id.clone();
        let last_id = rows.last().expect("checked non-empty above").id.clone();

        let events: Vec<TelemetryEventWire> = rows
            .iter()
            .filter_map(|r| match serde_json::from_str(&r.props_json) {
                Ok(w) => Some(w),
                Err(e) => {
                    // A row that fails to deserialize is a local bug (it was
                    // serialized by this same binary in `ingest`/
                    // `enqueue_local`), not a cloud-facing problem — drop it
                    // from the batch rather than failing the whole flush, and
                    // let the cursor advance past it like any other row.
                    tracing::error!(id = %r.id, "telemetry sync: dropping unreadable local row: {e}");
                    None
                }
            })
            .collect();

        if !events.is_empty() {
            if install_id.is_none() {
                let identity = identity::load_or_mint(&state.store)
                    .await
                    .map_err(|e| TError::Fatal(format!("load telemetry identity: {e}")))?;
                install_id = Some(identity.install_id);
            }
            let install_id = install_id.as_deref().expect("just set above");
            let batch = TelemetryBatch {
                v: 1,
                batch_id: batch_id(install_id, &first_id, &last_id),
                install_id: install_id.to_string(),
                generated_at: crate::heartbeat::fmt_rfc3339(time::OffsetDateTime::now_utc()),
                events,
            };

            match post_batch(&state.http, &url, &batch).await? {
                PostOutcome::Accepted => {}
                PostOutcome::EndpointMissing => return Ok(Outcome::Skipped("endpoint_missing")),
                PostOutcome::Poison(body) => {
                    tracing::error!(
                        first_id = %first_id,
                        last_id = %last_id,
                        "telemetry sync: cloud rejected this batch as malformed/unsupported \
                         (400) — skipping past it rather than wedging the queue behind it: {body}"
                    );
                    any_poison = true;
                }
            }
        }

        meta_put(state, META_TELEMETRY_CURSOR, &last_id).await?;
        cursor = Some(last_id);
        synced_any = true;

        if (chunk_len as i64) < CHUNK_SIZE {
            break; // fewer than a full chunk — caught up to `until`
        }
    }

    if synced_any {
        // Retention hygiene, and only after a fully clean drain: a poison
        // chunk's rows are left in place (see the module doc) so a bad batch
        // is inspectable locally even though the cursor has already moved
        // past it.
        if !any_poison {
            if let Some(c) = &cursor {
                if let Err(e) = state.store.delete_telemetry_events_through(c).await {
                    tracing::debug!("telemetry sync: prune after drain failed: {e}");
                }
            }
        }
        Ok(Outcome::Synced)
    } else {
        Ok(Outcome::Nothing)
    }
}

enum PostOutcome {
    Accepted,
    /// 404: older cloud without the endpoint yet.
    EndpointMissing,
    /// 400: this chunk's batch itself is malformed/unsupported — see the
    /// module doc's poison-chunk policy. Carries the response body for the
    /// caller's log line.
    Poison(String),
}

async fn post_batch(
    client: &reqwest::Client,
    url: &str,
    batch: &TelemetryBatch,
) -> Result<PostOutcome, TError> {
    let resp = client
        .post(url)
        .timeout(HTTP_TIMEOUT)
        .json(batch)
        .send()
        .await
        .map_err(|e| TError::Transient {
            message: format!("post: {e}"),
            retry_after: None,
        })?;
    let status = resp.status();
    let retry_after = retry_after_from_headers(resp.headers());

    if status.is_success() {
        return Ok(PostOutcome::Accepted);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        tracing::debug!("telemetry sync: cloud has no /api/v1/pulse yet (404)");
        return Ok(PostOutcome::EndpointMissing);
    }
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::BAD_REQUEST {
        return Ok(PostOutcome::Poison(body));
    }
    if status.as_u16() == 413 || status.as_u16() == 429 || status.is_server_error() {
        return Err(TError::Transient {
            message: format!("cloud answered {status}: {body}"),
            retry_after,
        });
    }
    Err(TError::Fatal(format!("cloud answered {status}: {body}")))
}

async fn meta_put(state: &AppState, key: &str, value: &str) -> Result<(), TError> {
    state
        .store
        .meta_set(key, value)
        .await
        .map_err(|e| TError::Fatal(format!("store {key}: {e}")))
}

/// Enqueue one already-flattened wire event onto the local queue. Dispatched
/// from `Request::IngestTelemetry`, sent by `dira`'s own gated telemetry
/// client.
///
/// Re-checks `[telemetry] enabled` daemon-side (belt and braces) rather than
/// trusting the caller's own gate: a version skew between an older `dira` and
/// a newer `dirad` (or vice versa) must never let an event slip past a
/// `false` knob just because one side's copy of the check is stale. Acks
/// [`Response::Ok`] either way — this is a fire-and-forget ingress, like
/// `IngestZavet`, so a disabled knob is a silent no-op, not an error the CLI
/// needs to see.
pub(crate) async fn ingest(state: &AppState, event: TelemetryEventWire) -> Response {
    if !state.config.telemetry.enabled {
        return Response::Ok;
    }
    if let Err(e) = store_event(state, &event).await {
        tracing::debug!("telemetry: drop event that failed to queue: {e}");
    }
    Response::Ok
}

/// Enqueue one daemon-local lifecycle event (`DaemonStarted`/`DaemonStopped`),
/// stamping the timestamp and the running daemon's own version via
/// [`dira_core::telemetry::event::TelemetryEvent::into_wire`]. Goes through
/// the exact same store-then-trigger path as [`ingest`] — the only difference
/// is that the event never crossed the control socket at all, so it flattens
/// to its wire shape here instead of arriving already flattened.
///
/// Respects the same consent gate as [`ingest`]: a disabled knob is a silent
/// no-op.
pub(crate) async fn enqueue_local(
    state: &AppState,
    event: dira_core::telemetry::event::TelemetryEvent,
) {
    if !state.config.telemetry.enabled {
        return;
    }
    let now = crate::heartbeat::fmt_rfc3339(time::OffsetDateTime::now_utc());
    let wire = event.into_wire(now, env!("CARGO_PKG_VERSION"));
    if let Err(e) = store_event(state, &wire).await {
        tracing::debug!("telemetry: drop local event that failed to queue: {e}");
    }
}

/// Shared tail of [`ingest`]/[`enqueue_local`]: mint an id, append to the
/// local queue, and nudge the sync task. Callers have already applied the
/// consent gate.
async fn store_event(state: &AppState, event: &TelemetryEventWire) -> Result<(), dira_core::Error> {
    let id = Ulid::generate().to_string();
    let created_at = crate::heartbeat::fmt_rfc3339(time::OffsetDateTime::now_utc());
    let props_json = serde_json::to_string(event)
        .map_err(|e| dira_core::Error::Decode(format!("serialize telemetry event: {e}")))?;
    state
        .store
        .insert_telemetry_event(&id, &created_at, &event.event, &props_json)
        .await?;
    let _ = state.telemetry_sync.trigger.try_send(());
    Ok(())
}

/// The telemetry channel's [`HealthChannel`] keys.
const TELEMETRY_HEALTH_CHANNEL: HealthChannel = HealthChannel {
    log: "telemetry sync",
    health_key: META_TELEMETRY_HEALTH,
    cursor_key: META_TELEMETRY_CURSOR,
    watermark_key: None,
};

/// Persist the telemetry channel's health snapshot to
/// [`META_TELEMETRY_HEALTH`] (see [`record_channel_health`]).
async fn record_health(
    state: &AppState,
    error_kind: Option<&str>,
    consecutive_failures: u32,
    backoff_secs: u64,
) {
    record_channel_health(
        state,
        &TELEMETRY_HEALTH_CHANNEL,
        error_kind,
        consecutive_failures,
        backoff_secs,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockCloud, MockResp};
    use dira_core::telemetry::event::TelemetryEvent;

    async fn state_with(cloud: &MockCloud, telemetry_enabled: bool) -> AppState {
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            telemetry: dira_core::config::TelemetryKnobs {
                enabled: telemetry_enabled,
            },
            ..Default::default()
        };
        let (state, ..) = crate::build_state(store, config).await.unwrap();
        state
    }

    fn keys(body: &str) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        v.as_object().unwrap().keys().cloned().collect()
    }

    #[tokio::test]
    async fn happy_path_advances_the_cursor_and_ships_the_documented_shape() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let state = state_with(&cloud, true).await;
        enqueue_local(&state, TelemetryEvent::DaemonStarted).await;

        cloud.push("/api/v1/pulse", MockResp::ok(r#"{"status":"accepted"}"#));
        let out = flush_telemetry(&state).await.unwrap();
        assert!(matches!(out, Outcome::Synced));

        let cursor = state
            .store
            .meta_get(META_TELEMETRY_CURSOR)
            .await
            .unwrap()
            .unwrap();
        assert!(!cursor.is_empty(), "cursor must advance after a 2xx");

        let reqs = cloud.requests("/api/v1/pulse");
        assert_eq!(reqs.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&reqs[0]).unwrap();
        assert_eq!(body["v"], 1);
        assert!(body["installId"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(body["events"][0]["event"], "cli_daemon_started");
        assert_eq!(
            keys(&reqs[0])
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            ["v", "batchId", "installId", "generatedAt", "events"]
                .into_iter()
                .map(String::from)
                .collect()
        );

        // Nothing new — the next flush is a no-op.
        let out = flush_telemetry(&state).await.unwrap();
        assert!(matches!(out, Outcome::Nothing));
        assert_eq!(cloud.requests("/api/v1/pulse").len(), 1);
    }

    #[tokio::test]
    async fn a_500_does_not_advance_the_cursor() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let state = state_with(&cloud, true).await;
        enqueue_local(&state, TelemetryEvent::DaemonStarted).await;

        cloud.push("/api/v1/pulse", MockResp::status(500, "boom"));
        let err = flush_telemetry(&state).await;
        assert!(matches!(err, Err(TError::Transient { .. })));
        assert_eq!(
            state.store.meta_get(META_TELEMETRY_CURSOR).await.unwrap(),
            None,
            "cursor must not advance on a failed POST"
        );
    }

    #[tokio::test]
    async fn disabled_knob_skips_without_touching_the_network() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let state = state_with(&cloud, false).await;
        // ingest/enqueue_local also re-check the knob — nothing should even
        // be queued.
        enqueue_local(&state, TelemetryEvent::DaemonStarted).await;
        assert_eq!(state.store.telemetry_max_event_id().await.unwrap(), None);

        let out = flush_telemetry(&state).await.unwrap();
        assert!(matches!(out, Outcome::Skipped("off")));
        assert!(cloud.requests("/api/v1/pulse").is_empty());
    }

    #[tokio::test]
    async fn no_cloud_url_skips_without_touching_the_network() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let store = dira_core::Store::open_in_memory().await.unwrap();
        let config = dira_core::Config {
            cloud_url: None,
            ..Default::default()
        };
        let (state, ..) = crate::build_state(store, config).await.unwrap();
        enqueue_local(&state, TelemetryEvent::DaemonStarted).await;
        // Queuing does not require cloud_url — only the flush does.
        assert!(state
            .store
            .telemetry_max_event_id()
            .await
            .unwrap()
            .is_some());

        let out = flush_telemetry(&state).await.unwrap();
        assert!(matches!(out, Outcome::Skipped("skipped")));
        assert!(cloud.requests("/api/v1/pulse").is_empty());
    }

    #[tokio::test]
    async fn a_400_advances_the_cursor_past_the_poison_chunk_instead_of_wedging() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let state = state_with(&cloud, true).await;
        enqueue_local(&state, TelemetryEvent::DaemonStarted).await;
        enqueue_local(&state, TelemetryEvent::DaemonStopped { uptime_secs: 1 }).await;

        cloud.push(
            "/api/v1/pulse",
            MockResp::status(400, r#"{"error":"unsupported_schema_version"}"#),
        );
        let out = flush_telemetry(&state).await.unwrap();
        assert!(matches!(out, Outcome::Synced));

        let cursor = state
            .store
            .meta_get(META_TELEMETRY_CURSOR)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !cursor.is_empty(),
            "a 400 must advance the cursor past the poison chunk, not wedge the queue"
        );
        // The poison chunk's rows are kept locally (not pruned) for inspection.
        assert_eq!(
            state
                .store
                .telemetry_events_since(None, &cursor, 100)
                .await
                .unwrap()
                .len(),
            2
        );

        // A follow-up flush with nothing new past the poisoned window is a no-op.
        let out = flush_telemetry(&state).await.unwrap();
        assert!(matches!(out, Outcome::Nothing));
        assert_eq!(cloud.requests("/api/v1/pulse").len(), 1);
    }

    #[tokio::test]
    async fn ingest_with_the_knob_disabled_inserts_nothing() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let state = state_with(&cloud, false).await;
        let wire =
            TelemetryEvent::DaemonStarted.into_wire("2026-01-01T00:00:00Z".into(), "0.0.0-test");
        let resp = ingest(&state, wire).await;
        assert!(matches!(resp, Response::Ok));
        assert_eq!(state.store.telemetry_max_event_id().await.unwrap(), None);
    }

    #[tokio::test]
    async fn ingest_with_the_knob_enabled_queues_and_triggers() {
        let cloud = MockCloud::start(&["/api/v1/pulse"]).await;
        let state = state_with(&cloud, true).await;
        let wire =
            TelemetryEvent::DaemonStarted.into_wire("2026-01-01T00:00:00Z".into(), "0.0.0-test");
        let resp = ingest(&state, wire).await;
        assert!(matches!(resp, Response::Ok));
        assert!(state
            .store
            .telemetry_max_event_id()
            .await
            .unwrap()
            .is_some());
    }
}
