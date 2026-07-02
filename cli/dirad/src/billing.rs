//! Cloud billing-summary fetch.
//!
//! A single background task periodically POSTs a signed
//! [`dira_contract::BillingSummaryEnvelope`] to `{cloud_url}/api/v1/billing/summary`
//! and caches the cloud's billable rollup for `dira status` / `dira watch`.
//! It mirrors the heartbeat's shape (one `reqwest::Client`, per-tick gating on
//! `cloud_url` + `device_id`, sign-the-payload, POST the envelope) and is just
//! as forgiving: billing display is best-effort, so a failed fetch keeps the
//! previous cache (stale beats absent) and the next tick retries.
//!
//! Cadence: a slow periodic refresh, plus an early refresh shortly after the
//! sync task flushes a batch (the flush is what moves the cloud's numbers), and
//! a minimum gap between fetches so notify bursts can't hot-loop the endpoint.
//! The cache is persisted under [`dira_core::sync::META_BILLING_SUMMARY`] so a
//! restarted or offline daemon still serves the last-known value.

use crate::state::AppState;
use dira_contract::{BillingSummaryEnvelope, BillingSummaryRequest, SCHEMA_VERSION};
use dira_core::sync::{parse_billing_summary_response, CachedBillingSummary, META_BILLING_SUMMARY};
use std::time::{Duration as StdDuration, Instant};
use time::OffsetDateTime;

/// Slow refresh cadence — billing moves at human speed.
const REFRESH_INTERVAL: StdDuration = StdDuration::from_secs(15 * 60);

/// How long after a sync flush to refresh — gives the cloud time to unpack the
/// batch into priced intervals before we ask.
const POST_SYNC_DELAY: StdDuration = StdDuration::from_secs(30);

/// Minimum gap between fetch attempts, bounding the rate under notify bursts.
const MIN_FETCH_GAP: StdDuration = StdDuration::from_secs(60);

/// HTTP timeout for a single summary POST.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// Spawn the background billing-summary task. No-ops each tick until the device
/// is both linked (`device_id`) and configured (`cloud_url`); never panics,
/// never blocks startup.
pub fn spawn(state: AppState) {
    tokio::spawn(run(state));
}

async fn run(state: AppState) {
    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("billing: failed to build http client, summary disabled: {e}");
            return;
        }
    };

    // Hydrate the in-memory cache from the persisted copy so `dira status`
    // shows the last-known value immediately after a daemon bounce, even
    // offline. Errors are non-fatal — worst case the footer is absent until
    // the first successful fetch.
    if let Ok(Some(json)) = state.store.meta_get(META_BILLING_SUMMARY).await {
        if let Ok(cached) = serde_json::from_str::<CachedBillingSummary>(&json) {
            if let Ok(mut slot) = state.billing.lock() {
                *slot = Some(cached);
            }
        }
    }

    let mut last_attempt: Option<Instant> = None;
    loop {
        fetch_once(&state, &client, &mut last_attempt).await;
        tokio::select! {
            // A sync flush just landed facts on the cloud — refresh soon after,
            // so the footer tracks the numbers the flush moved. `Notify` collapses
            // bursts; MIN_FETCH_GAP bounds the rate regardless.
            _ = state.billing_refresh.notified() => {
                tokio::time::sleep(POST_SYNC_DELAY).await;
            }
            _ = tokio::time::sleep(REFRESH_INTERVAL) => {}
        }
    }
}

/// One fetch attempt. Re-reads config + linkage each call so `dira device link`
/// / a config change takes effect without a daemon restart. Holds no lock
/// across the HTTP await.
async fn fetch_once(
    state: &AppState,
    client: &reqwest::Client,
    last_attempt: &mut Option<Instant>,
) {
    if let Some(t) = *last_attempt {
        if t.elapsed() < MIN_FETCH_GAP {
            return; // notify burst — the periodic tick will cover it
        }
    }
    let Some((cloud_url, device_id)) = state.cloud_link("billing").await else {
        return; // off until a cloud_url is configured and the device is linked
    };
    *last_attempt = Some(Instant::now());

    let now = OffsetDateTime::now_utc();
    let request = BillingSummaryRequest {
        device_id: device_id.clone(),
        sent_at: crate::heartbeat::fmt_rfc3339(now),
        period: "week".to_string(),
    };
    let Some(device_key) = state.device_key().await else {
        tracing::warn!("billing: signing key unavailable");
        return;
    };
    let sig = match device_key.sign_payload(&request) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("billing: sign failed: {e}");
            return;
        }
    };
    let envelope = BillingSummaryEnvelope {
        schema_version: SCHEMA_VERSION.to_string(),
        device_id,
        payload: request,
        sig,
    };

    let url = format!("{}/api/v1/billing/summary", cloud_url.trim_end_matches('/'));
    match client.post(&url).json(&envelope).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            let Some(summary) = parse_billing_summary_response(&body) else {
                tracing::debug!("billing: 2xx body without a summary — keeping cache");
                return;
            };
            let cached = CachedBillingSummary {
                summary,
                fetched_at: crate::heartbeat::fmt_rfc3339(now),
            };
            if let Ok(json) = serde_json::to_string(&cached) {
                if let Err(e) = state.store.meta_set(META_BILLING_SUMMARY, &json).await {
                    tracing::debug!("billing: persist cache failed: {e}");
                }
            }
            if let Ok(mut slot) = state.billing.lock() {
                *slot = Some(cached);
            }
            tracing::debug!("billing: refreshed summary from cloud");
        }
        Ok(resp) => {
            // A 404 is simply an older cloud without the endpoint; anything else
            // is logged and the cache (possibly stale) keeps serving status.
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::debug!("billing: summary {status}: {body}");
        }
        Err(e) => {
            tracing::debug!("billing: post summary failed: {e}");
        }
    }
}
