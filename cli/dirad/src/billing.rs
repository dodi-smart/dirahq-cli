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
//!
//! **Deep-idle skip (Task 22):** while the device is deep idle (the same
//! predicate the heartbeat and idle-ticker sweep decimation share, see
//! [`crate::heartbeat::is_deep_idle`]) AND the cache already reflects
//! everything the daemon has synced (the last successful fetch landed at or
//! after the newest known activity), the POST is skipped entirely — the
//! summary cannot have changed, so there is nothing worth waking the cloud's
//! Postgres for. No extra wake plumbing: unlike the heartbeat, this task does
//! not subscribe to `presence_wake` — billing is display-only (no instant
//! resume requirement) and the skip predicate is recomputed fresh off the
//! live registry every tick, so the very next scheduled tick after activity
//! resumes naturally observes `deep_idle == false` and fetches again.
//!
//! `last_success` — this daemon's own last successful fetch — starts `None`
//! every boot and is never seeded from the persisted cache's `fetched_at`.
//! That means the deep-idle skip can never fire on tick one: the first tick
//! of every daemon lifetime always performs one confirming fetch, after
//! which `last_success` is set and deep-idle skipping takes over as usual.
//! This costs at most one extra POST per boot, and avoids a display that
//! silently stays stale across a restart when real activity happened between
//! the last successful fetch and the restart (a cold-started, empty session
//! registry would otherwise read as deep-idle-forever and licensed a skip of
//! that first, confirming fetch).

use crate::heartbeat;
use crate::state::AppState;
use dira_contract::{BillingSummaryEnvelope, BillingSummaryRequest, SCHEMA_VERSION};
use dira_core::sync::{parse_billing_summary_response, CachedBillingSummary, META_BILLING_SUMMARY};
use std::time::{Duration as StdDuration, Instant};
use time::{Duration, OffsetDateTime};

/// Slow refresh cadence — billing moves at human speed.
const REFRESH_INTERVAL: StdDuration = StdDuration::from_secs(15 * 60);

/// How long after a sync flush to refresh — gives the cloud time to unpack the
/// batch into priced intervals before we ask.
const POST_SYNC_DELAY: StdDuration = StdDuration::from_secs(30);

/// Minimum gap between fetch attempts, bounding the rate under notify bursts.
const MIN_FETCH_GAP: StdDuration = StdDuration::from_secs(60);

/// HTTP timeout for a single summary POST.
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// Cap on a cloud-supplied Retry-After wait. This task is unsupervised (no
/// backoff/error classification like sync's `MAX_BACKOFF` ladder), and `secs`
/// comes straight from the cloud's response header/body with no upper bound
/// of its own — an absurd or malicious value fed uncapped into the
/// `Instant::now() + wait` arithmetic below can overflow `Instant`'s internal
/// representation and panic. An hour is already far beyond any legitimate
/// rate-limit window for a background display refresh.
const MAX_RETRY_AFTER_WAIT: StdDuration = StdDuration::from_secs(3600);

/// Deep-idle skip predicate (Task 22): true when firing the POST this tick
/// would only re-confirm what the cache already reflects. Requires BOTH: the
/// device is currently deep idle AND the last successful fetch happened at or
/// after the newest known activity — i.e. nothing has happened on the device
/// since the cache was last refreshed, so the cloud's answer cannot have
/// changed either.
///
/// `last_success` only ever advances on a confirmed 2xx (never on a bare
/// attempt), so a fetch that FAILED before idle set in leaves it pinned at
/// its previous, older value. Whenever activity happened after that stale
/// success, `last_success < newest_activity` and the skip does not fire — the
/// fetch keeps retrying every tick (deep idle or not) until it actually
/// succeeds. This is what makes the skip safe against a failed-then-idle
/// sequence.
fn should_skip_deep_idle_fetch(
    deep_idle: bool,
    last_success: Option<OffsetDateTime>,
    newest_activity: OffsetDateTime,
) -> bool {
    deep_idle && last_success.is_some_and(|success| success >= newest_activity)
}

/// Spawn the background billing-summary task. No-ops each tick until the device
/// is both linked (`device_id`) and configured (`cloud_url`); never panics,
/// never blocks startup.
pub fn spawn(state: AppState) {
    tokio::spawn(run(state));
}

async fn run(state: AppState) {
    // Hydrate the in-memory cache from the persisted copy so `dira status`
    // shows the last-known value immediately after a daemon bounce, even
    // offline. Errors are non-fatal — worst case the footer is absent until
    // the first successful fetch.
    //
    // `last_success` deliberately does NOT seed from the persisted cache's
    // `fetched_at`. It starts `None` every boot, so the very first tick's
    // `should_skip_deep_idle_fetch` check always fails (no `last_success` to
    // license a skip) and `fetch_once` performs one confirming fetch — this
    // daemon's actual last successful fetch. Seeding from the cache used to
    // combine with the tick-1 cold-start registry (empty until `hydrate()`
    // populates it, so `newest_activity` falls back to `UNIX_EPOCH` and
    // `deep_idle` is trivially true) to skip that first fetch on every
    // restart with a cached summary — even when real activity happened
    // between the last successful fetch and the restart, leaving the display
    // stale until brand-new activity arrived. Starting `None` costs at most
    // one extra POST per daemon lifetime; deep-idle skipping takes over from
    // the second tick onward exactly as before.
    let mut last_success: Option<OffsetDateTime> = None;
    if let Ok(Some(json)) = state.store.meta_get(META_BILLING_SUMMARY).await {
        if let Ok(cached) = serde_json::from_str::<CachedBillingSummary>(&json) {
            if let Ok(mut slot) = state.billing.lock() {
                *slot = Some(cached);
            }
        }
    }

    let mut last_attempt: Option<Instant> = None;
    loop {
        fetch_once(&state, &mut last_attempt, &mut last_success).await;
        tokio::select! {
            // A sync flush just landed facts on the cloud — refresh soon after,
            // so the footer tracks the numbers the flush moved. `Notify` collapses
            // bursts; MIN_FETCH_GAP bounds the rate regardless.
            _ = state.billing_refresh.notified() => {
                tokio::time::sleep(POST_SYNC_DELAY).await;
            }
            // Jittered ±10% so many daemons don't all refresh billing on the same
            // wall-clock cadence.
            _ = tokio::time::sleep(crate::jitter::jittered(REFRESH_INTERVAL, crate::jitter::DEFAULT_FRAC)) => {}
        }
    }
}

/// One fetch attempt. Re-reads config + linkage each call so `dira device link`
/// / a config change takes effect without a daemon restart. Holds no lock
/// across the HTTP await.
async fn fetch_once(
    state: &AppState,
    last_attempt: &mut Option<Instant>,
    last_success: &mut Option<OffsetDateTime>,
) {
    if let Some(t) = *last_attempt {
        if t.elapsed() < MIN_FETCH_GAP {
            return; // notify burst — the periodic tick will cover it
        }
    }

    // Deep-idle skip (Task 22), checked before touching `cloud_link`/signing so
    // a quiet device doesn't pay even the local device_id read for a tick
    // that's going nowhere. `last_activity` mirrors the heartbeat/idle-ticker's
    // `None` fallback: a registry that has never observed a single event
    // counts as idle-forever (`Duration::MAX`), not active — and, symmetrically,
    // "no activity ever" trivially satisfies "the cache already reflects it"
    // via the `UNIX_EPOCH` floor below, so a prior success can still license a
    // skip. Never touches `last_attempt` — a skip is not an attempt, so it
    // can't interfere with the `MIN_FETCH_GAP` burst gate above.
    let idle_check_at = OffsetDateTime::now_utc();
    let (active_count, last_activity) = {
        let reg = crate::control::lock_recover(&state.sessions);
        (
            reg.active(idle_check_at, state.config.session_stale_after())
                .len(),
            reg.last_activity_at(),
        )
    };
    let idle_for = last_activity
        .map(|at| (idle_check_at - at).max(Duration::ZERO))
        .unwrap_or(Duration::MAX);
    let deep_idle = heartbeat::is_deep_idle(active_count, idle_for, state.config.deep_idle_after());
    let newest_activity = last_activity.unwrap_or(OffsetDateTime::UNIX_EPOCH);
    if should_skip_deep_idle_fetch(deep_idle, *last_success, newest_activity) {
        tracing::debug!(
            "billing: deep idle and cache already reflects last activity — skipping fetch"
        );
        return;
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
    match state
        .http
        .post(&url)
        .timeout(HTTP_TIMEOUT)
        .json(&envelope)
        .send()
        .await
    {
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
            // Task 22: only a confirmed 2xx advances `last_success` — this is
            // what makes the deep-idle skip predicate safe against a fetch
            // that failed before idle set in (see `should_skip_deep_idle_fetch`).
            *last_success = Some(now);
            tracing::debug!("billing: refreshed summary from cloud");
        }
        Ok(resp) if resp.status().as_u16() == 429 => {
            // WP-B6: push the next fetch out to respect the cloud's Retry-After
            // (header first, typed body as fallback) — minimal, matching this
            // file's existing log-and-return style for a non-2xx response. The
            // fetch-gap check (`t.elapsed() < MIN_FETCH_GAP` above) reads
            // `*last_attempt`, so shifting it into the future by `wait -
            // MIN_FETCH_GAP` makes the gate hold for the full `wait`, not just
            // the usual minimum gap. `Instant` subtraction/`elapsed()` saturates
            // at zero rather than panicking (stable since Rust 1.60), so this is
            // safe even though `last_attempt` now points past `Instant::now()`.
            let header_hint = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(dira_core::sync::parse_retry_after_secs);
            let body = resp.text().await.unwrap_or_default();
            let hint = header_hint.or_else(|| dira_core::sync::parse_retry_after_body(&body));
            if let Some(secs) = hint {
                // Capped at MAX_RETRY_AFTER_WAIT BEFORE it ever reaches
                // `Instant::now() + wait` below — `secs` is attacker/cloud
                // controlled and unbounded, and `Instant` addition panics on
                // overflow (this task has no supervisor to catch it).
                let wait = StdDuration::from_secs(secs)
                    .min(MAX_RETRY_AFTER_WAIT)
                    .max(MIN_FETCH_GAP);
                *last_attempt = Some(Instant::now() + wait - MIN_FETCH_GAP);
            }
            tracing::debug!("billing: summary 429 (rate limited), retry_after={hint:?}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{keychain_lock, use_mock_keychain, MockCloud, MockResp};

    /// A linked `AppState` **and** the keychain lock guard that must outlive it.
    ///
    /// Every `fetch_once` against this state signs its request, which resolves
    /// the device key through `dira_core::identity` — keychain-first. Without
    /// the mock store installed that reaches the real OS keychain and blocks on
    /// an authorization prompt no CI runner can answer. Returning the guard
    /// rather than dropping it here makes the isolation impossible to forget:
    /// you cannot get a linked state without also holding the lock for as long
    /// as you use it. Bind it (`let (state, _keychain) = ...`), never `_`.
    async fn linked_state(cloud: &MockCloud) -> (AppState, tokio::sync::MutexGuard<'static, ()>) {
        let keychain = keychain_lock().await;
        use_mock_keychain();
        let store = dira_core::Store::open_in_memory().await.unwrap();
        dira_core::identity::set_device_id(&store, "01TESTDEVICE")
            .await
            .unwrap();
        let config = dira_core::Config {
            cloud_url: Some(cloud.base_url().to_string()),
            ..Default::default()
        };
        let (state, _rx, _sync_rx, _knowledge_rx) =
            crate::build_state(store, config).await.unwrap();
        (state, keychain)
    }

    /// Task 22 (integration): a freshly built state has an empty session
    /// registry (no event ever observed) — deep idle from tick one under the
    /// `None` last-activity fallback — and a `last_success` already licenses
    /// the skip, so `fetch_once` must not POST at all, and must leave
    /// `last_attempt` untouched (a skip is not an attempt).
    #[tokio::test]
    async fn fetch_once_skips_the_post_when_deep_idle_and_cache_already_current() {
        let cloud = MockCloud::start(&["/api/v1/billing/summary"]).await;
        let (state, _keychain) = linked_state(&cloud).await;

        let mut last_attempt: Option<Instant> = None;
        let mut last_success = Some(OffsetDateTime::now_utc());
        fetch_once(&state, &mut last_attempt, &mut last_success).await;

        assert!(
            cloud.requests("/api/v1/billing/summary").is_empty(),
            "deep idle + a cache already reflecting the (nonexistent) activity must skip the POST"
        );
        assert!(
            last_attempt.is_none(),
            "a skip must not count as an attempt for the MIN_FETCH_GAP gate"
        );
    }

    /// Regression test for the restart-skips-the-confirming-fetch bug: `run`
    /// used to seed `last_success` straight from the persisted cache's
    /// `fetched_at`. Combined with a cold-started (tick-1, pre-`hydrate`)
    /// empty session registry — `newest_activity` falls back to
    /// `UNIX_EPOCH`, so `deep_idle` is trivially true — that seeding made
    /// EVERY restart with a cached summary skip its first fetch outright,
    /// even when real activity happened between the last successful fetch
    /// and the restart. `last_success` must instead start `None` each boot,
    /// so `run`'s very first tick always performs one confirming fetch
    /// regardless of how fresh the persisted cache looks.
    #[tokio::test]
    async fn run_always_fetches_on_tick_one_after_a_restart_with_a_cached_summary() {
        let cloud = MockCloud::start(&["/api/v1/billing/summary"]).await;
        cloud.push("/api/v1/billing/summary", MockResp::ok("{}"));
        let (state, _keychain) = linked_state(&cloud).await;

        // Persist a cache as if this were a prior daemon lifetime's last
        // successful fetch, timestamped now — as fresh as a cache can look,
        // which is exactly the case the old seeding treated as "nothing to
        // confirm".
        let cached = CachedBillingSummary {
            summary: Default::default(),
            fetched_at: crate::heartbeat::fmt_rfc3339(OffsetDateTime::now_utc()),
        };
        state
            .store
            .meta_set(
                META_BILLING_SUMMARY,
                &serde_json::to_string(&cached).unwrap(),
            )
            .await
            .unwrap();

        let started = Instant::now();
        let handle = tokio::spawn(run(state));

        let deadline = StdDuration::from_secs(5);
        loop {
            if !cloud.requests("/api/v1/billing/summary").is_empty() {
                break;
            }
            assert!(
                started.elapsed() < deadline,
                "a restart with a cached summary must still fire one confirming \
                 fetch on tick one, not skip it as deep-idle-with-a-current-cache"
            );
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }

        assert_eq!(
            cloud.requests("/api/v1/billing/summary").len(),
            1,
            "tick one must fetch exactly once"
        );
        handle.abort();
    }

    /// Task 22 (integration): recent, un-ended activity keeps the registry out
    /// of deep idle, so `fetch_once` must still POST even when `last_success`
    /// is set far enough in the future that the pure predicate alone would
    /// license a skip — proving the wiring re-derives `deep_idle` fresh every
    /// tick instead of trusting a stale flag.
    #[tokio::test]
    async fn fetch_once_does_not_skip_when_not_deep_idle() {
        let cloud = MockCloud::start(&["/api/v1/billing/summary"]).await;
        cloud.push("/api/v1/billing/summary", MockResp::ok("{}"));
        let (state, _keychain) = linked_state(&cloud).await;

        let ev = dira_core::model::RawEvent {
            id: "s1-start".to_string(),
            at: OffsetDateTime::now_utc(),
            session_id: "s1".to_string(),
            harness: dira_contract::Harness::ClaudeCode,
            kind: dira_core::model::EventKind::SessionStart,
            cwd: None,
            project: None,
            identity_email: None,
            branch: None,
            tool: None,
            label: None,
            activity: None,
            note: None,
        };
        state
            .sessions
            .lock()
            .unwrap()
            .observe(&ev, state.config.idle());

        let mut last_attempt: Option<Instant> = None;
        let mut last_success = Some(OffsetDateTime::now_utc() + Duration::days(1));
        fetch_once(&state, &mut last_attempt, &mut last_success).await;

        assert_eq!(
            cloud.requests("/api/v1/billing/summary").len(),
            1,
            "an active/recently-active device must still fetch, licensing last_success or not"
        );
    }

    /// WP-B6: a 429 must push `last_attempt` far enough into the future that the
    /// gate (`t.elapsed() < MIN_FETCH_GAP`) holds for the FULL Retry-After, not
    /// just the ordinary `MIN_FETCH_GAP` — proving the "next fetch respects
    /// Retry-After" contract without waiting out real time in the test.
    #[tokio::test]
    async fn fetch_429_pushes_last_attempt_out_by_retry_after() {
        let cloud = MockCloud::start(&["/api/v1/billing/summary"]).await;
        cloud.push(
            "/api/v1/billing/summary",
            MockResp::status(429, r#"{"error":"rate_limited","retryAfterSecs":120}"#)
                .with_header("Retry-After", "120"),
        );
        let (state, _keychain) = linked_state(&cloud).await;

        let before = Instant::now();
        let mut last_attempt: Option<Instant> = None;
        let mut last_success: Option<OffsetDateTime> = None;
        fetch_once(&state, &mut last_attempt, &mut last_success).await;

        let t = last_attempt.expect("fetch_once must record an attempt");
        // Immediately after the 429, the gate must still hold (elapsed() is
        // saturating, so a future `t` reads as ~0 elapsed) — a fetch right now
        // would be suppressed.
        assert!(t.elapsed() < MIN_FETCH_GAP);
        // `last_attempt` was pushed out by `retryAfterSecs (120) - MIN_FETCH_GAP
        // (60) = 60s` beyond `before` — i.e. the gate now holds for the FULL
        // 120s Retry-After, not just the ordinary 60s MIN_FETCH_GAP. Generous
        // tolerance for the (sub-millisecond, purely local) time between
        // `before` and the 429 landing.
        let delta = t.saturating_duration_since(before);
        assert!(
            delta >= StdDuration::from_secs(55) && delta <= StdDuration::from_secs(65),
            "expected last_attempt ~60s past `before`, got {delta:?}"
        );
    }

    /// Regression test for the panic fix: `Instant::now() + wait` overflows
    /// (panics) if `wait` is built straight from an uncapped cloud-supplied
    /// Retry-After. An absurd hint (`u64::MAX` seconds) must not panic this
    /// unsupervised background task — it must be capped at
    /// `MAX_RETRY_AFTER_WAIT` well before reaching the `Instant` arithmetic.
    #[tokio::test]
    async fn fetch_429_with_an_absurd_retry_after_is_capped_and_does_not_panic() {
        let cloud = MockCloud::start(&["/api/v1/billing/summary"]).await;
        cloud.push(
            "/api/v1/billing/summary",
            MockResp::status(429, "").with_header("Retry-After", &u64::MAX.to_string()),
        );
        let (state, _keychain) = linked_state(&cloud).await;

        let before = Instant::now();
        let mut last_attempt: Option<Instant> = None;
        let mut last_success: Option<OffsetDateTime> = None;
        // The absence of a panic IS the primary assertion here.
        fetch_once(&state, &mut last_attempt, &mut last_success).await;

        let t = last_attempt.expect("fetch_once must record an attempt");
        let delta = t.saturating_duration_since(before);
        assert!(
            delta <= MAX_RETRY_AFTER_WAIT,
            "expected the wait capped at MAX_RETRY_AFTER_WAIT, got {delta:?}"
        );
    }

    /// A 429 without any retry hint (no header, no typed body) is a no-op on
    /// pacing beyond the ordinary `MIN_FETCH_GAP` already set before the POST —
    /// this file's existing "log and keep the cache" style.
    #[tokio::test]
    async fn fetch_429_without_hint_leaves_the_ordinary_gap() {
        let cloud = MockCloud::start(&["/api/v1/billing/summary"]).await;
        cloud.push("/api/v1/billing/summary", MockResp::status(429, ""));
        let (state, _keychain) = linked_state(&cloud).await;

        let mut last_attempt: Option<Instant> = None;
        let mut last_success: Option<OffsetDateTime> = None;
        let before = Instant::now();
        fetch_once(&state, &mut last_attempt, &mut last_success).await;
        let t = last_attempt.expect("fetch_once must record an attempt");

        assert!(t >= before && t <= Instant::now());
    }

    /// Task 22 — deep-idle skip predicate matrix: `deep_idle` alone is not
    /// enough; the cache must already reflect the newest activity.
    #[test]
    fn skip_predicate_requires_both_deep_idle_and_a_success_at_or_after_activity() {
        let activity = OffsetDateTime::UNIX_EPOCH + Duration::seconds(1_000);
        let before_activity = activity - Duration::seconds(1);
        let at_activity = activity;
        let after_activity = activity + Duration::seconds(1);

        // Not deep idle: never skip, no matter how the timestamps line up.
        assert!(!should_skip_deep_idle_fetch(
            false,
            Some(after_activity),
            activity
        ));
        assert!(!should_skip_deep_idle_fetch(false, None, activity));

        // Deep idle but never successfully fetched (brand new / never-linked
        // cache): must not skip — there is nothing yet to skip in favor of.
        assert!(!should_skip_deep_idle_fetch(true, None, activity));

        // Deep idle, but the last success predates the newest activity — the
        // failed-before-idle-set-in retry case from the brief: a fetch that
        // failed (so `last_success` never advanced past its previous, older
        // value) while activity kept happening must still be retried.
        assert!(!should_skip_deep_idle_fetch(
            true,
            Some(before_activity),
            activity
        ));

        // Deep idle, last success exactly at the newest activity (boundary,
        // inclusive) — the cache already reflects everything: skip.
        assert!(should_skip_deep_idle_fetch(
            true,
            Some(at_activity),
            activity
        ));

        // Deep idle, last success strictly after the newest activity: skip.
        assert!(should_skip_deep_idle_fetch(
            true,
            Some(after_activity),
            activity
        ));
    }
}
